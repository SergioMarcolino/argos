//! The chat panel: history, composer, attachments.
//!
//! The transport lives in `argos_core::chat`; this module owns the UI state —
//! entries, textures, the unread count — and the slow parts of sending (file
//! reads, the open dialog, clipboard images), all of which run on spawned
//! threads so the UI thread is never blocked driving the share pipeline.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use eframe::egui::{self, RichText};
use image::ImageEncoder;

use argos_core::chat::{Chat, ChatKind, ChatMessage, MAX_ATTACHMENT_BYTES, MAX_TEXT_CHARS};
use argos_core::lan::LanPeer;

use crate::ui::theme;

/// How many entries the history holds before the oldest are dropped, textures
/// included — a long session cannot be allowed to grow without bound.
const HISTORY_CAP: usize = 300;

/// The most attachment bytes the history may retain. 300 entries × 25 MiB
/// would be gigabytes, so once the total passes this, the oldest attachments
/// shed their bytes — the entry, its thumbnail and its "not delivered" marks
/// all stay; only the original data goes.
const MAX_HISTORY_BYTES: usize = 256 * 1024 * 1024;

/// The largest edge a stored image texture may have. Big enough to read text
/// in a screenshot, small enough that a busy image chat does not eat the GPU.
const MAX_TEXTURE_EDGE: u32 = 1280;

/// The box a thumbnail is fitted into in the history.
const THUMB_MAX: egui::Vec2 = egui::vec2(240.0, 180.0);

/// One line in the history.
struct Entry {
    /// Empty for system lines, which are not messages and cannot fail.
    msg_id: String,
    mine: bool,
    sender: String,
    time: String,
    content: Content,
    /// Names of peers this message failed to reach, if any did.
    failed: Vec<String>,
    /// Where an attachment was saved, once it has been.
    saved: Option<PathBuf>,
}

enum Content {
    Text(String),
    Image {
        name: String,
        /// The original bytes — what Save writes. Dropped once evicted.
        data: Vec<u8>,
        /// The bytes were shed to stay under `MAX_HISTORY_BYTES`.
        evicted: bool,
        /// Filled in once the background decode finishes.
        texture: Option<egui::TextureHandle>,
        decoding: bool,
    },
    File {
        name: String,
        /// The original bytes; dropped once evicted.
        data: Vec<u8>,
        /// The size at arrival, kept after `data` is evicted.
        size: u64,
        evicted: bool,
    },
    /// A divider, e.g. switching networks.
    System(String),
}

/// A file the OS handed us — from the open dialog or a drop — whose bytes were
/// read off the UI thread and are now ready to send.
struct PickedFile {
    name: String,
    result: Result<Vec<u8>, String>,
}

/// A finished background image decode: `(entry key, pixels or why it failed)`.
type DecodeResult = (String, Result<egui::ColorImage, String>);

/// Everything the panel needs from the app, passed in each frame because the
/// panel is a view over state it does not own.
pub struct ChatArgs<'a> {
    pub chat: Option<&'a Chat>,
    /// The LAN peers messages fan out to — already network-filtered.
    pub peers: &'a [LanPeer],
    pub my_id: &'a str,
    pub my_name: &'a str,
    /// Why `chat` is `None`, when it is.
    pub error: Option<&'a str>,
}

#[derive(Default)]
pub struct ChatPanel {
    entries: Vec<Entry>,
    /// Total attachment bytes the entries still hold, for `MAX_HISTORY_BYTES`.
    retained: usize,
    input: String,
    /// Inline composer feedback: oversize attachment, unreadable file.
    error: Option<String>,
    unread: usize,
    /// Background completions; both channels are created lazily on first use.
    decode: Option<(Sender<DecodeResult>, Receiver<DecodeResult>)>,
    picked: Option<(Sender<PickedFile>, Receiver<PickedFile>)>,
    /// Which entry's image is open in the full-size window, by msg_id.
    image_window: Option<String>,
    /// Ctrl+V edge detection — `GetAsyncKeyState` reports level, and the send
    /// must fire once per press, not every frame the key is down.
    paste_held: bool,
}

