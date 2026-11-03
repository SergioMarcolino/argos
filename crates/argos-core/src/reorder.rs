//! A receive-side reorder buffer for RTP video packets.
//!
//! The raw depacketizer is strictly in-order: one lost packet clears the
//! partial frame, and a retransmitted packet arriving late clears the *next*
//! frame too — the freeze users see on lossy links. This assembler instead
//! stores packets by sequence number, waits out a bounded retransmit window
//! (default 150 ms), and only breaks the stream when the deadline passes.
//! Complete frames are emitted immediately, so a clean link adds zero delay.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rtc::rtp::packet::Packet;

use crate::h264::nalu_type;

const FU_A: u8 = 28;
/// NALU type of an IDR slice — the sync gate mirrors `Depacketizer`.
const IDR: u8 = 5;
/// Hard cap on held packets: ~1 s of a 30 fps stream's worst case.
const MAX_PACKETS: usize = 2048;

/// What the assembler produces, in receive order.
#[derive(Debug, PartialEq)]
pub enum Output {
    /// A complete access unit as a list of NALUs in sequence order.
    Frame(Vec<Vec<u8>>),
    /// A frame (and its dependents) could not be completed in time — the
    /// caller should request a keyframe and wait for an IDR.
    Broken,
}

/// Counts what the buffer actually did — the receiver's evidence that loss is
/// being survived rather than hidden.
#[derive(Default, Debug, Clone, Copy)]
pub struct ReorderStats {
    /// Frames that completed only because a gap was later filled.
    pub recovered: u64,
    /// Frames dropped after the deadline.
    pub dropped: u64,
}

struct Held {
    packet: Packet,
}

/// Packets of one RTP timestamp group — one video frame. Keys are the
/// wrap-safe offset from `base` (the earliest seq under wrapping order), so a
/// frame straddling 65535 -> 0 orders correctly.
struct Group {
    packets: BTreeMap<u32, Held>,
    base: u16,
    timestamp: u32,
    arrived: Instant,
    /// A packet arrived that filled an interior gap — the frame only
    /// completed because a retransmission landed.
    recovered: bool,
}

impl Group {
    /// Packets in emission order: (seq, held).
    fn seq_iter(&self) -> impl Iterator<Item = (u16, &Held)> {
        let base = self.base;
        self.packets
            .iter()
            .map(move |(&off, h)| (base.wrapping_add(off as u16), h))
    }

    /// The seq of the earliest packet.
    fn first_seq(&self) -> Option<u16> {
        if self.packets.is_empty() {
            None
        } else {
            Some(self.base)
        }
    }

    /// The seq of the last packet in stream order.
    fn last_seq(&self) -> Option<u16> {
        self.packets
            .last_key_value()
            .map(|(&off, _)| self.base.wrapping_add(off as u16))
    }

    /// Insert a packet, rebasing the map when a wrap-earlier seq arrives.
    /// Returns false for a duplicate.
    fn insert(&mut self, seq: u16, held: Held) -> bool {
        if !self.packets.is_empty() && FrameAssembler::seq_lt(seq, self.base) {
            let old_base = self.base;
            self.base = seq;
            let shift = old_base.wrapping_sub(seq) as u32;
            self.packets = std::mem::take(&mut self.packets)
                .into_iter()
                .map(|(off, h)| (off + shift, h))
                .collect();
        }
        self.packets
            .insert(seq.wrapping_sub(self.base) as u32, held)
            .is_none()
    }
}

