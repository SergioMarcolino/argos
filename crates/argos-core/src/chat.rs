//! One-to-many network chat over TCP: text, images and files.
//!
//! UDP would not carry this — a 25 MiB attachment is thousands of datagrams,
//! each a chance to lose the whole message — so chat is a short TCP connection
//! per message, one frame in each. The listener port is advertised in the LAN
//! hello (`chat_port`); a peer whose hello predates it has no chat and is never
//! sent one.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::lan::{is_allowed, random_id, Allowed};

/// The most text a single message may carry, in characters.
pub const MAX_TEXT_CHARS: usize = 4000;
/// The most bytes a single attachment may carry.
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
/// The most bytes the JSON header may occupy.
const MAX_HEADER_BYTES: u64 = 64 * 1024;
/// Frame format version; anything else is refused.
const VERSION: u32 = 1;
/// A peer that stops mid-frame only gets this long to finish, so a stuck
/// sender cannot hold a connection thread forever.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// What a message carries. The wire tag is the `type` field of the JSON
/// header, so adding a kind later does not move any other field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatKind {
    /// Plain text; `data` is empty and `text` is the message.
    Text { text: String },
    /// An image; `data` is the encoded image (PNG, JPEG, …) and `name` is a
    /// display name, not a path.
    Image { name: String },
    /// Any other file; `data` is the file bytes and `name` is its file name.
    File { name: String },
}

/// One chat message as the app sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatMessage {
    /// Unique per send, so the UI can annotate delivery failures back onto the
    /// entry it appended optimistically.
    pub msg_id: String,
    /// The sender's LAN id — display purposes only; trust comes from `is_allowed`
    /// on the socket's source address, not from this string.
    pub from_id: String,
    pub from_name: String,
    pub kind: ChatKind,
    /// The attachment bytes. Empty for `Text`.
    pub data: Vec<u8>,
}

impl ChatMessage {
    /// A fresh message with a random id, ready to fill in and send.
    pub fn new(from_id: String, from_name: String, kind: ChatKind, data: Vec<u8>) -> Self {
        Self {
            msg_id: random_id(),
            from_id,
            from_name,
            kind,
            data,
        }
    }
}

/// What the listener and sender threads report back to the app.
pub enum ChatEvent {
    /// A complete, valid frame from an allowed peer.
    Received { from: IpAddr, message: ChatMessage },
    /// One target of one send could not be delivered. A send fans out to every
    /// peer, so this fires once per failed target, not per message.
    SendFailed {
        msg_id: String,
        to_name: String,
        error: String,
    },
}

/// The JSON header a frame opens with. Every field except `kind` defaults so a
/// build with a slightly different header still parses — the version check is
/// what gates real incompatibility.
#[derive(Serialize, Deserialize)]
struct Header {
    #[serde(default)]
    v: u32,
    #[serde(default)]
    msg_id: String,
    #[serde(default)]
    from_id: String,
    #[serde(default)]
    from_name: String,
    kind: ChatKind,
}

/// Writes one frame: `[u32 BE header_len][header JSON][u64 BE data_len][data]`.
pub(crate) fn write_frame<W: Write>(writer: &mut W, message: &ChatMessage) -> std::io::Result<()> {
    let header = serde_json::to_vec(&Header {
        v: VERSION,
        msg_id: message.msg_id.clone(),
        from_id: message.from_id.clone(),
        from_name: message.from_name.clone(),
        kind: message.kind.clone(),
    })?;
    writer.write_all(&(header.len() as u32).to_be_bytes())?;
    writer.write_all(&header)?;
    writer.write_all(&(message.data.len() as u64).to_be_bytes())?;
    writer.write_all(&message.data)?;
    Ok(())
}

