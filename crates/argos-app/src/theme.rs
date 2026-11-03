//! The visible language of the app, in one place.
//!
//! Argos is a screen-watching tool for a trusted network: it runs dark, in a
//! corner, while the real work happens elsewhere. The palette is that brief —
//! quiet indigo surfaces with one light-purple accent, so the single loud
//! thing, whether a friend is *live*, is unmissable.
use eframe::egui::{self, Color32, CornerRadius, Visuals};

/// Server rail and title bar — the darkest surface.
pub const RAIL: Color32 = Color32::from_rgb(0x16, 0x14, 0x1C);
/// Channel sidebar.
pub const SIDEBAR: Color32 = Color32::from_rgb(0x1D, 0x1A, 0x24);
/// Content background — a graphite-indigo near-black.
pub const BG: Color32 = Color32::from_rgb(0x24, 0x21, 0x2D);
/// Floating surfaces: windows, cards.
pub const SURFACE: Color32 = Color32::from_rgb(0x2B, 0x27, 0x36);
/// Resting controls: buttons, selectable rows.
pub const RAISED: Color32 = Color32::from_rgb(0x32, 0x2D, 0x3F);
/// Hovered controls.
pub const HOVER: Color32 = Color32::from_rgb(0x52, 0x4A, 0x6B);
/// Held controls — above HOVER so a press reads as going in.
pub const PRESSED: Color32 = Color32::from_rgb(0x62, 0x59, 0x7D);
/// Sunken fields egui draws itself, chiefly text edits and the composer.
pub const INSET: Color32 = Color32::from_rgb(0x18, 0x15, 0x1F);
/// The faintest surface egui draws, for alternating rows.
pub const FAINT: Color32 = Color32::from_rgb(0x21, 0x1E, 0x2A);
/// Hairlines between sections of a dark surface.
pub const BORDER: Color32 = Color32::from_rgb(0x33, 0x2D, 0x42);

/// Primary text. Contrast on `BG`: ~13:1.
pub const TEXT: Color32 = Color32::from_rgb(0xEE, 0xEB, 0xF5);
/// Secondary text: anything that must hold still while status changes.
pub const MUTED: Color32 = Color32::from_rgb(0xA3, 0x9C, 0xB8);
/// The brand — light purple for highlighted text, links and selections.
pub const ACCENT: Color32 = Color32::from_rgb(0xB9, 0xA6, 0xFF);
/// Filled primary actions (white text on top).
pub const ACCENT_FILL: Color32 = Color32::from_rgb(0x7B, 0x5C, 0xF0);
/// Primary actions while hovered.
pub const ACCENT_FILL_HOVER: Color32 = Color32::from_rgb(0x8D, 0x72, 0xF5);
/// A peer is live and watching / a healthy status.
pub const OK: Color32 = Color32::from_rgb(0x5D, 0xD3, 0x9E);
/// Something is being waited on.
pub const WARN: Color32 = Color32::from_rgb(0xF0, 0xB8, 0x5A);
/// Something is wrong.
pub const ERR: Color32 = Color32::from_rgb(0xF2, 0x77, 0x7A);
/// The "LIVE" pill.
pub const LIVE: Color32 = Color32::from_rgb(0xE5, 0x48, 0x4D);

/// The user panel's strip at the sidebar's foot — a half-step above RAIL.
pub const USER_PANEL: Color32 = Color32::from_rgb(0x19, 0x16, 0x20);

/// Spacing scale, in points. Reach for the nearest step rather than a bare
/// number so the rhythm survives an edit.
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_S: f32 = 8.0;
pub const SPACE_M: f32 = 12.0;
pub const SPACE_L: f32 = 20.0;

/// Corner radii, egui's own unit.
pub const RADIUS_S: u8 = 6;
pub const RADIUS: u8 = 10;
/// Floating windows and cards.
pub const RADIUS_L: u8 = 14;

/// Type scale, in points: caption, body, heading, and the splash wordmark.
/// Installed into egui as its text styles, so a screen asks for "heading" or
/// "body" rather than passing a bare size to a call site.
pub const TEXT_S: f32 = 12.0;
pub const TEXT_M: f32 = 14.0;
pub const TEXT_L: f32 = 20.0;
pub const TEXT_XL: f32 = 28.0;

