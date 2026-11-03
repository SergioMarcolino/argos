//! The launch splash: a full-window overlay for the first ~1.8 s, skipped by
//! any input. Everything it draws is timing, so the timing is a pure function
//! and the draw is just its evaluation.

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32};

use crate::theme;

/// How long the overlay takes when nobody skips it.
const DURATION: Duration = Duration::from_millis(1800);
/// The tail fade's length.
const FADE_OUT: Duration = Duration::from_millis(350);

/// Where the animation is, as four numbers the draw reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplashPhase {
    pub logo_alpha: f32,
    pub logo_scale: f32,
    pub title_alpha: f32,
    /// 1 while fully opaque, falling to 0 in the tail fade.
    pub overlay_alpha: f32,
    pub done: bool,
}

/// Ease-out-back for the logo's scale-in.
fn ease_out_back(t: f32) -> f32 {
    let c = 1.70158;
    let t = t - 1.0;
    1.0 + (c + 1.0) * t * t * t + c * t * t
}

/// Maps elapsed time (with `skipped` applied as a jump into the fade) to the
/// drawable state. Kept pure so the easing curve is a test, not a screenshot.
/// `skipped_at` is the moment the user skipped; from then the clock runs in
/// the fade tail only — playback position plus time since the skip.
pub fn splash_phase(elapsed: Duration, skipped_at: Option<Duration>) -> SplashPhase {
    let elapsed = match skipped_at {
        Some(at) => (DURATION - FADE_OUT) + elapsed.saturating_sub(at),
        None => elapsed,
    };
    let t = elapsed.as_secs_f32();
    let fade = ((t - (DURATION - FADE_OUT).as_secs_f32()) / FADE_OUT.as_secs_f32()).clamp(0.0, 1.0);
    SplashPhase {
        logo_alpha: (t / 0.5).clamp(0.0, 1.0),
        logo_scale: 0.8 + 0.2 * ease_out_back((t / 0.5).clamp(0.0, 1.0)),
        title_alpha: ((t - 0.3) / 0.4).clamp(0.0, 1.0),
        overlay_alpha: 1.0 - fade,
        done: elapsed >= DURATION,
    }
}

/// The overlay's state. The clock starts on the first `update`, not in `new` —
/// creation time is setup, not showtime.
pub struct Splash {
    started: Option<Instant>,
    skipped_at: Option<Duration>,
    done: bool,
}

impl Splash {
    pub fn new() -> Self {
        Self {
            started: None,
            skipped_at: None,
            done: false,
        }
    }

    /// Draws the overlay over the whole window; call last in `update`.
    pub fn show(&mut self, ctx: &egui::Context, icon: &egui::TextureHandle) {
        if self.done {
            return;
        }
        let start = *self.started.get_or_insert_with(Instant::now);
        let elapsed = start.elapsed();
        if self.skipped_at.is_none()
            && ctx.input(|i| i.pointer.any_pressed() || !i.keys_down.is_empty())
        {
            self.skipped_at = Some(elapsed);
        }
        let phase = splash_phase(elapsed, self.skipped_at);
        if phase.done {
            self.done = true;
            return;
        }
        ctx.request_repaint();
        let rect = ctx.screen_rect();
        let layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::new("splash"));
        let painter = ctx.layer_painter(layer);
        painter.rect_filled(
            rect,
            egui::CornerRadius::ZERO,
            theme::RAIL.gamma_multiply(phase.overlay_alpha),
        );
        // Soft radial glow behind the logo: concentric discs fading toward
        // the edge, so there is no visible rim.
        let glow_center = rect.center() - egui::vec2(0.0, 40.0);
        for i in (1..=24u32).rev() {
            let t = i as f32 / 24.0;
            let alpha = (1.0 - t).powi(2) * 0.14 * phase.overlay_alpha;
            painter.circle_filled(
                glow_center,
                200.0 * t,
                theme::ACCENT_FILL.gamma_multiply(alpha),
            );
        }
        let center = rect.center() - egui::vec2(0.0, 60.0);
        let size = 128.0 * phase.logo_scale;
        let logo_rect = egui::Rect::from_center_size(center, egui::vec2(size, size));
        painter.image(
            icon.id(),
            logo_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE.gamma_multiply(phase.logo_alpha * phase.overlay_alpha),
        );
        // Faux-bold: the bundled Plex instance has no bold face, so the
        // wordmark is drawn twice a hair apart.
        for dx in [0.0_f32, 0.6] {
            painter.text(
                center + egui::vec2(dx, size / 2.0 + 26.0),
                egui::Align2::CENTER_CENTER,
                "Argos",
                egui::FontId::proportional(theme::TEXT_XL),
                theme::TEXT.gamma_multiply(phase.title_alpha * phase.overlay_alpha),
            );
        }
        painter.text(
            center + egui::vec2(0.0, size / 2.0 + 56.0),
            egui::Align2::CENTER_CENTER,
            "Watch together.",
            egui::FontId::proportional(theme::TEXT_M),
            theme::MUTED.gamma_multiply(phase.title_alpha * phase.overlay_alpha),
        );
        // Thin accent progress line.
        let progress = (elapsed.as_secs_f32() / DURATION.as_secs_f32()).clamp(0.0, 1.0);
        let line = egui::Rect::from_center_size(
            center + egui::vec2(0.0, size / 2.0 + 96.0),
            egui::vec2(160.0 * progress, 2.0),
        );
        painter.rect_filled(
            line,
            egui::CornerRadius::same(1),
            theme::ACCENT.gamma_multiply(phase.overlay_alpha),
        );
    }
}

impl Default for Splash {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fade-in must only rise, the fade-out only fall, and a skip must
    /// jump into the tail without rewinding the entrance.
    #[test]
    fn the_phases_are_monotonic_and_bounded() {
        let mut last = 0.0;
        for ms in (0..=500).step_by(25) {
            let p = splash_phase(Duration::from_millis(ms), None);
            assert!(p.logo_alpha >= last, "logo alpha went backwards");
            last = p.logo_alpha;
        }
        let mut last = 1.0;
        for ms in (1400..=1800).step_by(25) {
            let p = splash_phase(Duration::from_millis(ms), None);
            assert!(p.overlay_alpha <= last, "fade went backwards");
            last = p.overlay_alpha;
        }
        assert!(splash_phase(DURATION, None).done);
        // A skip at t=100 jumps into the tail: only the fade remains, so it
        // finishes a fade's-length sooner than playback.
        let skipped = splash_phase(Duration::from_millis(100), Some(Duration::ZERO));
        assert!(!skipped.done);
        assert!(skipped.overlay_alpha <= 1.0);
        // The entrance is not rewound: the logo is fully in.
        assert_eq!(skipped.logo_alpha, 1.0);
        assert!(
            splash_phase(
                Duration::from_millis(100) + FADE_OUT,
                Some(Duration::from_millis(100))
            )
            .done
        );
    }
}
