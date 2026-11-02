//! The Share screen's source picker: a monitor, or one window via WGC.
//!
//! The selection is kept as an enum (monitor name / hwnd) rather than an index
//! into a list — a refresh reorders the list and an index would silently point
//! at a different source.

use eframe::egui::{self, RichText};

use argos_media::capture::{self, CaptureSource, MonitorInfo, WindowInfo};

/// The picker's state: the enumeration results and the current selection.
pub struct SourcePicker {
    pub monitors: Vec<MonitorInfo>,
    pub windows: Vec<WindowInfo>,
    /// Whether WGC can run here; when false the window half is simply absent.
    supported: bool,
    selection: SourceSel,
    /// Set when a refresh found the chosen window gone.
    notice: Option<String>,
}

#[derive(Clone, PartialEq)]
enum SourceSel {
    /// By monitor device name, which is what DXGI resolves.
    Monitor(String),
    /// By hwnd, which is what WGC captures.
    Window(isize),
}

impl SourcePicker {
    pub fn new() -> Self {
        Self {
            monitors: Vec::new(),
            windows: Vec::new(),
            supported: capture::window_capture_supported(),
            selection: SourceSel::Monitor(String::new()),
            notice: None,
        }
    }

    /// Re-enumerates monitors and windows. A selection that vanished falls
    /// back to the primary monitor with a note — never a silent change.
    pub fn refresh(&mut self) {
        self.monitors = capture::list_monitors();
        self.windows = if self.supported {
            capture::list_windows()
        } else {
            Vec::new()
        };
        if let SourceSel::Monitor(name) = &self.selection {
            if !self.monitors.iter().any(|m| &m.name == name) {
                self.selection = SourceSel::Monitor(String::new());
            }
        }
        if let SourceSel::Window(hwnd) = &self.selection {
            if !self.windows.iter().any(|w| w.hwnd == *hwnd) {
                self.selection = SourceSel::Monitor(String::new());
                self.notice =
                    Some("The selected window is gone — back to the primary screen.".into());
            }
        }
        if self.monitors.is_empty() && self.windows.is_empty() {
            self.notice = Some("No monitor or window found.".into());
        }
    }

    /// The selected source as the capture layer wants it, with a fallback to
    /// the primary monitor when the selection cannot be resolved.
    pub fn source(&self) -> Option<CaptureSource> {
        match &self.selection {
            SourceSel::Window(hwnd) => self
                .windows
                .iter()
                .find(|w| w.hwnd == *hwnd)
                .map(|w| CaptureSource::Window(w.clone()))
                .or_else(|| self.primary_monitor()),
            SourceSel::Monitor(name) => self
                .monitors
                .iter()
                .find(|m| &m.name == name)
                .map(|m| CaptureSource::Monitor(m.clone()))
                .or_else(|| self.primary_monitor()),
        }
    }

    fn primary_monitor(&self) -> Option<CaptureSource> {
        let m = self
            .monitors
            .iter()
            .find(|m| m.is_primary)
            .or(self.monitors.first())?;
        Some(CaptureSource::Monitor(m.clone()))
    }

    /// Whether a window is the current source — for the "whole system audio"
    /// note the share view shows.
    pub fn is_window(&self) -> bool {
        matches!(self.selection, SourceSel::Window(_))
    }

    /// The combo plus its refresh button. Disabled while a capture is live —
    /// a source change mid-stream silently did nothing before, which read as
    /// a bug rather than a rule.
    pub fn picker(&mut self, ui: &mut egui::Ui, busy: bool) {
        ui.horizontal(|ui| {
            let label = match self.source() {
                Some(CaptureSource::Monitor(m)) => format!("Screen: {}", monitor_label(&m)),
                Some(CaptureSource::Window(w)) => {
                    format!("Window: {}", window_label(&w))
                }
                None => "No source".to_string(),
            };
            ui.add_enabled_ui(!busy, |ui| {
                let response = egui::ComboBox::from_label("Source")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        for monitor in &self.monitors {
                            let text = format!("Screen: {}", monitor_label(monitor));
                            if ui
                                .selectable_label(
                                    self.selection == SourceSel::Monitor(monitor.name.clone()),
                                    text,
                                )
                                .clicked()
                            {
                                self.selection = SourceSel::Monitor(monitor.name.clone());
                                self.notice = None;
                            }
                        }
                        if !self.windows.is_empty() {
                            ui.separator();
                        }
                        for window in &self.windows {
                            let text = format!("Window: {}", window_label(window));
                            if ui
                                .selectable_label(
                                    self.selection == SourceSel::Window(window.hwnd),
                                    text,
                                )
                                .clicked()
                            {
                                self.selection = SourceSel::Window(window.hwnd);
                                self.notice = None;
                            }
                        }
                    });
                if busy {
                    response
                        .response
                        .on_disabled_hover_text("Stop sharing and preview to change the source");
                }
            });
            if ui
                .small_button("↻")
                .on_hover_text("Refresh sources")
                .clicked()
            {
                self.refresh();
            }
        });
        if let Some(notice) = &self.notice {
            ui.label(RichText::new(notice).weak());
        }
        if self.is_window() {
            ui.label(RichText::new("Audio: whole system sound is shared").weak());
        }
    }
}

fn monitor_label(info: &MonitorInfo) -> String {
    let primary = if info.is_primary { " [primary]" } else { "" };
    format!("{} ({}x{}){}", info.name, info.width, info.height, primary)
}

/// `title — process`, truncated so a page-title marathon cannot blow the combo.
fn window_label(info: &WindowInfo) -> String {
    let title: String = info.title.chars().take(60).collect();
    let title = if info.title.chars().count() > 60 {
        format!("{title}…")
    } else {
        title
    };
    if info.process.is_empty() {
        title
    } else {
        format!("{title} — {}", info.process)
    }
}
