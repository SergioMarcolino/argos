//! Lossy-link harness (run with `cargo test -p argos-core --test lossy -- --ignored --nocapture`).
//!
//! Measures the receive path against packet loss WITHOUT a full ICE session:
//! the old depacketizer (drop-on-gap → decode error → IDR gate) vs the new
//! FrameAssembler plus a simulated NACK loop — each lost packet is re-requested
//! after a 20 ms detect interval, the retransmit takes 40 ms RTT, can itself be
//! lost at the same rate, and is retried up to 3 times.
//! Time is packet-position-derived (~500 pkt/s): one unit ≈ 2 ms.

use argos_core::h264::{Depacketizer, Packetizer};
use argos_core::reorder::{FrameAssembler, Output};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A tiny deterministic PRNG — no external deps.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next_f64(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as f64 / (1u64 << 31) as f64
    }
    fn below(&mut self, n: u64) -> u64 {
        (self.next_f64() * n as f64) as u64
    }
}

fn stream(minutes: u64, seed: u64) -> Vec<rtc::rtp::packet::Packet> {
    // Synthetic 720p30 stream: an IDR every 4 s (~60 KB), P-frames (~6 KB).
    let mut rng = Rng::new(seed);
    let mut pz = Packetizer::new(0x5a5a_77e1, 96, 1200);
    let mut out = Vec::new();
    for frame in 0..minutes * 60 * 30 {
        let keyframe = frame % 120 == 0;
        let size = if keyframe {
            60_000
        } else {
            5_000 + rng.below(4_000) as usize
        };
        let mut nalu = vec![if keyframe { 0x65u8 } else { 0x41u8 }];
        nalu.resize(size, (frame % 251) as u8);
        out.extend(pz.packetize(&nalu, frame as u32 * 3000, true));
    }
    out
}

/// One attempt of one packet through the lossy link: `Some(position_unit)` =
/// delivered at that packet-position, `None` = lost.
struct Link {
    percent: f64,
    burst: usize,
    rng: Rng,
    burst_left: usize,
}

impl Link {
    fn new(percent: f64, burst: usize, seed: u64) -> Self {
        Self {
            percent,
            burst,
            rng: Rng::new(seed),
            burst_left: 0,
        }
    }
    /// Attempt a send at packet position `pos` (~2 ms each). Burst drops are
    /// sticky: once a burst begins it swallows its full run.
    fn transmit(&mut self, pos: usize) -> Option<usize> {
        let lost = if self.burst_left > 0 {
            self.burst_left -= 1;
            true
        } else if self.burst > 0 && self.rng.next_f64() < 0.002 {
            self.burst_left = self.burst - 1;
            true
        } else {
            self.rng.next_f64() < self.percent / 100.0
        };
        if lost {
            None
        } else {
            Some(pos)
        }
    }
}

/// Decode-accurate "before" semantics: the old depacketizer emits a frame
/// when the marker lands — complete or not — and a damaged frame errors the
/// decoder, after which DecodeSink resets and gates everything until the next
/// IDR (exactly the freeze the user reported).
fn run_before(delivered: &[(usize, rtc::rtp::packet::Packet)]) -> (u64, u64) {
    let mut dep = Depacketizer::new();
    let mut frames = 0u64;
    let mut last_pos = 0usize;
    let mut worst_ms = 0u64;
    let mut seen: std::collections::BTreeMap<u32, std::collections::BTreeSet<u16>> =
        Default::default();
    for (pos, p) in delivered {
        let ts = p.header.timestamp;
        let seq = p.header.sequence_number;
        let marker = p.header.marker;
        seen.entry(ts).or_default().insert(seq);
        if let Some(nalus) = dep.push(p) {
            let seqs = &seen[&ts];
            let complete = {
                let first = *seqs.iter().next().unwrap();
                let last_s = *seqs.iter().next_back().unwrap();
                marker && seqs.len() as u32 == last_s.wrapping_sub(first) as u32 + 1
            };
            if complete {
                frames += 1;
                worst_ms = worst_ms.max((pos - last_pos) as u64 * 2);
                last_pos = *pos;
            } else {
                dep.reset();
            }
            let _ = nalus;
        }
    }
    (frames, worst_ms)
}