impl ChatPanel {
    /// Messages that arrived while the panel was hidden. Cleared on open.
    pub fn unread(&self) -> usize {
        self.unread
    }

    pub fn clear_unread(&mut self) {
        self.unread = 0;
    }

    /// A message arrived from the LAN. Appends it and, for images, queues the
    /// decode that turns `data` into something drawable.
    pub fn push_received(&mut self, message: ChatMessage, visible: bool) {
        self.unread = next_unread(self.unread, visible);
        let content = match &message.kind {
            ChatKind::Text { text } => Content::Text(text.clone()),
            ChatKind::Image { name } => {
                self.queue_decode(message.msg_id.clone(), message.data.clone());
                Content::Image {
                    name: name.clone(),
                    data: message.data.clone(),
                    evicted: false,
                    texture: None,
                    decoding: true,
                }
            }
            ChatKind::File { name } => Content::File {
                name: name.clone(),
                size: message.data.len() as u64,
                data: message.data.clone(),
                evicted: false,
            },
        };
        self.push(Entry {
            msg_id: message.msg_id,
            mine: false,
            sender: sanitize_sender(&message.from_name),
            time: now_hhmm(),
            content,
            failed: Vec::new(),
            saved: None,
        });
    }

    /// One target of one send refused the message; annotate the entry that was
    /// appended when it went out.
    pub fn mark_failed(&mut self, msg_id: &str, to_name: &str) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.msg_id == msg_id) {
            entry.failed.push(to_name.to_string());
        }
    }

    /// A non-message line, e.g. "— Network: home —".
    pub fn system_line(&mut self, text: String) {
        self.push(Entry {
            msg_id: String::new(),
            mine: false,
            sender: String::new(),
            time: String::new(),
            content: Content::System(text),
            failed: Vec::new(),
            saved: None,
        });
    }

    fn push(&mut self, entry: Entry) {
        self.retained += attachment_bytes(&entry.content);
        self.entries.push(entry);
        // Dropping an entry drops its texture with it — `TextureHandle` frees
        // on drop — so the cap bounds GPU memory too.
        if self.entries.len() > HISTORY_CAP {
            let removed = self.entries.remove(0);
            self.retained = self
                .retained
                .saturating_sub(attachment_bytes(&removed.content));
        }
        self.enforce_budget();
    }

    /// Sheds the bytes of the oldest attachments until the history is back
    /// under `MAX_HISTORY_BYTES`.
    fn enforce_budget(&mut self) {
        let mut index = 0;
        while self.retained > MAX_HISTORY_BYTES && index < self.entries.len() {
            let freed = evict(&mut self.entries[index].content);
            if freed == 0 {
                index += 1;
            } else {
                self.retained -= freed;
            }
        }
    }

    /// The whole panel: right sidebar, history on top, composer underneath.
    pub fn show(&mut self, ctx: &egui::Context, args: &ChatArgs<'_>) {
        self.drain_completed(ctx, args);
        self.poll_paste(ctx, args);
        self.poll_dropped(ctx);

        egui::SidePanel::right("chat_panel")
            .resizable(true)
            .default_width(320.0)
            .width_range(220.0..=600.0)
            .show(ctx, |ui| {
                ui.label(RichText::new("Chat").strong());
                if let Some(error) = args.error {
                    ui.label(RichText::new(format!("Chat unavailable: {error}")).color(theme::ERR));
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        if self.entries.is_empty() {
                            ui.label(RichText::new("No messages yet.").weak());
                        }
                        for entry in &mut self.entries {
                            Self::entry(ui, &mut self.image_window, entry);
                        }
                    });
                self.composer(ui, args);
            });

        self.full_image_window(ctx);
    }

    /// Picks up whatever the background threads finished: decoded images,
    /// picked/read files, clipboard PNGs.
    fn drain_completed(&mut self, ctx: &egui::Context, args: &ChatArgs<'_>) {
        if let Some((_, rx)) = &self.decode {
            let mut done = Vec::new();
            while let Ok(result) = rx.try_recv() {
                done.push(result);
            }
            for (msg_id, result) in done {
                if let Some(Entry {
                    content:
                        Content::Image {
                            texture, decoding, ..
                        },
                    ..
                }) = self.entries.iter_mut().find(|e| e.msg_id == msg_id)
                {
                    *decoding = false;
                    *texture = result.ok().map(|image| {
                        ctx.load_texture(
                            format!("chat-img-{msg_id}"),
                            image,
                            egui::TextureOptions::LINEAR,
                        )
                    });
                }
            }
        }
        if let Some((_, rx)) = &self.picked {
            let mut done = Vec::new();
            while let Ok(picked) = rx.try_recv() {
                done.push(picked);
            }
            for picked in done {
                match picked.result {
                    Ok(bytes) => self.send_attachment(args, picked.name, bytes),
                    // Empty string = the user cancelled the dialog.
                    Err(error) if !error.is_empty() => self.error = Some(error),
                    _ => {}
                }
            }
        }
    }

    /// Ctrl+V with an image on the clipboard sends it as PNG.
    ///
    /// egui-winit consumes Ctrl+V before it becomes an egui key event: when
    /// the clipboard is text it pushes `Event::Paste`, and when it is an image
    /// it pushes nothing at all — so the only way to see the gesture is the
    /// raw key state. If a `Paste` event did arrive this frame, text won and
    /// the image check is skipped, so a clipboard holding both does not
    /// double-send.
    fn poll_paste(&mut self, ctx: &egui::Context, args: &ChatArgs<'_>) {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_V};
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(false);
        let text_pasted =
            ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
        let down = focused
            && !text_pasted
            && unsafe {
                GetAsyncKeyState(VK_CONTROL.0 as i32) < 0 && GetAsyncKeyState(VK_V.0 as i32) < 0
            };
        if !down {
            self.paste_held = false;
            return;
        }
        if self.paste_held {
            return;
        }
        self.paste_held = true;
        if args.chat.is_none() {
            return;
        }
        let tx = self.picked_sender();
        thread::spawn(move || {
            // No image on the clipboard is not an error — most Ctrl+V presses
            // are text, and those are egui's to handle.
            if let Ok(bytes) = clipboard_png() {
                let _ = tx.send(PickedFile {
                    name: "clipboard.png".to_string(),
                    result: Ok(bytes),
                });
            }
        });
    }

    /// Files dropped anywhere on the window become attachments.
    fn poll_dropped(&mut self, ctx: &egui::Context) {
        let paths: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        for path in paths {
            self.read_file(path);
        }
    }

    /// Reads `path` on a background thread and sends the result as an
    /// attachment. Reading on the UI thread would stall the share pipeline
    /// for the duration of a slow disk.
    fn read_file(&mut self, path: PathBuf) {
        let tx = self.picked_sender();
        thread::spawn(move || {
            let _ = tx.send(read_attachment(&path));
        });
    }

    /// The "Attach…" button: the dialog and the read both happen on a spawned
    /// thread. `GetOpenFileNameW` blocks until the user answers, which on the
    /// UI thread would freeze the whole app for as long as the dialog is up.
    fn open_dialog(&mut self) {
        let tx = self.picked_sender();
        thread::spawn(move || {
            let result = pick_file();
            match result {
                None => {
                    // Cancelled: empty error is the convention picked up by
                    // `drain_completed`.
                    let _ = tx.send(PickedFile {
                        name: String::new(),
                        result: Err(String::new()),
                    });
                }
                Some(path) => {
                    let _ = tx.send(read_attachment(&path));
                }
            }
        });
    }

    /// The sender half of the picked-file channel, created on first use.
    fn picked_sender(&mut self) -> Sender<PickedFile> {
        self.picked.get_or_insert_with(channel).0.clone()
    }

    /// One entry row: who and when, then the content.
    fn entry(ui: &mut egui::Ui, image_window: &mut Option<String>, entry: &mut Entry) {
        if let Content::System(text) = &entry.content {
            ui.label(RichText::new(text).weak().italics());
            return;
        }
        let sender = if entry.mine {
            "You".to_string()
        } else {
            entry.sender.clone()
        };
        ui.label(RichText::new(format!("{sender}  {}", entry.time)).weak());
        match &mut entry.content {
            Content::Text(text) => {
                ui.add(egui::Label::new(text.as_str()).selectable(true).wrap());
            }
            Content::Image {
                name,
                data,
                evicted,
                texture,
                decoding,
            } => {
                ui.label(RichText::new(name.as_str()).weak());
                match texture {
                    Some(texture) => {
                        let image = egui::Image::new(&*texture).max_size(THUMB_MAX);
                        let response = ui.add(image);
                        if response.interact(egui::Sense::click()).clicked() {
                            *image_window = Some(entry.msg_id.clone());
                        }
                    }
                    None => {
                        ui.label(
                            RichText::new(if *decoding {
                                "Decoding…"
                            } else {
                                "Could not decode this image"
                            })
                            .weak(),
                        );
                    }
                }
                if *evicted {
                    ui.label(RichText::new("No longer in memory").weak());
                } else {
                    Self::save_row(ui, data, name, &mut entry.saved);
                }
            }
            Content::File {
                name,
                data,
                size,
                evicted,
            } => {
                ui.label(format!("{}  ({})", name, human_size(*size)));
                if *evicted {
                    ui.label(RichText::new("No longer in memory").weak());
                } else {
                    Self::save_row(ui, data, name, &mut entry.saved);
                }
            }
            Content::System(_) => {}
        }
        for name in &entry.failed {
            ui.label(RichText::new(format!("not delivered to {name}")).color(theme::ERR));
        }
    }

    /// Save / "Show in folder" under an attachment. Nothing is ever saved or
    /// opened automatically — received bytes are somebody else's.
    fn save_row(ui: &mut egui::Ui, data: &[u8], name: &str, saved: &mut Option<PathBuf>) {
        ui.horizontal(|ui| match saved {
            Some(path) => {
                if ui.small_button("Show in folder").clicked() {
                    let _ = std::process::Command::new("explorer")
                        .arg(format!("/select,{}", path.display()))
                        .spawn();
                }
            }
            None => {
                if ui.small_button("Save").clicked() {
                    *saved = save_attachment(name, data).ok();
                }
            }
        });
    }

    /// The full-size view of an image entry, in its own resizable window.
    fn full_image_window(&mut self, ctx: &egui::Context) {
        let Some(msg_id) = self.image_window.clone() else {
            return;
        };
        let Some(entry) = self.entries.iter_mut().find(|e| e.msg_id == msg_id) else {
            self.image_window = None;
            return;
        };
        let mut open = true;
        if let Content::Image {
            name,
            data,
            texture: Some(texture),
            ..
        } = &mut entry.content
        {
            egui::Window::new(name.as_str())
                .open(&mut open)
                .resizable(true)
                .default_size([640.0, 480.0])
                .show(ctx, |ui| {
                    let available = ui.available_size();
                    let scale = (available.x / texture.size()[0] as f32)
                        .min(available.y / texture.size()[1] as f32)
                        .clamp(0.05, 1.0);
                    let size = egui::vec2(
                        texture.size()[0] as f32 * scale,
                        texture.size()[1] as f32 * scale,
                    );
                    ui.add(egui::Image::new(&*texture).fit_to_exact_size(size));
                });
            let _ = data;
        }
        if !open {
            self.image_window = None;
        }
    }

    /// Text field, attach button, send handling.
    fn composer(&mut self, ui: &mut egui::Ui, args: &ChatArgs<'_>) {
        ui.separator();
        let targets = chat_targets(args.peers);
        let can_send = args.chat.is_some() && !targets.is_empty();
        if args.chat.is_some() && targets.is_empty() {
            ui.label(RichText::new("Nobody in this network has chat").weak());
        }
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(theme::ERR));
        }
        ui.add_enabled_ui(can_send, |ui| {
            ui.horizontal(|ui| {
                if ui.small_button("Attach…").clicked() {
                    self.open_dialog();
                }
                // Enter sends; Shift+Enter is a newline. The key must be eaten
                // before the TextEdit sees it, or the send also inserts the
                // newline mid-message — and the press only counts when the
                // composer has focus, checked through its stable id.
                let edit_id = ui.id().with("composer");
                let send_pressed = ui.memory(|m| m.has_focus(edit_id))
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                let edit = egui::TextEdit::multiline(&mut self.input)
                    .id(edit_id)
                    .hint_text("Message — Enter to send, Shift+Enter for a new line")
                    .desired_rows(2);
                let response = ui.add(edit);
                if send_pressed {
                    self.send_text(args);
                    response.request_focus();
                }
                if ui.button("Send").clicked() {
                    self.send_text(args);
                }
            });
        });
    }

    fn send_text(&mut self, args: &ChatArgs<'_>) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        // The draft stays until a send succeeds — an over-cap message is an
        // error to show, not a reason to delete what the user typed.
        if text.chars().count() > MAX_TEXT_CHARS {
            self.error = Some(format!("Message is over {MAX_TEXT_CHARS} characters"));
            return;
        }
        self.input.clear();
        self.error = None;
        let message = ChatMessage::new(
            args.my_id.to_string(),
            args.my_name.to_string(),
            ChatKind::Text { text: text.clone() },
            Vec::new(),
        );
        self.deliver(args, message, Content::Text(text));
    }

    /// Routes an attachment to Image or File by name, enforces the size cap,
    /// and sends. Called for picked files, dropped files and clipboard images.
    fn send_attachment(&mut self, args: &ChatArgs<'_>, name: String, data: Vec<u8>) {
        if data.len() > MAX_ATTACHMENT_BYTES {
            self.error = Some(format!("{name} is over the 25 MB limit"));
            return;
        }
        self.error = None;
        let is_image = is_image_name(&name);
        let kind = if is_image {
            ChatKind::Image { name: name.clone() }
        } else {
            ChatKind::File { name: name.clone() }
        };
        let message = ChatMessage::new(
            args.my_id.to_string(),
            args.my_name.to_string(),
            kind,
            data.clone(),
        );
        let content = if is_image {
            // Own images get the same decode path as received ones, so the
            // preview cannot lie about what was sent.
            self.queue_decode(message.msg_id.clone(), data.clone());
            Content::Image {
                name,
                data,
                evicted: false,
                texture: None,
                decoding: true,
            }
        } else {
            Content::File {
                name,
                size: data.len() as u64,
                data,
                evicted: false,
            }
        };
        self.deliver(args, message, content);
    }

    /// Sends and appends the local copy immediately, so the writer never
    /// waits for delivery to see their own line.
    fn deliver(&mut self, args: &ChatArgs<'_>, message: ChatMessage, content: Content) {
        if let Some(chat) = args.chat {
            chat.send(chat_targets(args.peers), message.clone());
        }
        self.push(Entry {
            msg_id: message.msg_id,
            mine: true,
            sender: "You".to_string(),
            time: now_hhmm(),
            content,
            failed: Vec::new(),
            saved: None,
        });
    }

    /// Decodes `data` on a background thread and reports to `msg_id`'s entry.
    fn queue_decode(&mut self, msg_id: String, data: Vec<u8>) {
        let tx = self.decode.get_or_insert_with(channel).0.clone();
        thread::spawn(move || {
            let result = decode_image(&data);
            let _ = tx.send((msg_id, result));
        });
    }
}