/// Reads one frame, or says why it was refused.
///
/// Every length is checked before a single byte of that length is allocated:
/// a peer that claims a 4 GiB attachment is dropped on the length word, not on
/// the allocation the word would have caused.
pub(crate) fn read_frame<R: Read>(reader: &mut R) -> Result<ChatMessage, String> {
    let mut raw = [0u8; 4];
    reader
        .read_exact(&mut raw)
        .map_err(|e| format!("truncated header length: {e}"))?;
    let header_len = u32::from_be_bytes(raw) as u64;
    if header_len == 0 || header_len > MAX_HEADER_BYTES {
        return Err(format!("header length {header_len} out of range"));
    }
    let mut header_buf = vec![0u8; header_len as usize];
    reader
        .read_exact(&mut header_buf)
        .map_err(|e| format!("truncated header: {e}"))?;
    let header: Header =
        serde_json::from_slice(&header_buf).map_err(|e| format!("bad header: {e}"))?;
    if header.v != VERSION {
        return Err(format!("unsupported frame version {}", header.v));
    }

    let mut raw = [0u8; 8];
    reader
        .read_exact(&mut raw)
        .map_err(|e| format!("truncated data length: {e}"))?;
    let data_len = u64::from_be_bytes(raw);
    if data_len > MAX_ATTACHMENT_BYTES as u64 {
        return Err(format!("attachment {data_len} bytes exceeds the cap"));
    }
    let mut data = vec![0u8; data_len as usize];
    reader
        .read_exact(&mut data)
        .map_err(|e| format!("truncated data: {e}"))?;

    if let ChatKind::Text { text } = &header.kind {
        if text.chars().count() > MAX_TEXT_CHARS {
            return Err("text exceeds the character cap".to_string());
        }
    }
    Ok(ChatMessage {
        msg_id: header.msg_id,
        from_id: header.from_id,
        from_name: header.from_name,
        kind: header.kind,
        data,
    })
}

/// The chat endpoint: a TCP listener plus the event channel both directions
/// report into.
pub struct Chat {
    port: u16,
    events: Receiver<ChatEvent>,
    /// Kept so `send` can clone it for each delivery worker — a failed connect
    /// still has to reach the UI as `SendFailed`.
    events_tx: Sender<ChatEvent>,
    stop: Arc<AtomicBool>,
    joins: Vec<JoinHandle<()>>,
}

impl Chat {
    /// Binds an ephemeral port on all interfaces. The port is advertised to
    /// peers through the LAN hello, so it does not need to be fixed.
    ///
    /// `allowed` is the same shared filter discovery uses: a connection from
    /// outside the active network is closed unread.
    pub fn start(allowed: Allowed) -> Result<Self, String> {
        let listener =
            TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let (events_tx, events) = channel();

        let listener_stop = Arc::clone(&stop);
        let listener_tx = events_tx.clone();
        let join = thread::Builder::new()
            .name("argos-chat-listen".to_string())
            .spawn(move || listen_loop(listener, listener_stop, allowed, listener_tx))
            .map_err(|e| e.to_string())?;

        Ok(Self {
            port,
            events,
            events_tx,
            stop,
            joins: vec![join],
        })
    }

    /// The port peers should send to — the value that goes into the hello.
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn try_event(&self) -> Option<ChatEvent> {
        self.events.try_recv().ok()
    }

    /// Delivers `message` to each target in turn, on a worker thread.
    ///
    /// Non-blocking by design: the UI thread drives the share pipeline and
    /// cannot wait on a TCP connect to a machine that is down. Each connection
    /// carries exactly one frame and is closed after it — no session, no
    /// retry; a lost message shows up as `SendFailed`, not a hang.
    pub fn send(&self, targets: Vec<(String, SocketAddr)>, message: ChatMessage) {
        let msg_id = message.msg_id.clone();
        let tx = self.events_tx.clone();
        thread::spawn(move || {
            for (to_name, addr) in targets {
                if let Err(error) = send_one(addr, &message) {
                    let _ = tx.send(ChatEvent::SendFailed {
                        msg_id: msg_id.clone(),
                        to_name,
                        error,
                    });
                }
            }
        });
    }
}

fn send_one(addr: SocketAddr, message: &ChatMessage) -> Result<(), String> {
    let mut stream =
        TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|e| e.to_string())?;
    write_frame(&mut stream, message).map_err(|e| e.to_string())?;
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}

