//! Compact stereo input/output meters with editor-side display ballistics.
//!
//! The audio thread publishes raw sample peaks. This module turns those into a
//! deliberately readable instrument: transients arrive immediately, levels
//! fall smoothly in dB, and a short peak-hold line keeps the crest visible.

use nih_plug_egui::egui::{
    pos2, Align2, Color32, FontId, Mesh, Rect, Shape, Stroke, StrokeKind, Ui,
};

use crate::gui::theme::{self, fade, white, MOCHI, NEON, SURFACE_DEEP};
use crate::meters::IoPeaks;

pub const FLOOR_DB: f32 = -72.0;
const RELEASE_DB_PER_SECOND: f32 = 36.0;
const HOLD_SECONDS: f64 = 0.7;
const CLIP_SECONDS: f64 = 1.2;

#[derive(Clone, Copy, Debug)]
struct Channel {
    level_db: f32,
    held_db: f32,
    hold_until: f64,
    clip_until: f64,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            level_db: FLOOR_DB,
            held_db: FLOOR_DB,
            hold_until: 0.0,
            clip_until: 0.0,
        }
    }
}

impl Channel {
    fn update(&mut self, peak: f32, now: f64, dt: f32) {
        let incoming = amplitude_to_db(peak);

        // Sample peaks must never be averaged on the way into the display.
        // Only the decay is smoothed, and it is linear in dB so it looks even.
        if incoming >= self.level_db {
            self.level_db = incoming;
        } else {
            self.level_db = (self.level_db - RELEASE_DB_PER_SECOND * dt).max(incoming);
        }

        if incoming >= self.held_db {
            self.held_db = incoming;
            self.hold_until = now + HOLD_SECONDS;
        } else if now > self.hold_until {
            self.held_db = (self.held_db - RELEASE_DB_PER_SECOND * dt).max(self.level_db);
        }

        if peak.is_finite() && peak.abs() >= 1.0 {
            self.clip_until = now + CLIP_SECONDS;
        }
    }
}

/// Persistent visual state for both stereo meters.
#[derive(Clone, Debug, Default)]
pub struct IoMeterState {
    input: [Channel; 2],
    output: [Channel; 2],
    last_time: Option<f64>,
}

impl IoMeterState {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn update(&mut self, peaks: IoPeaks, now: f64) {
        let dt = self
            .last_time
            // Use real elapsed wall time. A host can pause UI painting while
            // audio keeps running; clamping this made the meter stay visibly
            // hot long after its hold/clip timers had already expired.
            .map(|last| (now - last).max(0.0) as f32)
            .unwrap_or(1.0 / 60.0);
        self.last_time = Some(now);

        for channel in 0..2 {
            self.input[channel].update(peaks.input[channel], now, dt);
            self.output[channel].update(peaks.output[channel], now, dt);
        }
    }

    pub fn show(&self, ui: &Ui, input_rect: Rect, output_rect: Rect, now: f64) {
        draw_meter(ui, input_rect, "IN", &self.input, now);
        draw_meter(ui, output_rect, "OUT", &self.output, now);
    }

    pub fn is_active(&self, now: f64) -> bool {
        self.input.iter().chain(self.output.iter()).any(|channel| {
            channel.level_db > FLOOR_DB || channel.held_db > FLOOR_DB || channel.clip_until > now
        })
    }
}

fn amplitude_to_db(peak: f32) -> f32 {
    if peak.is_finite() && peak > 0.0 {
        (20.0 * peak.log10()).max(FLOOR_DB)
    } else {
        FLOOR_DB
    }
}