/// The attachment bytes an entry still holds. Text and system lines hold
/// none; an evicted attachment's bytes are already gone.
fn attachment_bytes(content: &Content) -> usize {
    match content {
        Content::Image { data, .. } | Content::File { data, .. } => data.len(),
        _ => 0,
    }
}

/// Drops an attachment's bytes, returning how many were freed. `0` for text,
/// system lines, and attachments already shed.
fn evict(content: &mut Content) -> usize {
    match content {
        Content::Image { data, evicted, .. } | Content::File { data, evicted, .. } if !*evicted => {
            let freed = data.len();
            *data = Vec::new();
            *evicted = true;
            freed
        }
        _ => 0,
    }
}

/// The delivery list: every chat-capable peer's address.
fn chat_targets(peers: &[LanPeer]) -> Vec<(String, SocketAddr)> {
    peers
        .iter()
        .filter(|peer| peer.chat_port != 0)
        .map(|peer| {
            (
                peer.name.clone(),
                SocketAddr::new(peer.addr.ip(), peer.chat_port),
            )
        })
        .collect()
}

/// The unread count after one more message: hidden panels accumulate, visible
/// ones do not.
fn next_unread(unread: usize, visible: bool) -> usize {
    if visible {
        unread
    } else {
        unread + 1
    }
}