fn listen_loop(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    allowed: Allowed,
    events: Sender<ChatEvent>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => {
                if !is_allowed(&allowed.read().ok().and_then(|set| set.clone()), addr.ip()) {
                    continue;
                }
                let events = events.clone();
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
                    let mut stream = stream;
                    if let Ok(message) = read_frame(&mut stream) {
                        let _ = events.send(ChatEvent::Received {
                            from: addr.ip(),
                            message,
                        });
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}

impl Drop for Chat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for join in self.joins.drain(..) {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::io::Cursor;
    use std::net::Ipv4Addr;
    use std::sync::RwLock;
    use std::time::Instant;

    fn text_message(text: &str) -> ChatMessage {
        ChatMessage::new(
            "aaaa".to_string(),
            "desk".to_string(),
            ChatKind::Text {
                text: text.to_string(),
            },
            Vec::new(),
        )
    }

    fn round_trip(message: &ChatMessage) -> Result<ChatMessage, String> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, message).expect("write");
        let mut cursor = Cursor::new(bytes);
        read_frame(&mut cursor)
    }

    /// The frame is the whole contract between two separately built binaries:
    /// what one writes, the other must read back field for field.
    #[test]
    fn a_frame_round_trips() {
        for message in [
            text_message("hello"),
            ChatMessage::new(
                "bbbb".to_string(),
                "sofa".to_string(),
                ChatKind::File {
                    name: "notes.txt".to_string(),
                },
                b"file bytes \x00\x01".to_vec(),
            ),
            ChatMessage::new(
                "cccc".to_string(),
                "lamp".to_string(),
                ChatKind::Image {
                    name: "shot.png".to_string(),
                },
                Vec::new(),
            ),
        ] {
            assert_eq!(round_trip(&message).expect("read"), message);
        }
    }

    /// A claimed header length above the cap is refused on the length word —
    /// the allocation it would have caused never happens.
    #[test]
    fn an_oversize_header_is_refused_without_allocating() {
        let mut bytes = ((MAX_HEADER_BYTES + 1) as u32).to_be_bytes().to_vec();
        let mut cursor = Cursor::new(bytes.clone());
        assert!(read_frame(&mut cursor).is_err());

        // Same for the payload: cap + 1 fails at the length word.
        let header = serde_json::to_vec(&Header {
            v: VERSION,
            msg_id: String::new(),
            from_id: String::new(),
            from_name: String::new(),
            kind: ChatKind::Text {
                text: String::new(),
            },
        })
        .expect("header");
        bytes = (header.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&((MAX_ATTACHMENT_BYTES as u64 + 1).to_be_bytes()));
        let mut cursor = Cursor::new(bytes);
        assert!(read_frame(&mut cursor).is_err());
    }

    /// A peer that goes quiet mid-frame produces an error, not a hang or a
    /// half message. Both truncation points are exercised.
    #[test]
    fn a_truncated_stream_errors() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &text_message("hi")).expect("write");
        for cut in [2, 6, bytes.len() - 3] {
            let mut cursor = Cursor::new(bytes[..cut].to_vec());
            assert!(read_frame(&mut cursor).is_err(), "cut at {cut} parsed");
        }
    }

    /// A frame from a different protocol version is refused outright: its
    /// layout cannot be trusted, so none of it is.
    #[test]
    fn the_wrong_version_is_rejected() {
        let header = serde_json::to_vec(&Header {
            v: VERSION + 1,
            msg_id: String::new(),
            from_id: String::new(),
            from_name: String::new(),
            kind: ChatKind::Text {
                text: "hi".to_string(),
            },
        })
        .expect("header");
        let mut bytes = (header.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&0u64.to_be_bytes());
        let mut cursor = Cursor::new(bytes);
        assert!(read_frame(&mut cursor).is_err());
    }

    /// Text past the character cap is refused; exactly the cap is fine.
    #[test]
    fn text_is_capped_in_characters() {
        let over = "x".repeat(MAX_TEXT_CHARS + 1);
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &text_message(&over)).expect("write");
        let mut cursor = Cursor::new(bytes);
        assert!(read_frame(&mut cursor).is_err());
    }

    fn allowed_none() -> Allowed {
        Arc::new(RwLock::new(None))
    }

    /// The whole path on loopback: bind, send one frame, receive it as an
    /// event. This is the piece everything else assumes works.
    #[test]
    fn a_sent_message_is_received() {
        let chat = Chat::start(allowed_none()).expect("chat starts");
        let message = text_message("hello over tcp");
        chat.send(
            vec![(
                "self".to_string(),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), chat.port()),
            )],
            message.clone(),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(ChatEvent::Received { message: got, .. }) = chat.try_event() {
                assert_eq!(got, message);
                return;
            }
            assert!(Instant::now() < deadline, "no message within 2 s");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// A connection from an address outside the allowed set is closed unread:
    /// the sender may even complete its write, but no event is emitted.
    #[test]
    fn a_peer_outside_the_network_gets_nothing_through() {
        let set: HashSet<Ipv4Addr> = [Ipv4Addr::new(26, 0, 0, 1)].into_iter().collect();
        let chat = Chat::start(Arc::new(RwLock::new(Some(set)))).expect("chat starts");
        chat.send(
            vec![(
                "self".to_string(),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), chat.port()),
            )],
            text_message("should not arrive"),
        );
        thread::sleep(Duration::from_millis(500));
        assert!(
            chat.try_event().is_none(),
            "a filtered-out connection still delivered a message"
        );
    }
}