fn draw_meter(ui: &Ui, rect: Rect, label: &str, channels: &[Channel; 2], now: f64) {
    if rect.width() < 18.0 || rect.height() < 42.0 {
        return;
    }

    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, theme::corner(8), Color32::from_black_alpha(138));
    painter.rect_stroke(
        rect,
        theme::corner(8),
        Stroke::new(1.0, white(24)),
        StrokeKind::Inside,
    );

    let label_h = 16.0;
    let clip_h = 4.0;
    let rails = Rect::from_min_max(
        pos2(rect.min.x + 5.0, rect.min.y + label_h + clip_h + 4.0),
        pos2(rect.max.x - 5.0, rect.max.y - 17.0),
    );
    let gap = 3.0;
    let rail_w = ((rails.width() - gap) * 0.5).max(2.0);

    painter.text(
        pos2(rect.center().x, rect.min.y + 8.0),
        Align2::CENTER_CENTER,
        label,
        theme::medium(theme::MICRO),
        white(126),
    );

    for (index, channel) in channels.iter().enumerate() {
        let x = rails.min.x + index as f32 * (rail_w + gap);
        let rail = Rect::from_min_max(pos2(x, rails.min.y), pos2(x + rail_w, rails.max.y));
        painter.rect_filled(rail, theme::corner(2), SURFACE_DEEP);

        let t = db_to_unit(channel.level_db);
        if t > 0.0 {
            let fill =
                Rect::from_min_max(pos2(rail.min.x, rail.max.y - rail.height() * t), rail.max);
            painter.add(vertical_gradient(fill, fade(NEON, 0.92), fade(MOCHI, 0.72)));
        }

        let hold_y = rail.max.y - rail.height() * db_to_unit(channel.held_db);
        painter.line_segment(
            [pos2(rail.min.x, hold_y), pos2(rail.max.x, hold_y)],
            Stroke::new(1.0, white(220)),
        );

        let clip = Rect::from_min_max(
            pos2(rail.min.x, rect.min.y + label_h),
            pos2(rail.max.x, rect.min.y + label_h + clip_h),
        );
        let clipped = now < channel.clip_until;
        painter.rect_filled(
            clip,
            theme::corner(1),
            if clipped {
                Color32::from_rgb(0xff, 0x54, 0x62)
            } else {
                white(18)
            },
        );
    }

    let hottest = channels
        .iter()
        .map(|channel| channel.level_db)
        .fold(FLOOR_DB, f32::max);
    let readout = if hottest <= FLOOR_DB + 0.01 {
        "-inf".to_owned()
    } else {
        format!("{hottest:.0}")
    };
    painter.text(
        pos2(rect.center().x, rect.max.y - 6.0),
        Align2::CENTER_BOTTOM,
        readout,
        FontId::monospace(theme::MICRO),
        white(150),
    );
}

fn db_to_unit(db: f32) -> f32 {
    ((db.clamp(FLOOR_DB, 0.0) - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

fn vertical_gradient(rect: Rect, bottom: Color32, top: Color32) -> Shape {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
    Shape::Mesh(mesh.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_and_full_scale_map_to_the_meter_ends() {
        assert_eq!(amplitude_to_db(0.0), FLOOR_DB);
        assert_eq!(amplitude_to_db(f32::NAN), FLOOR_DB);
        assert!((amplitude_to_db(1.0) - 0.0).abs() < f32::EPSILON);
        assert_eq!(db_to_unit(FLOOR_DB), 0.0);
        assert_eq!(db_to_unit(0.0), 1.0);
    }

    #[test]
    fn attack_is_instant_and_release_is_time_based() {
        let mut meter = IoMeterState::default();
        meter.update(
            IoPeaks {
                input: [1.0, 0.5],
                output: [0.25, 0.125],
            },
            1.0,
        );
        assert_eq!(meter.input[0].level_db, 0.0);
        assert!((meter.input[1].level_db + 6.0206).abs() < 0.001);

        meter.update(IoPeaks::default(), 1.1);
        assert!((meter.input[0].level_db + 3.6).abs() < 0.001);
        assert_eq!(meter.input[0].held_db, 0.0);
        assert!(meter.input[0].clip_until > 1.1);

        meter.update(IoPeaks::default(), 1.8);
        assert!(meter.input[0].held_db < 0.0);
        assert!(meter.input[0].held_db >= meter.input[0].level_db);

        // A host may suspend painting while its window is obscured. On the
        // first frame back, decay must catch up to wall time instead of acting
        // as though only one 100 ms frame passed.
        meter.update(IoPeaks::default(), 3.8);
        assert!(meter.input[0].level_db <= FLOOR_DB + 0.001);
    }
}