/// A received sender name, made safe to draw: control characters out, and a
/// blank name is still somebody.
fn sanitize_sender(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    if clean.is_empty() {
        "Someone".to_string()
    } else {
        clean.chars().take(64).collect()
    }
}

/// The local clock as `HH:MM`, for entry timestamps.
fn now_hhmm() -> String {
    let time = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:02}:{:02}", time.wHour, time.wMinute)
}

/// A file name that is safe to write under Downloads: no path parts, no
/// characters Windows forbids, and it is never empty.
fn sanitize_filename(name: &str) -> String {
    let clean: String = name
        .chars()
        .filter(|c| {
            !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') && !c.is_control()
        })
        .collect();
    let clean = clean.trim_matches(|c| c == '.' || c == ' ').to_string();
    if clean.is_empty() {
        "file".to_string()
    } else {
        clean
    }
}

/// `name`, or `name (n)` with the number inside the extension, when the plain
/// name is already taken in `existing`.
fn dedupe_name(existing: &HashSet<String>, name: &str) -> String {
    if !existing.contains(name) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    let mut n = 1u32;
    loop {
        let candidate = format!("{stem} ({n}){ext}");
        if !existing.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Where received attachments land, and the write itself.
fn save_attachment(name: &str, data: &[u8]) -> std::io::Result<PathBuf> {
    let dir = PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default())
        .join("Downloads")
        .join("Argos");
    std::fs::create_dir_all(&dir)?;
    let existing: HashSet<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    let path = dir.join(dedupe_name(&existing, &sanitize_filename(name)));
    std::fs::write(&path, data)?;
    Ok(path)
}

/// Bytes, KiB, MiB — the only magnitudes the 25 MB cap can produce.
fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Whether a file name looks like an image we can try to decode.
fn is_image_name(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    )
}

