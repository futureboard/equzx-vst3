//! A software rasteriser for the editor's own output, for looking at.
//!
//! Test-only. The editor cannot be opened on a build machine — baseview asks
//! for an sRGB-capable GLX config and a headless X server has none — so the one
//! thing the other tests cannot do is show what a frame actually looks like.
//! They assert that a panel settled at a sane rectangle and put vertices inside
//! it, which is worth a great deal and is still not the same as seeing it.
//!
//! This takes what egui tessellated and draws it the way the GPU would: the same
//! premultiplied blend, the same font atlas, the same vertex colours. Two things
//! it does not do, both deliberate:
//!
//! * **Paint callbacks are skipped.** Those are the frosted panels and the
//!   bloom, and they need a GL context. Skipping them leaves the flat fallback
//!   the panels are painted on underneath — which is exactly what a driver that
//!   refused the shaders would show, so the result is a real answer to a real
//!   question rather than a gap.
//! * **No sRGB conversion.** egui blends in gamma space and writes gamma-encoded
//!   bytes, so working in the same space is what makes this match the plugin.

use nih_plug_egui::egui::epaint::{ImageData, Primitive, TextureId, Vertex};
use nih_plug_egui::egui::{ClippedPrimitive, Color32, Pos2, Rect, TexturesDelta};

/// The font atlas, as coverage per texel.
pub struct Atlas {
    width: usize,
    height: usize,
    coverage: Vec<f32>,
}

impl Atlas {
    /// Pull the atlas out of the textures egui asked to be uploaded.
    pub fn from_delta(delta: &TexturesDelta) -> Option<Self> {
        let (_, image) = delta
            .set
            .iter()
            .find(|(id, _)| *id == TextureId::Managed(0))?;
        match &image.image {
            ImageData::Font(font) => Some(Self {
                width: font.size[0],
                height: font.size[1],
                coverage: font.pixels.clone(),
            }),
            ImageData::Color(color) => Some(Self {
                width: color.size[0],
                height: color.size[1],
                coverage: color.pixels.iter().map(|p| p.a() as f32 / 255.0).collect(),
            }),
        }
    }

    /// Bilinear sample, clamped. egui points every non-text vertex at a texel
    /// the atlas keeps fully opaque, so this returns 1.0 for ordinary geometry
    /// and the glyph's coverage for text — which is why both can go through the
    /// same path.
    fn sample(&self, u: f32, v: f32) -> f32 {
        if self.coverage.is_empty() {
            return 1.0;
        }
        let x = (u * self.width as f32 - 0.5).clamp(0.0, self.width as f32 - 1.0);
        let y = (v * self.height as f32 - 0.5).clamp(0.0, self.height as f32 - 1.0);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let at = |x: usize, y: usize| self.coverage[y * self.width + x];
        let top = at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx;
        let bottom = at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx;
        top * (1.0 - fy) + bottom * fy
    }
}

pub struct Canvas {
    pub width: usize,
    pub height: usize,
    /// Premultiplied gamma-space RGB, which is what the framebuffer holds.
    pixels: Vec<[f32; 3]>,
}

impl Canvas {
    pub fn new(width: usize, height: usize, clear: Color32) -> Self {
        let clear = [
            clear.r() as f32 / 255.0,
            clear.g() as f32 / 255.0,
            clear.b() as f32 / 255.0,
        ];
        Self {
            width,
            height,
            pixels: vec![clear; width * height],
        }
    }

    /// Draw everything egui produced for one frame.
    ///
    /// Returns how many callback primitives were skipped, so a caller can say so
    /// rather than quietly showing an incomplete picture.
    pub fn draw(&mut self, primitives: &[ClippedPrimitive], atlas: &Atlas) -> usize {
        let mut skipped = 0;
        for primitive in primitives {
            let clip = primitive.clip_rect;
            match &primitive.primitive {
                Primitive::Callback(_) => skipped += 1,
                Primitive::Mesh(mesh) => {
                    for triangle in mesh.indices.chunks_exact(3) {
                        self.triangle(
                            &mesh.vertices[triangle[0] as usize],
                            &mesh.vertices[triangle[1] as usize],
                            &mesh.vertices[triangle[2] as usize],
                            clip,
                            atlas,
                        );
                    }
                }
            }
        }
        skipped
    }