pub struct FrameAssembler {
    /// Frames by RTP timestamp, insertion-ordered for emission.
    groups: Vec<Group>,
    /// Sequence number of the last packet of the last emitted frame —
    /// the next frame must chain onto it.
    last_end: Option<u16>,
    /// True once an IDR has been emitted (or reset after `Broken`).
    synced: bool,
    /// How long an incomplete frame waits for retransmissions.
    pub deadline: Duration,
    /// The highest seq already emitted or dropped: anything at or behind it
    /// (under wrapping order) is a straggler and ignored. A window, not a
    /// set — a HashSet would wrap the u16 seq space in ~2 minutes and drop
    /// the entire stream forever.
    floor: Option<u16>,
    pub stats: ReorderStats,
    /// Packet count held, for the cap.
    held: usize,
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            groups: Vec::new(),
            last_end: None,
            synced: false,
            deadline: Duration::from_millis(150),
            floor: None,
            stats: ReorderStats::default(),
            held: 0,
        }
    }

    /// Clamps the retransmit window so a misconfigured value can't stall the
    /// stream forever or turn the buffer off.
    pub fn set_deadline_ms(&mut self, ms: u64) {
        self.deadline = Duration::from_millis(ms.clamp(40, 400));
    }

    /// The longest a held packet has been waiting — the receive-side metric.
    pub fn hold_ms(&self, now: Instant) -> u64 {
        self.groups
            .iter()
            .map(|g| now.saturating_duration_since(g.arrived).as_millis() as u64)
            .max()
            .unwrap_or(0)
    }

    /// Wrapping-aware sequence order: `a` comes before `b`.
    fn seq_lt(a: u16, b: u16) -> bool {
        (a.wrapping_sub(b) as i16) < 0
    }

    fn seq_next(a: u16) -> u16 {
        a.wrapping_add(1)
    }

    /// Whether `seq` is at or behind the emit/drop floor: already emitted or
    /// already dropped — a straggler must never disturb the live frame.
    fn is_stale(&self, seq: u16) -> bool {
        self.floor.is_some_and(|f| !Self::seq_lt(f, seq))
    }

    /// Wrapping timestamp order for group ordering (`a` earlier than `b`).
    fn ts_lt(a: u32, b: u32) -> bool {
        (a.wrapping_sub(b) as i32) < 0
    }

    /// Push one packet; returns whatever became decidable now.
    pub fn push(&mut self, packet: &Packet, now: Instant) -> Vec<Output> {
        let seq = packet.header.sequence_number;
        if self.is_stale(seq) {
            return Vec::new();
        }
        let ts = packet.header.timestamp;
        // Groups stay ordered by RTP timestamp (wrapping), not arrival order —
        // a retransmit arriving after a later frame's first packet must not
        // reorder emission.
        let pos = match self.groups.iter().position(|g| g.timestamp == ts) {
            Some(pos) => pos,
            None => {
                let pos = self
                    .groups
                    .iter()
                    .position(|g| Self::ts_lt(ts, g.timestamp))
                    .unwrap_or(self.groups.len());
                self.groups.insert(
                    pos,
                    Group {
                        packets: BTreeMap::new(),
                        base: seq,
                        timestamp: ts,
                        arrived: now,
                        recovered: false,
                    },
                );
                pos
            }
        };
        let held = Held {
            packet: packet.clone(),
        };
        if !self.groups[pos].insert(seq, held) {
            return Vec::new(); // duplicate
        }
        // Filling an interior gap (not extending the tail) is what
        // "recovered" means.
        if let Some(last) = self.groups[pos].last_seq() {
            if Self::seq_lt(seq, last) {
                self.groups[pos].recovered = true;
            }
        }
        self.held += 1;
        self.drain(now)
    }

    /// Deadline pass: drop anything expired and emit what became complete.
    pub fn poll(&mut self, now: Instant) -> Vec<Output> {
        self.drain(now)
    }

    /// Emit complete in-order frames; expire stale ones. Strictly ordered:
    /// nothing leaves while an earlier frame is missing.
    fn drain(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        while !self.groups.is_empty() {
            if self.frame_complete(&self.groups[0]) {
                // Chains onto the last emitted frame OR starts a NAL when
                // nothing is yet emitted (start/reset case).
                let group = self.groups.remove(0);
                self.held -= group.packets.len();
                if let Some(frame) = self.finish_group(group) {
                    out.push(Output::Frame(frame));
                }
                continue;
            }
            let group = &self.groups[0];
            if now.saturating_duration_since(group.arrived) >= self.deadline
                || self.held > MAX_PACKETS
            {
                let group = self.groups.remove(0);
                self.held -= group.packets.len();
                self.drop_group(group);
                self.synced = false;
                self.stats.dropped += 1;
                out.push(Output::Broken);
                continue;
            }
            break;
        }
        out
    }

    /// A group is complete when it has a marker packet and every sequence
    /// number from its first to the marker is present — and its first packet
    /// either chains the emitted stream or starts a NAL.
    fn frame_complete(&self, group: &Group) -> bool {
        let Some(first) = group.first_seq() else {
            return false;
        };
        let Some((_, last_held)) = group.packets.iter().next_back() else {
            return false;
        };
        if !last_held.packet.header.marker {
            return false;
        }
        // Every offset 0..=max must be present — the wrap-safe keys make the
        // contiguous check order-agnostic.
        if !group
            .packets
            .keys()
            .enumerate()
            .all(|(i, &off)| off == i as u32)
        {
            return false;
        }
        // Chain rule: continue the emitted stream, or start fresh on a NAL
        // boundary (single NALU or FU-A start fragment).
        match self.last_end {
            Some(end) => first == Self::seq_next(end),
            None => Self::starts_nal(&group.packets[&0].packet),
        }
    }

    /// Whether a packet begins a NALU: a whole NALU payload or an FU-A start.
    fn starts_nal(packet: &Packet) -> bool {
        let Some(&first_byte) = packet.payload.first() else {
            return false;
        };
        let nalu_type = first_byte & 0x1f;
        if nalu_type == FU_A {
            packet.payload.get(1).is_some_and(|h| h & 0x80 != 0)
        } else {
            true
        }
    }

    /// Reassemble a completed group into NALUs in sequence order, handling
    /// FU-A fragmentation. `None` means the sync gate rejected it (pre-IDR).
    fn finish_group(&mut self, group: Group) -> Option<Vec<Vec<u8>>> {
        let mut nalus: Vec<Vec<u8>> = Vec::new();
        let mut fragment: Vec<u8> = Vec::new();
        let mut fragment_type = 0u8;
        let recovered_gap = group.recovered;
        for (seq, held) in group.seq_iter() {
            self.last_end = Some(seq);
            self.floor = Some(seq);
            let payload = &held.packet.payload;
            match payload.first().map(|b| b & 0x1f) {
                Some(FU_A) => {
                    let indicator = payload[0];
                    let header = payload.get(1).copied().unwrap_or(0);
                    if header & 0x80 != 0 {
                        fragment.clear();
                        fragment_type = header & 0x1f;
                    }
                    if payload.len() > 2 {
                        fragment.extend_from_slice(&payload[2..]);
                    }
                    if header & 0x40 != 0 && !fragment.is_empty() {
                        let mut nalu = Vec::with_capacity(fragment.len() + 1);
                        nalu.push((indicator & 0xe0) | fragment_type);
                        nalu.extend_from_slice(&fragment);
                        fragment.clear();
                        nalus.push(nalu);
                    }
                }
                Some(_) => nalus.push(payload.to_vec()),
                None => {}
            }
        }
        if recovered_gap {
            self.stats.recovered += 1;
        }
        // Same sync gate as the depacketizer: nothing decodes until an IDR.
        if nalus.iter().any(|n| nalu_type(n) == IDR) {
            self.synced = true;
        }
        if self.synced && !nalus.is_empty() {
            Some(nalus)
        } else {
            None
        }
    }

    /// Bury a dead group: the floor advances past its span so every
    /// straggler (including the gap that killed it) is dropped instead of
    /// starting a phantom frame. `last_end` resets so the next frame is
    /// accepted on a NAL boundary — chaining on the dropped tail would
    /// cascade the loss into the next frame and stall the stream.
    fn drop_group(&mut self, group: Group) {
        if let Some(last) = group.last_seq() {
            self.floor = Some(last);
        }
        self.last_end = None;
    }
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::Packetizer;
    use bytes::Bytes;
    use rtc::rtp::header::Header;
    use std::time::Duration;

    fn packetizer() -> Packetizer {
        Packetizer::new(0x5a5a_77e1, 96, 400)
    }

    fn pkt(seq: u16, ts: u32, payload: &[u8], marker: bool) -> Packet {
        Packet {
            header: Header {
                version: 2,
                padding: false,
                extension: false,
                marker,
                payload_type: 96,
                sequence_number: seq,
                timestamp: ts,
                ssrc: 1,
                csrc: vec![],
                extension_profile: 0,
                extensions: vec![],
                extensions_padding: 0,
            },
            payload: Bytes::copy_from_slice(payload),
        }
    }

    fn now() -> Instant {
        Instant::now()
    }

    /// In-order packets emit immediately — the clean link adds zero delay.
    #[test]
    fn in_order_frames_emit_immediately() {
        let mut a = FrameAssembler::new();
        let idr = [0x65u8, 1, 2, 3];
        let p = [0x41u8, 9, 9];
        let mut out = a.push(&pkt(0, 3000, &idr, true), now());
        assert!(matches!(out.as_slice(), [Output::Frame(_)]));
        out = a.push(&pkt(1, 6000, &p, true), now());
        assert!(matches!(out.as_slice(), [Output::Frame(_)]));
    }

    /// P-frames are withheld until the first IDR, same as the depacketizer.
    #[test]
    fn p_frames_wait_for_the_first_idr() {
        let mut a = FrameAssembler::new();
        let p = [0x41u8, 9];
        let idr = [0x65u8, 1];
        assert!(a.push(&pkt(0, 1, &p, true), now()).is_empty());
        // The IDR itself emits and unlocks the stream.
        assert!(matches!(
            a.push(&pkt(1, 2, &idr, true), now()).as_slice(),
            [Output::Frame(_)]
        ));
        // Withheld pre-IDR P-frames are dropped at the gate; later ones emit.
        assert!(matches!(
            a.push(&pkt(2, 3, &p, true), now()).as_slice(),
            [Output::Frame(_)]
        ));
    }

    /// A gap filled inside the window emits the frame and counts as recovered.
    #[test]
    fn a_filled_gap_recovers_the_frame() {
        let mut a = FrameAssembler::new();
        let mut pz = packetizer();
        let keyframe = vec![0x65u8; 900];
        let packets = pz.packetize(&keyframe, 3000, true);
        assert!(packets.len() > 2, "fixture must fragment");
        // Deliver 0 and the marker first: incomplete — nothing emits.
        let last = packets.len() - 1;
        assert!(a.push(&packets[0], now()).is_empty());
        assert!(a.push(&packets[last], now()).is_empty());
        // The retransmitted middle packet completes it.
        let out = a.push(&packets[1], now());
        assert!(matches!(out.as_slice(), [Output::Frame(_)]));
        assert_eq!(a.stats.recovered, 1);
    }

    /// An unfilled gap goes Broken on the deadline; P-frames behind it are
    /// dropped until the next IDR re-syncs.
    #[test]
    fn an_unfilled_gap_breaks_and_resyncs_on_idr() {
        let mut a = FrameAssembler::new();
        a.set_deadline_ms(50);
        let t0 = now();
        let idr = [0x65u8, 1];
        a.push(&pkt(0, 1000, &idr, true), t0);
        // P-frame with a gap at seq 2 (seq 1 missing).
        let p = [0x41u8, 2];
        assert!(a.push(&pkt(2, 2000, &p, true), t0).is_empty());
        let out = a.poll(t0 + Duration::from_millis(60));
        assert!(out.contains(&Output::Broken));
        // The next P-frame chains but the sync gate holds it until an IDR.
        let out = a.push(&pkt(3, 3000, &p, true), t0 + Duration::from_millis(70));
        assert!(out.is_empty());
        // An IDR re-syncs.
        let out = a.push(&pkt(4, 4000, &idr, true), t0 + Duration::from_millis(80));
        assert!(matches!(out.as_slice(), [Output::Frame(_)]));
    }

    /// The historical bug: a retransmitted packet for a *dropped* frame must
    /// not corrupt the frame currently being assembled.
    #[test]
    fn a_late_packet_for_a_dropped_frame_is_ignored() {
        let mut a = FrameAssembler::new();
        a.set_deadline_ms(40);
        let t0 = now();
        let idr = [0x65u8, 1];
        a.push(&pkt(0, 1000, &idr, true), t0);
        // Frame at ts 2000: seqs 1,2,3 with marker on 3; seq 2 never arrives
        // → Broken on deadline.
        let p = [0x41u8, 7];
        a.push(&pkt(1, 2000, &p, false), t0);
        a.push(&pkt(3, 2000, &p, true), t0);
        let out = a.poll(t0 + Duration::from_millis(50));
        assert!(out.contains(&Output::Broken));
        // The retransmitted seq 2 arrives late — ignored...
        assert!(a
            .push(&pkt(2, 2000, &p, false), t0 + Duration::from_millis(60))
            .is_empty());
        // ...and the next real frame still completes intact.
        let out = a.push(&pkt(4, 3000, &idr, true), t0 + Duration::from_millis(70));
        assert!(matches!(out.as_slice(), [Output::Frame(_)]));
    }

    /// Duplicates are absorbed without double-processing.
    #[test]
    fn duplicates_are_ignored() {
        let mut a = FrameAssembler::new();
        let idr = [0x65u8, 1];
        a.push(&pkt(0, 1000, &idr, true), now());
        let out = a.push(&pkt(0, 1000, &idr, true), now());
        assert!(out.is_empty());
    }

    /// Sequence numbers wrap at 65535 → 0 without breaking the chain.
    #[test]
    fn sequence_numbers_wrap() {
        let mut a = FrameAssembler::new();
        let idr = [0x65u8, 1];
        let p = [0x41u8, 2];
        assert!(matches!(
            a.push(&pkt(65534, 1000, &idr, true), now()).as_slice(),
            [Output::Frame(_)]
        ));
        assert!(matches!(
            a.push(&pkt(65535, 2000, &p, true), now()).as_slice(),
            [Output::Frame(_)]
        ));
        assert!(matches!(
            a.push(&pkt(0, 3000, &p, true), now()).as_slice(),
            [Output::Frame(_)]
        ));
    }

    /// Out-of-order FU-A fragments still reassemble in sequence order.
    #[test]
    fn out_of_order_fragments_reassemble_in_order() {
        let mut a = FrameAssembler::new();
        let mut pz = packetizer();
        let keyframe = vec![0x65u8; 900];
        let packets = pz.packetize(&keyframe, 3000, true);
        for packet in packets.iter().rev() {
            a.push(packet, now());
        }
        // The frame completed during the reversed feeding; a following
        // frame emitting proves the chain survived.
        let p = [0x41u8, 9];
        let next_seq = packets.len() as u16;
        let out = a.push(&pkt(next_seq, 6000, &p, true), now());
        assert!(matches!(out.as_slice(), [Output::Frame(_)]));
    }

    /// A frame whose tail never arrives is dropped on deadline — the
    /// marker-less tail must not hang the buffer forever.
    #[test]
    fn a_marker_less_tail_times_out() {
        let mut a = FrameAssembler::new();
        a.set_deadline_ms(40);
        let t0 = now();
        let idr = [0x65u8, 1];
        a.push(&pkt(0, 1000, &idr, true), t0);
        let p = [0x41u8, 7];
        // Frame 2's tail is incomplete: seq 2 arrives, marker seq 3 missing.
        a.push(&pkt(2, 2000, &p, false), t0);
        let out = a.poll(t0 + Duration::from_millis(50));
        assert!(out.contains(&Output::Broken));
    }

    /// 70 000 packets in order cross the u16 wrap several times; a staleness
    /// set (the old design) would eat the whole stream after ~2 minutes.
    #[test]
    fn seventy_thousand_packets_survive_the_wrap() {
        let mut a = FrameAssembler::new();
        let idr = [0x65u8, 1];
        let p = [0x41u8, 2];
        let mut emitted = 0u64;
        for i in 0..70_000u32 {
            let seq = (i % 65536) as u16;
            let nalu = if i % 1000 == 0 { &idr } else { &p };
            for out in a.push(&pkt(seq, i * 90, nalu, true), now()) {
                if matches!(out, Output::Frame(_)) {
                    emitted += 1;
                }
            }
        }
        assert_eq!(emitted, 70_000);
    }

    /// A frame whose packets straddle 65535 → 0 completes and emits.
    #[test]
    fn a_frame_straddling_the_wrap_completes() {
        let mut a = FrameAssembler::new();
        let idr = [0x65u8, 1];
        a.push(&pkt(65530, 1000, &idr, true), now());
        let p = [0x41u8, 7];
        // Frame at ts 2000 chains from 65531 and straddles 65535 → 0 —
        // out-of-order delivery too, so the wrap-safe keys do the work.
        assert!(a.push(&pkt(0, 2000, &p, true), now()).is_empty());
        let mut frames = 0;
        for seq in [65531u16, 65532, 65533, 65534, 65535] {
            frames += a
                .push(&pkt(seq, 2000, &p, false), now())
                .iter()
                .filter(|o| matches!(o, Output::Frame(_)))
                .count();
        }
        assert!(a.push(&pkt(65534, 2000, &p, false), now()).is_empty()); // dup
                                                                         // The complete frame (65531..=0) emitted exactly once, in order.
        frames += a
            .poll(now())
            .iter()
            .filter(|o| matches!(o, Output::Frame(_)))
            .count();
        assert_eq!(frames, 1);
    }

    /// Losing frame N's marker drops it at the deadline, but frame N+1 (an
    /// IDR) is accepted on the NAL boundary right away — no second wait.
    #[test]
    fn a_marker_loss_drops_one_frame_not_the_next() {
        let mut a = FrameAssembler::new();
        a.set_deadline_ms(40);
        let t0 = now();
        let idr = [0x65u8, 1];
        a.push(&pkt(0, 1000, &idr, true), t0);
        // Frame at ts 2000: seq 1 (marker is seq 2 — lost).
        let p = [0x41u8, 7];
        a.push(&pkt(1, 2000, &p, false), t0);
        // Frame N+1's packets arrive meanwhile: seqs 3,4,5, marker on 5.
        let idr_nalu = [0x65u8, 9];
        a.push(&pkt(3, 3000, &idr_nalu, false), t0);
        a.push(&pkt(4, 3000, &idr_nalu, false), t0);
        a.push(&pkt(5, 3000, &idr_nalu, true), t0);
        // Deadline: the markerless frame dies; the IDR emits immediately —
        // no second deadline wait, no cascade.
        let out = a.poll(t0 + Duration::from_millis(50));
        assert!(out.contains(&Output::Broken));
        assert!(out.iter().any(|o| matches!(o, Output::Frame(_))));
    }

    /// A frame whose first packet arrives *after* the next frame's packets
    /// is still emitted first — group order is stream order.
    #[test]
    fn a_late_first_packet_does_not_reorder_frames() {
        let mut a = FrameAssembler::new();
        a.set_deadline_ms(200);
        let t0 = now();
        let idr = [0x65u8, 1];
        a.push(&pkt(0, 1000, &idr, true), t0);
        // Frame ts 3000 arrives entirely first (seqs 2,3, marker 3); frame
        // ts 2000's packet (seq 1) comes after — like a retransmit.
        let p = [0x41u8, 8];
        a.push(&pkt(2, 3000, &p, false), t0);
        a.push(&pkt(3, 3000, &p, true), t0);
        // Now the earlier frame's packet lands: still emits BEFORE ts 3000.
        let out = a.push(&pkt(1, 2000, &p, true), t0);
        assert_eq!(out.len(), 2, "ts 2000 then ts 3000, in stream order");
    }
}