/// Decode + downscale, for the background image thread.
fn decode_image(data: &[u8]) -> Result<egui::ColorImage, String> {
    let image = image::load_from_memory(data).map_err(|e| e.to_string())?;
    let (w, h) = (image.width(), image.height());
    let scale = MAX_TEXTURE_EDGE as f32 / w.max(h) as f32;
    let image = if scale < 1.0 {
        image.resize(
            (w as f32 * scale) as u32,
            (h as f32 * scale) as u32,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    Ok(egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

/// Reads a file for sending, checking its size before its bytes: a multi-GB
/// drop must fail on the metadata alone rather than being pulled into memory
/// first.
fn read_attachment(path: &std::path::Path) -> PickedFile {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let result = match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_ATTACHMENT_BYTES as u64 => {
            Err(format!("{name} is over the 25 MB limit"))
        }
        Ok(_) => std::fs::read(path).map_err(|e| format!("{name}: {e}")),
        Err(e) => Err(format!("{name}: {e}")),
    };
    PickedFile { name, result }
}

/// The clipboard's image, re-encoded as PNG so it travels as a file format
/// rather than raw pixels.
fn clipboard_png() -> Result<Vec<u8>, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let image = clipboard.get_image().map_err(|e| e.to_string())?;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &image.bytes,
            image.width as u32,
            image.height as u32,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| e.to_string())?;
    Ok(png)
}

/// The native open-file dialog. `None` means the user cancelled.
fn pick_file() -> Option<PathBuf> {
    use windows::core::PWSTR;
    use windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OPENFILENAMEW,
    };
    let mut buffer = [0u16; 1024];
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        lpstrFile: PWSTR(buffer.as_mut_ptr()),
        nMaxFile: buffer.len() as u32,
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_HIDEREADONLY,
        ..Default::default()
    };
    let ok = unsafe { GetOpenFileNameW(&mut ofn) }.as_bool();
    if !ok {
        return None;
    }
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(0);
    Some(PathBuf::from(String::from_utf16_lossy(&buffer[..end])))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sanitizer is what stands between a hostile file name and the user's
    /// Downloads folder: traversal, separators and Windows-reserved characters
    /// must all be gone.
    #[test]
    fn filenames_are_sanitized() {
        assert_eq!(sanitize_filename("..\\..\\evil.exe"), "evil.exe");
        assert_eq!(
            sanitize_filename("a/b\\c:d*e?f\"g<h>i|j.txt"),
            "abcdefghij.txt"
        );
        assert_eq!(sanitize_filename("..."), "file");
        assert_eq!(sanitize_filename(""), "file");
        assert_eq!(sanitize_filename("plain.txt"), "plain.txt");
    }

    /// A name that collides is numbered inside its extension, and the counter
    /// keeps going past the first taken slot.
    #[test]
    fn taken_names_dedupe_with_a_counter() {
        let existing: HashSet<String> = ["a.txt", "a (1).txt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(dedupe_name(&existing, "a.txt"), "a (2).txt");
        assert_eq!(dedupe_name(&existing, "b.txt"), "b.txt");
        let existing: HashSet<String> = ["photo"].iter().map(|s| s.to_string()).collect();
        assert_eq!(dedupe_name(&existing, "photo"), "photo (1)");
    }

    #[test]
    fn sizes_read_like_people_expect() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(25 * 1024 * 1024), "25.0 MB");
    }

    /// Only extensions the decoder is built with count as images — everything
    /// else is a file, so an ".exe" never gets offered a preview.
    #[test]
    fn image_names_classify_by_extension() {
        for name in ["a.png", "b.JPG", "c.jpeg", "d.gif", "e.webp", "f.bmp"] {
            assert!(is_image_name(name), "{name}");
        }
        for name in ["a.exe", "b.txt", "pngfile", "c.png.txt"] {
            assert!(!is_image_name(name), "{name}");
        }
    }

    /// Over-budget attachments shed only their bytes: the entry, and the fact
    /// it was an attachment, survive — and a second call frees nothing.
    #[test]
    fn eviction_drops_bytes_but_keeps_the_entry() {
        let mut file = Content::File {
            name: "big.bin".to_string(),
            data: vec![7u8; 1024],
            size: 1024,
            evicted: false,
        };
        assert_eq!(attachment_bytes(&file), 1024);
        assert_eq!(evict(&mut file), 1024);
        assert_eq!(attachment_bytes(&file), 0);
        assert!(matches!(file, Content::File { evicted: true, .. }));
        assert_eq!(evict(&mut file), 0);

        // Text and system lines are never budget food.
        let mut text = Content::Text("hi".to_string());
        assert_eq!(evict(&mut text), 0);
        let mut system = Content::System("— x —".to_string());
        assert_eq!(evict(&mut system), 0);
    }

    /// Hidden accumulates; visible does not. The count is the button label's
    /// "(n)" — a wrong rule here either nags about read messages or hides new
    /// ones.
    #[test]
    fn unread_counts_only_while_hidden() {
        assert_eq!(next_unread(0, false), 1);
        assert_eq!(next_unread(3, false), 4);
        assert_eq!(next_unread(3, true), 3);
    }
}