    fn triangle(&mut self, a: &Vertex, b: &Vertex, c: &Vertex, clip: Rect, atlas: &Atlas) {
        let area = edge(a.pos, b.pos, c.pos);
        if area.abs() < 1e-6 {
            return;
        }

        let min_x = a.pos.x.min(b.pos.x).min(c.pos.x).max(clip.min.x).max(0.0);
        let max_x = a
            .pos
            .x
            .max(b.pos.x)
            .max(c.pos.x)
            .min(clip.max.x)
            .min(self.width as f32);
        let min_y = a.pos.y.min(b.pos.y).min(c.pos.y).max(clip.min.y).max(0.0);
        let max_y = a
            .pos
            .y
            .max(b.pos.y)
            .max(c.pos.y)
            .min(clip.max.y)
            .min(self.height as f32);
        if min_x >= max_x || min_y >= max_y {
            return;
        }

        for y in min_y.floor() as usize..(max_y.ceil() as usize).min(self.height) {
            for x in min_x.floor() as usize..(max_x.ceil() as usize).min(self.width) {
                let p = Pos2::new(x as f32 + 0.5, y as f32 + 0.5);
                if !clip.contains(p) {
                    continue;
                }
                let (mut wa, mut wb, mut wc) = (
                    edge(b.pos, c.pos, p) / area,
                    edge(c.pos, a.pos, p) / area,
                    edge(a.pos, b.pos, p) / area,
                );
                // A shade of slack, so the seam between two triangles sharing an
                // edge does not show as a hairline of background.
                if wa < -0.002 || wb < -0.002 || wc < -0.002 {
                    continue;
                }
                let sum = wa + wb + wc;
                wa /= sum;
                wb /= sum;
                wc /= sum;

                let colour = |f: fn(&Vertex) -> f32| wa * f(a) + wb * f(b) + wc * f(c);
                let src = [
                    colour(|v| v.color.r() as f32 / 255.0),
                    colour(|v| v.color.g() as f32 / 255.0),
                    colour(|v| v.color.b() as f32 / 255.0),
                ];
                let alpha = colour(|v| v.color.a() as f32 / 255.0);
                let coverage = atlas.sample(
                    colour(|v| v.uv.x),
                    colour(|v| v.uv.y),
                );

                // What the fragment shader does: multiply the vertex colour by
                // the texture, then blend premultiplied.
                let src_a = alpha * coverage;
                let dst = &mut self.pixels[y * self.width + x];
                for i in 0..3 {
                    dst[i] = src[i] * coverage + dst[i] * (1.0 - src_a);
                }
            }
        }
    }

    /// Encode as a PNG. Stored deflate blocks — no compression, no dependency.
    pub fn to_png(&self) -> Vec<u8> {
        let mut raw = Vec::with_capacity(self.height * (1 + self.width * 3));
        for y in 0..self.height {
            raw.push(0); // filter: none
            for x in 0..self.width {
                for channel in self.pixels[y * self.width + x] {
                    raw.push((channel.clamp(0.0, 1.0) * 255.0).round() as u8);
                }
            }
        }

        let mut zlib = vec![0x78, 0x01];
        for (i, chunk) in raw.chunks(65_535).enumerate() {
            let last = (i + 1) * 65_535 >= raw.len();
            zlib.push(u8::from(last));
            zlib.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
            zlib.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
            zlib.extend_from_slice(chunk);
        }
        zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut header = Vec::new();
        header.extend_from_slice(&(self.width as u32).to_be_bytes());
        header.extend_from_slice(&(self.height as u32).to_be_bytes());
        header.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolour
        chunk(&mut png, b"IHDR", &header);
        chunk(&mut png, b"IDAT", &zlib);
        chunk(&mut png, b"IEND", &[]);
        png
    }
}

fn edge(a: Pos2, b: Pos2, c: Pos2) -> f32 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = kind.to_vec();
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + *byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