/// The "after" path: FrameAssembler + a NACK loop. In-flight retransmits are
/// injected at their arrival position; a lost packet whose NACK also dies is
/// retried up to 3 times before being left for the deadline.
fn run_after(
    percent: f64,
    burst: usize,
    stream: &[rtc::rtp::packet::Packet],
) -> (u64, u64, u64, u64) {
    let mut a = FrameAssembler::new();
    let t0 = Instant::now();
    // First pass through the link for originals + the retransmit queue.
    let mut link = Link::new(percent, burst, 7);
    // (arrival pos, packet, try#)
    let mut in_flight: VecDeque<(usize, rtc::rtp::packet::Packet, u8)> = VecDeque::new();
    // (packet index, due pos for its NACK to fire)
    let mut pending_nack: VecDeque<(usize, usize, u8)> = VecDeque::new();
    let mut frames = 0u64;
    let mut broken = 0u64;
    let mut last_frame_pos = 0usize;
    let mut worst_ms = 0u64;
    // When a frame goes Broken the viewer requests a keyframe; the sharer's
    // next frame is an IDR ~60 ms (30 positions) later — the fast recovery
    // the new code path actually performs.
    let mut want_idr_at: Option<usize> = None;

    let total = stream.len();
    for pos in 0..(total + 80_000) {
        let now = t0 + Duration::from_millis(pos as u64 * 2);
        // Retransmits whose arrival time has come.
        while let Some(&(at, _, _)) = in_flight.front() {
            if at <= pos {
                let (_, pkt, _) = in_flight.pop_front().unwrap();
                for out in a.push(&pkt, now) {
                    if let Output::Frame(_) = out {
                        frames += 1;
                        worst_ms = worst_ms.max((pos - last_frame_pos) as u64 * 2);
                        last_frame_pos = pos;
                    }
                }
            } else {
                break;
            }
        }
        // NACKs maturing: queue the next retransmit attempt (which can itself
        // be lost; up to 3 tries).
        while let Some(&(idx, due, tries)) = pending_nack.front() {
            if due <= pos {
                pending_nack.pop_front();
                // The retransmit leg goes through the same lossy link.
                if link.transmit(due).is_some() {
                    in_flight.push_back((due + 20, stream[idx].clone(), tries));
                } else if tries < 3 {
                    pending_nack.push_back((idx, due + 10, tries + 1));
                }
            } else {
                break;
            }
        }
        if pos >= total {
            if pending_nack.is_empty() && in_flight.is_empty() {
                break;
            }
            a.poll(now);
            continue;
        }
        let mut pkt = stream[pos].clone();
        if link.transmit(pos).is_none() {
            // Lost: the NACK generator fires ~20 ms (10 positions) later.
            pending_nack.push_back((pos, pos + 10, 1));
            continue;
        }
        // A requested IDR: flip this frame's FIRST packet to an IDR NALU —
        // an FU-A start fragment carries the type in its second byte, a
        // single NALU in its first.
        if want_idr_at.is_some_and(|at| pos >= at) {
            let is_fua_start = pkt.payload.first().is_some_and(|b| b & 0x1f == 28)
                && pkt.payload.get(1).is_some_and(|h| h & 0x80 != 0);
            let is_single = pkt.payload.first().is_some_and(|b| b & 0x1f == 1);
            if is_fua_start || is_single {
                let mut payload = pkt.payload.to_vec();
                if is_fua_start {
                    payload[1] = (payload[1] & 0xe0) | 5;
                } else {
                    payload[0] = 0x65;
                }
                pkt.payload = bytes::Bytes::from(payload);
                want_idr_at = None;
            }
        }
        for out in a.push(&pkt, now) {
            match out {
                Output::Frame(_) => {
                    frames += 1;
                    worst_ms = worst_ms.max((pos - last_frame_pos) as u64 * 2);
                    last_frame_pos = pos;
                }
                Output::Broken => {
                    broken += 1;
                    want_idr_at = Some(pos + 30);
                }
            }
        }
        for out in a.poll(now) {
            if let Output::Broken = out {
                broken += 1;
                want_idr_at = Some(pos + 30);
            }
        }
    }
    let recovered = a.stats.recovered;
    (frames, worst_ms, broken, recovered)
}

#[test]
#[ignore = "lossy-link measurement — run explicitly"]
fn lossy_link_table() {
    println!(
        "\n=== lossy link: 4-minute 720p30 stream (seq wraps), 20 ms NACK, 40 ms RTT, 3 tries ==="
    );
    println!(
        "{:>8} | {:>24} | {:>40}",
        "loss", "BEFORE frames/worst", "AFTER frames/worst/broken/recovered"
    );
    for (pct, burst, label) in [
        (0.0, 0, "0%"),
        (1.0, 0, "1%"),
        (3.0, 0, "3%"),
        (5.0, 0, "5%"),
        (1.0, 20, "burst20"),
    ] {
        let packets = stream(4, 42);
        let total_frames = 4u64 * 60 * 30;
        // BEFORE pass: same loss model, originals only.
        let mut link = Link::new(pct, burst, 7);
        let delivered: Vec<_> = packets
            .iter()
            .enumerate()
            .filter_map(|(i, p)| link.transmit(i).map(|_| (i, p.clone())))
            .collect();
        let (bf, bg) = run_before(&delivered);
        let (af, ag, bk, rc) = run_after(pct, burst, &packets);
        println!(
            "{:>8} | {:>7}/{:>4} {:>10} | {:>7}/{:>4} {:>10} {:>6} {:>9}",
            label,
            bf,
            total_frames,
            format!("{:>4.2}s", bg as f64 / 1000.0),
            af,
            total_frames,
            format!("{:>4.2}s", ag as f64 / 1000.0),
            bk,
            rc
        );
    }
}
