//! Small styled widgets shared across the chrome: the filled primary button,
//! letter avatars, status dots and the LIVE pill.

use eframe::egui::{self, Color32, RichText};

use crate::theme;

/// A deterministic avatar colour: the seed decides hue, so the same person
/// wears the same colour everywhere they appear — every call, every PC,
/// every launch. `RandomState` is deliberately avoided: its keys differ per
/// call, which made colours flicker each frame.
pub fn avatar_color(seed: &str) -> Color32 {
    const PALETTE: [Color32; 5] = [
        Color32::from_rgb(0x7B, 0x5C, 0xF0),
        Color32::from_rgb(0x9B, 0x84, 0xE8),
        Color32::from_rgb(0x5B, 0x8C, 0xD9),
        Color32::from_rgb(0xC0, 0x77, 0xC9),
        Color32::from_rgb(0x68, 0x63, 0xC4),
    ];
    // FNV-1a 64-bit: fixed, portable, and good enough for five buckets.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in seed.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    PALETTE[(hash % PALETTE.len() as u64) as usize]
}

/// One or two uppercase initials, e.g. "Ada Lovelace" -> "AL". `?` for empty.
pub fn initials(name: &str) -> String {
    let mut parts = name.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(first), Some(second)) => {
            let mut out = String::new();
            out.extend(first.chars().take(1));
            out.extend(second.chars().take(1));
            out.to_uppercase()
        }
        (Some(first), None) => first.chars().take(1).collect::<String>().to_uppercase(),
        _ => "?".to_string(),
    }
}

/// A filled circle with the name's initials, coloured by `id_seed`.
pub fn avatar(ui: &mut egui::Ui, name: &str, id_seed: &str, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), size / 2.0, avatar_color(id_seed));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initials(name),
        egui::FontId::proportional(size * 0.42),
        Color32::WHITE,
    );
    response
}

/// Where an avatar's status dot sits and what it says.
#[derive(Clone, Copy, PartialEq)]
pub enum DotKind {
    /// Online, not streaming.
    Online,
    /// Streaming — the red LIVE dot.
    Live,
}

/// A small status dot for the bottom-right of an avatar, ringed so it reads
/// against whatever the avatar's colour happens to be.
pub fn status_dot(painter: &egui::Painter, center: egui::Pos2, kind: DotKind, ring: Color32) {
    painter.circle_filled(center, 4.5, ring);
    painter.circle_filled(
        center,
        3.0,
        match kind {
            DotKind::Online => theme::OK,
            DotKind::Live => theme::LIVE,
        },
    );
}

/// The little red "LIVE" pill on rows that are streaming.
pub fn live_pill(ui: &mut egui::Ui) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        "LIVE".to_owned(),
        egui::FontId::proportional(theme::TEXT_S * 0.85),
        Color32::WHITE,
    );
    let size = galley.size() + egui::vec2(10.0, 4.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(4), theme::LIVE);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
    response
}

/// The filled purple button for primary actions.
pub fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let response = ui.add(
        egui::Button::new(RichText::new(text).color(Color32::WHITE))
            .fill(theme::ACCENT_FILL)
            .corner_radius(egui::CornerRadius::same(theme::RADIUS_S))
            .min_size(egui::vec2(0.0, 34.0)),
    );
    if response.hovered() {
        ui.painter().rect_filled(
            response.rect,
            egui::CornerRadius::same(theme::RADIUS_S),
            theme::ACCENT_FILL_HOVER,
        );
        ui.painter().text(
            response.rect.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(theme::TEXT_M),
            Color32::WHITE,
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flicker bug: `RandomState` made every call a different colour.
    /// Fifty calls on one seed must be identical, and a few seeds must not
    /// all land on one palette entry.
    #[test]
    fn avatar_color_stays_fixed_per_seed() {
        let first = avatar_color("game-night");
        for _ in 0..50 {
            assert_eq!(avatar_color("game-night"), first);
        }
        let colors: std::collections::HashSet<Color32> = [
            "friday-squad",
            "game-night",
            "The Cowboy Room",
            "All",
            "Test Friend",
        ]
        .iter()
        .map(|seed| avatar_color(seed))
        .collect();
        assert!(colors.len() > 1, "every seed mapped to one colour");
    }
}
