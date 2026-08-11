//! The seven filter-shape glyphs on the band type buttons.
//!
//! The web UI drew these as SVG paths over a 20×17 box. The same seven outlines
//! are here as line and quadratic segments in the same coordinate space, so the
//! buttons are recognisably the buttons they were.

use nih_plug_egui::egui::{epaint::PathStroke, Color32, Pos2, Rect, Shape, Stroke};

use crate::params::BandKind;

/// Design space of the glyphs, matching the old `viewBox`.
const W: f32 = 20.0;
const H: f32 = 17.0;

/// One segment of an outline: a straight run, or a quadratic through a control
/// point.
enum Seg {
    To(f32, f32),
    Quad(f32, f32, f32, f32),
}

fn outline(kind: BandKind) -> (Pos2, &'static [Seg]) {
    use Seg::{Quad, To};
    match kind {
        BandKind::LowCut => (
            Pos2::new(2.0, 14.0),
            &[To(8.0, 14.0), Quad(13.0, 14.0, 13.0, 3.0), To(18.0, 3.0)],
        ),
        BandKind::LowShelf => (
            Pos2::new(2.0, 4.0),
            &[
                To(7.0, 4.0),
                Quad(10.0, 4.0, 10.0, 9.0),
                To(13.0, 13.0),
                To(18.0, 13.0),
            ],
        ),
        BandKind::Bell => (
            Pos2::new(2.0, 13.0),
            &[Quad(7.0, 13.0, 10.0, 4.0), Quad(13.0, 13.0, 18.0, 13.0)],
        ),
        BandKind::Notch => (
            Pos2::new(2.0, 4.0),
            &[Quad(9.0, 4.0, 10.0, 14.0), Quad(11.0, 4.0, 18.0, 4.0)],
        ),
        BandKind::BandPass => (
            Pos2::new(2.0, 14.0),
            &[Quad(9.0, 14.0, 10.0, 4.0), Quad(11.0, 14.0, 18.0, 14.0)],
        ),
        BandKind::HighShelf => (
            Pos2::new(2.0, 13.0),
            &[
                To(7.0, 13.0),
                To(10.0, 9.0),
                Quad(10.0, 4.0, 13.0, 4.0),
                To(18.0, 4.0),
            ],
        ),
        BandKind::HighCut => (
            Pos2::new(2.0, 3.0),
            &[To(7.0, 3.0), Quad(12.0, 3.0, 12.0, 14.0), To(18.0, 14.0)],
        ),
    }
}

/// Flatten the outline into `rect`, fitted and centred.
pub fn shape(kind: BandKind, rect: Rect, color: Color32, width: f32) -> Shape {
    let scale = (rect.width() / W).min(rect.height() / H);
    let origin = Pos2::new(
        rect.center().x - W * scale / 2.0,
        rect.center().y - H * scale / 2.0,
    );
    let map = |x: f32, y: f32| Pos2::new(origin.x + x * scale, origin.y + y * scale);

    let (start, segments) = outline(kind);
    let mut points = vec![map(start.x, start.y)];
    let mut cursor = start;
    for seg in segments {
        match *seg {
            Seg::To(x, y) => {
                points.push(map(x, y));
                cursor = Pos2::new(x, y);
            }
            Seg::Quad(cx, cy, x, y) => {
                // Eight steps is past the point where another one shows at this
                // size, and these are drawn a dozen at a time.
                for i in 1..=8 {
                    let t = i as f32 / 8.0;
                    let u = 1.0 - t;
                    points.push(map(
                        u * u * cursor.x + 2.0 * u * t * cx + t * t * x,
                        u * u * cursor.y + 2.0 * u * t * cy + t * t * y,
                    ));
                }
                cursor = Pos2::new(x, y);
            }
        }
    }

    Shape::Path(nih_plug_egui::egui::epaint::PathShape {
        points,
        closed: false,
        fill: Color32::TRANSPARENT,
        stroke: PathStroke::new(width, color),
    })
}

/// The power symbol on the bypass button.
pub fn power(rect: Rect, color: Color32, width: f32) -> Shape {
    let c = rect.center();
    let r = rect.width().min(rect.height()) * 0.36;
    let mut arc: Vec<Pos2> = Vec::with_capacity(21);
    // Open at the top, where the stem goes.
    for i in 0..=20 {
        let a = (-125.0 + 250.0 * i as f32 / 20.0f32).to_radians();
        arc.push(Pos2::new(c.x + r * a.sin(), c.y - r * a.cos()));
    }
    Shape::Vec(vec![
        Shape::Path(nih_plug_egui::egui::epaint::PathShape {
            points: arc,
            closed: false,
            fill: Color32::TRANSPARENT,
            stroke: PathStroke::new(width, color),
        }),
        Shape::line_segment(
            [Pos2::new(c.x, c.y - r * 1.25), Pos2::new(c.x, c.y - r * 0.1)],
            Stroke::new(width, color),
        ),
    ])
}

/// The resonance mark: a peak with a bar pressing it down.
pub fn resonance(rect: Rect, color: Color32, width: f32) -> Shape {
    let scale = (rect.width() / 12.0).min(rect.height() / 12.0);
    let o = Pos2::new(
        rect.center().x - 6.0 * scale,
        rect.center().y - 6.0 * scale,
    );
    let map = |x: f32, y: f32| Pos2::new(o.x + x * scale, o.y + y * scale);
    Shape::Vec(vec![
        Shape::Path(nih_plug_egui::egui::epaint::PathShape {
            points: vec![
                map(1.0, 9.5),
                map(3.2, 9.5),
                map(6.0, 3.0),
                map(8.8, 9.5),
                map(11.0, 9.5),
            ],
            closed: false,
            fill: Color32::TRANSPARENT,
            stroke: PathStroke::new(width, color),
        }),
        Shape::line_segment([map(4.0, 5.2), map(8.0, 5.2)], Stroke::new(width, color)),
    ])
}

/// A rightward arrow.
///
/// Drawn rather than typed: egui's bundled fonts have no `→`, and a character
/// the font cannot find comes out as an empty box. Anything in this UI that is
/// really a symbol is a shape for that reason.
pub fn arrow_right(rect: Rect, color: Color32, width: f32) -> Shape {
    let c = rect.center();
    let reach = rect.width() * 0.5;
    let head = rect.height() * 0.25;
    Shape::Vec(vec![
        Shape::line_segment(
            [Pos2::new(c.x - reach, c.y), Pos2::new(c.x + reach, c.y)],
            Stroke::new(width, color),
        ),
        Shape::Path(nih_plug_egui::egui::epaint::PathShape {
            points: vec![
                Pos2::new(c.x + reach - head, c.y - head),
                Pos2::new(c.x + reach, c.y),
                Pos2::new(c.x + reach - head, c.y + head),
            ],
            closed: false,
            fill: Color32::TRANSPARENT,
            stroke: PathStroke::new(width, color),
        }),
    ])
}

/// A chevron, pointing down when closed and up when open.
pub fn chevron(rect: Rect, open: bool, color: Color32) -> Shape {
    let c = rect.center();
    let (w, h) = (3.4, 2.0);
    let dir = if open { -1.0 } else { 1.0 };
    Shape::Path(nih_plug_egui::egui::epaint::PathShape {
        points: vec![
            Pos2::new(c.x - w, c.y - h * dir),
            Pos2::new(c.x, c.y + h * dir),
            Pos2::new(c.x + w, c.y - h * dir),
        ],
        closed: false,
        fill: Color32::TRANSPARENT,
        stroke: PathStroke::new(1.5, color),
    })
}