/// Height of the custom title bar, close to a native Windows caption.
pub const TITLE_BAR_HEIGHT: f32 = 32.0;
/// The server rail's fixed width.
pub const RAIL_WIDTH: f32 = 72.0;
/// The channel header's height, matching the rail's row spacing.
pub const CHANNEL_HEADER: f32 = 56.0;
/// The user panel's height at the sidebar's foot.
pub const USER_PANEL_HEIGHT: f32 = 52.0;
/// Widest a column of text may get, so prose does not stretch across a
/// maximised window. Screens that show video ignore it on purpose.
pub const CONTENT_MAX_WIDTH: f32 = 720.0;

/// The sidebar's width, and the range a drag may take it to. The People list
/// grows with the names in it, so the panel is resizable — but not unbounded:
/// a sidebar that can swallow the window is worse than one that cannot move.
pub const SIDEBAR_WIDTH: f32 = 240.0;
pub const SIDEBAR_MIN_WIDTH: f32 = 190.0;
pub const SIDEBAR_MAX_WIDTH: f32 = 420.0;

/// Installs the palette and the type scale into egui's global style.
///
/// Text colours still follow egui's dark defaults — this moves surfaces,
/// status, and sizes — so `.weak()` and friends keep their meaning.
pub fn apply(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = SURFACE;
    visuals.extreme_bg_color = INSET;
    visuals.faint_bg_color = FAINT;
    visuals.window_corner_radius = CornerRadius::same(RADIUS_L);
    visuals.window_shadow = egui::Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(80),
    };
    visuals.widgets.inactive.bg_fill = RAISED;
    visuals.widgets.inactive.weak_bg_fill = RAISED;
    visuals.widgets.inactive.corner_radius = CornerRadius::same(RADIUS_S);
    visuals.widgets.hovered.bg_fill = HOVER;
    visuals.widgets.hovered.corner_radius = CornerRadius::same(RADIUS_S);
    visuals.widgets.active.bg_fill = PRESSED;
    visuals.widgets.active.corner_radius = CornerRadius::same(RADIUS_S);
    // Any hairline egui draws — separators, widget outlines — is the palette's
    // BORDER, not the default dark-theme grey.
    visuals.widgets.noninteractive.bg_stroke.color = BORDER;
    visuals.widgets.noninteractive.fg_stroke.color = TEXT;
    visuals.weak_text_color = Some(MUTED);
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    visuals.hyperlink_color = ACCENT;

    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (egui::TextStyle::Heading, egui::FontId::proportional(TEXT_L)),
        (egui::TextStyle::Body, egui::FontId::proportional(TEXT_M)),
        (egui::TextStyle::Button, egui::FontId::proportional(TEXT_M)),
        (egui::TextStyle::Small, egui::FontId::proportional(TEXT_S)),
        (egui::TextStyle::Monospace, egui::FontId::monospace(TEXT_M)),
    ]
    .into();
    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 7.0);
    style.spacing.window_margin = egui::Margin::same(16);
    // Hover/press fills ease in instead of snapping.
    style.animation_time = 0.12;
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.floating = true;
    style.spacing.scroll.floating_width = 6.0;
    ctx.set_style(style);
}

/// The app's proportional face: IBM Plex Sans, embedded so the type is
/// Argos's own rather than whatever egui ships. This is the variable font,
/// resolved at its default instance (Regular); its licence sits beside it as
/// `assets/IBMPlexSans-OFL.txt`.
const PROPORTIONAL: &[u8] = include_bytes!("../assets/IBMPlexSans.ttf");

/// Installs the font families.
///
/// Separate from `apply` because it must run *once*: `set_fonts` rebuilds the
/// glyph atlas, and rebuilding it every frame would cost more than the type
/// is worth. So this is called from `ArgosApp::new`, and `apply` stays the
/// per-frame part.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "plex-sans".to_owned(),
        egui::FontData::from_static(PROPORTIONAL).into(),
    );
    // Front of the proportional family, so every non-monospace label is Plex.
    // The bundled faces stay behind it as fallbacks for the glyphs it does
    // not carry — emoji, arrows and the like.
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "plex-sans".to_owned());
    ctx.set_fonts(fonts);
}
