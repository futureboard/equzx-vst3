//! The GPU effects the UI is drawn with.
//!
//! egui already renders on the GPU — every panel, curve and label is tessellated
//! once and drawn by the same OpenGL context baseview hands the editor. What
//! this module adds is the two things geometry alone cannot express, and which
//! the old web UI leaned on CSS and canvas for:
//!
//! * **frosted glass**, which needs whatever is already on screen behind a panel
//!   blurred in place — the `backdrop-filter` of the stylesheet this was ported
//!   from; and
//! * **bloom**, which the composite EQ curve used to get from a canvas
//!   `shadowBlur` around its own path.
//!
//! Both are screen-space post passes. They reach the framebuffer through an
//! [`egui::PaintCallback`], which the glow renderer invokes mid-frame with the
//! live GL context: whatever has been drawn up to that point in the frame is
//! sitting in the default framebuffer, so a panel that emits its callback before
//! painting itself gets exactly the backdrop a real sheet of glass would.
//!
//! The blur is dual-Kawase — successive halvings with a five-tap filter on the
//! way down and a tent-weighted eight-tap on the way back up. Three levels reach
//! an effective radius of tens of pixels while only ever convolving over a
//! sixty-fourth of the region's pixels, which is what keeps eight frosted panels
//! affordable at sixty frames a second.
//!
//! Every effect has a plain painted fallback while its program initializes or
//! after a driver failure. Once ready, the callback replaces that fallback so
//! its backdrop copy sees the real surface behind the glass.

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};

use egui_glow::glow::{self, HasContext};
use egui_glow::Painter;
use nih_plug_egui::egui::{self, Color32, PaintCallbackInfo, Pos2, Rect};

const VERT: &str = include_str!("shaders/fullscreen.vert");
const DOWN: &str = include_str!("shaders/down.frag");
const UP: &str = include_str!("shaders/up.frag");
const GLASS: &str = include_str!("shaders/glass.frag");
const BLOOM: &str = include_str!("shaders/bloom.frag");

/// Enough entries for the plot bloom and every distinct glass surface visible
/// at once. Exact-size chains avoid sampling unused texture capacity, while the
/// small LRU prevents resizing from retaining an unbounded trail of old sizes.
const MAX_SCRATCH_CHAINS: usize = 8;

fn effects_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var_os("EQUZX_DISABLE_FX").is_some())
}

/// The material one smoked-glass surface is made of. Sizes are in egui
/// points; the renderer converts.
///
/// One struct serves every large surface — header, inspector, the recessed
/// dynamics card, popovers, the band readout — with the presets below setting
/// how forward each one sits. Small controls do not run this shader; they
/// approximate the material with plain paint in `chrome`.
#[derive(Clone, Copy, Debug)]
pub struct Glass {
    /// Tint laid over the blurred backdrop. Alpha is how much of the plate is
    /// tint rather than backdrop.
    pub tint: Color32,
    pub corner_radius: f32,
    /// The reflection hanging just inside the top edge.
    pub top_reflection: f32,
    /// The directional rim where the border faces the light, up and to the
    /// left of the surface.
    pub edge_reflection: f32,
    /// The thin bevel highlight tracing the whole border.
    pub inner_highlight: f32,
    /// How much darker the plate sits into its bottom seat.
    pub bottom_shadow: f32,
    /// Width of the crisp rim reflection, in points.
    pub specular_width: f32,
    /// The one broad, static reflection band crossing the surface.
    pub band: f32,
    /// Ambient accent bled into the body, as if the UI's pink reflected in.
    pub rose: f32,
    /// Film grain, which keeps a large plate from banding.
    pub noise: f32,
    /// Where the specular highlight sits, relative to the plate's top-left.
    pub sheen: Option<Pos2>,
    pub sheen_amount: f32,
    /// Whole-plate opacity, for eased fades. 1.0 is the resting state.
    pub opacity: f32,
    /// Halvings before the blur turns around. More is softer and cheaper per
    /// pixel, but starts to lose the shape of what is behind it.
    pub levels: u32,
}

impl Default for Glass {
    fn default() -> Self {
        Self::panel(0.8)
    }
}

impl Glass {
    /// A floating plate, by how forward it sits: the header at 1.0, the
    /// inspector around 0.8, popovers around 0.7. One knob keeps the family
    /// related without making every surface equally glossy.
    pub fn panel(strength: f32) -> Self {
        let tune = crate::gui::tune::get();
        let reflect = strength * tune.glass_reflection;
        Self {
            tint: Color32::from_rgba_unmultiplied(
                0x24,
                0x24,
                0x2b,
                (172.0 * tune.glass_tint) as u8,
            ),
            corner_radius: 22.0,
            top_reflection: 0.072 * reflect,
            edge_reflection: 0.40 * reflect * tune.glass_edge,
            inner_highlight: 0.055 * reflect,
            bottom_shadow: 0.14 * strength.min(1.0),
            specular_width: 1.6,
            band: 0.022 * reflect,
            rose: 0.010 * strength,
            noise: 0.012,
            sheen: None,
            sheen_amount: 0.0,
            opacity: 1.0,
            levels: 3,
        }
    }

    /// The recessed card inside the inspector: more smoked, barely lit, no
    /// broad reflection — glass set into the surface rather than floating
    /// over it.
    pub fn recessed() -> Self {
        let tune = crate::gui::tune::get();
        Self {
            tint: Color32::from_rgba_unmultiplied(
                0x18,
                0x18,
                0x20,
                (188.0 * tune.glass_tint) as u8,
            ),
            corner_radius: 16.0,
            top_reflection: 0.040 * tune.glass_reflection,
            edge_reflection: 0.18 * tune.glass_reflection * tune.glass_edge,
            inner_highlight: 0.032 * tune.glass_reflection,
            bottom_shadow: 0.18,
            specular_width: 1.2,
            band: 0.0,
            rose: 0.007,
            noise: 0.010,
            sheen: None,
            sheen_amount: 0.0,
            opacity: 1.0,
            levels: 2,
        }
    }

    /// The small readout beside a band handle: nearly opaque smoked black
    /// with one caught edge, so the text stays sharp over a live spectrum.
    pub fn tooltip() -> Self {
        let tune = crate::gui::tune::get();
        Self {
            tint: Color32::from_rgba_unmultiplied(0x0a, 0x0a, 0x0d, 205),
            corner_radius: 8.0,
            top_reflection: 0.035 * tune.glass_reflection,
            edge_reflection: 0.20 * tune.glass_reflection * tune.glass_edge,
            inner_highlight: 0.030 * tune.glass_reflection,
            bottom_shadow: 0.16,
            specular_width: 1.2,
            band: 0.0,
            rose: 0.006,
            noise: 0.008,
            sheen: None,
            sheen_amount: 0.0,
            opacity: 1.0,
            levels: 2,
        }
    }
}

/// An additive glow over whatever is already in the region.
#[derive(Clone, Copy, Debug)]
pub struct Bloom {
    pub tint: Color32,
    pub intensity: f32,
    /// Brightness a pixel has to reach before it contributes, 0..1.
    pub threshold: f32,
    pub levels: u32,
}

impl Default for Bloom {
    fn default() -> Self {
        Self {
            tint: Color32::WHITE,
            intensity: 0.55,
            threshold: 0.5,
            levels: 3,
        }
    }
}

#[derive(Clone, Copy)]
enum Job {
    Glass(Glass),
    Bloom(Bloom),
}

impl Job {
    /// How far outside the rectangle the blur has to reach for the edges to be
    /// fed real pixels rather than a clamped smear.
    fn padding(&self) -> i32 {
        let levels = match self {
            Job::Glass(g) => g.levels,
            Job::Bloom(b) => b.levels,
        };
        // Each level roughly doubles the reach of the four-texel Kawase tap.
        (1 << levels.min(5)) * 3
    }

    fn levels(&self) -> u32 {
        match self {
            Job::Glass(g) => g.levels.clamp(1, 5),
            Job::Bloom(b) => b.levels.clamp(1, 5),
        }
    }
}

/// Shared, lazily built GL state. One of these lives for as long as the editor.
pub struct FxRenderer {
    state: Mutex<State>,
}

enum State {
    Uninit,
    /// The shaders would not build on this driver. Recorded so the failure costs
    /// one log line rather than one per frame, and every effect quietly falls
    /// back to the flat paint underneath it.
    Failed(Arc<glow::Context>),
    Ready(Resources),
}

impl Default for FxRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl FxRenderer {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::Uninit),
        }
    }

    /// Frost everything already drawn behind `rect`.
    pub fn glass(self: &Arc<Self>, ctx: &egui::Context, rect: Rect, glass: Glass) -> egui::Shape {
        self.shape(ctx, rect, Job::Glass(glass))
    }

    /// Add a bloom over everything already drawn inside `rect`.
    pub fn bloom(self: &Arc<Self>, ctx: &egui::Context, rect: Rect, bloom: Bloom) -> egui::Shape {
        self.shape(ctx, rect, Job::Bloom(bloom))
    }

    /// Whether custom compositing is live. Until the first callback has built
    /// the programs, and whenever it failed or was explicitly disabled, callers
    /// keep their plain painted fallback underneath the controls.
    pub fn is_ready(&self) -> bool {
        if effects_disabled() {
            return false;
        }
        self.state
            .lock()
            .is_ok_and(|state| matches!(*state, State::Ready(_)))
    }

    fn shape(self: &Arc<Self>, ctx: &egui::Context, rect: Rect, job: Job) -> egui::Shape {
        let this = Arc::clone(self);
        let repaint_ctx = ctx.clone();
        egui::Shape::Callback(egui::epaint::PaintCallback {
            rect,
            callback: Arc::new(egui_glow::CallbackFn::new(move |info, painter| {
                if this.render(&info, painter, job) {
                    // The frame was built using the previous availability
                    // state. Repaint so a successful init drops the fallback,
                    // or a runtime failure restores it immediately.
                    repaint_ctx.request_repaint();
                }
            })),
        })
    }

    /// Returns true when the UI must rebuild its CPU fallback choice.
    fn render(&self, info: &PaintCallbackInfo, painter: &Painter, job: Job) -> bool {
        // The A/B switch: with EQUZX_DISABLE_FX set, no custom GL runs at all
        // and every surface stays on its flat painted fallback.
        if effects_disabled() {
            return false;
        }

        let gl = painter.gl();

        let Ok(mut state) = self.state.lock() else {
            return false;
        };

        // The editor state outlives an individual baseview window. Reopening
        // the editor creates a new GL context, so numeric object names cached
        // from the previous one must be discarded and rebuilt. Retaining the
        // glow Arc makes pointer identity generation-safe even if allocators
        // would otherwise reuse the same address.
        let context_changed = match &*state {
            State::Ready(res) => !Arc::ptr_eq(&res.context, gl),
            State::Failed(context) => !Arc::ptr_eq(context, gl),
            State::Uninit => false,
        };
        if context_changed {
            *state = State::Uninit;
        }

        let mut repaint = context_changed;
        if matches!(*state, State::Uninit) {
            repaint = true;
            *state = match Resources::new(gl) {
                Ok(res) => State::Ready(res),
                Err(err) => {
                    nih_plug::nih_log!(
                        "EQUZX: GPU effects unavailable, falling back to flat panels: {err}"
                    );
                    State::Failed(Arc::clone(gl))
                }
            };
        }
        let State::Ready(res) = &mut *state else {
            return repaint;
        };

        // Safety: this callback only ever runs from the renderer's own thread,
        // between two of its draw calls, with the context current. Everything
        // below restores the bindings egui does not put back itself.
        if let Err(err) = unsafe { res.run(gl, info, job) } {
            // Every index in a blur chain denotes a specific scale. A missing
            // target cannot be treated as a shorter chain without sampling the
            // wrong resolution, so fail closed and restore the CPU fallback.
            unsafe { res.destroy(gl) };
            nih_plug::nih_log!(
                "EQUZX: GPU effects stopped after a render-target failure, falling back to flat panels: {err}"
            );
            *state = State::Failed(Arc::clone(gl));
            return true;
        }
        repaint
    }
}

/// One rung of the blur chain: a texture and the framebuffer that renders to it.
struct Level {
    texture: glow::Texture,
    framebuffer: glow::Framebuffer,
    width: i32,
    height: i32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ChainKey {
    width: i32,
    height: i32,
    count: usize,
}

/// One complete dual-Kawase scratch chain. Equal-sized callbacks reuse it
/// sequentially because egui invokes callbacks in draw order on one thread.
struct ScratchChain {
    key: ChainKey,
    levels: Vec<Level>,
    last_used: u64,
}

/// A compiled program with its uniform locations resolved once.
struct Shader {
    program: glow::Program,
    uniforms: Vec<(&'static str, Option<glow::UniformLocation>)>,
}

impl Shader {
    unsafe fn new(
        gl: &glow::Context,
        fragment: &str,
        names: &[&'static str],
    ) -> Result<Self, String> {
        let program = unsafe { link(gl, VERT, fragment)? };
        let uniforms = names
            .iter()
            .map(|name| (*name, unsafe { gl.get_uniform_location(program, name) }))
            .collect();
        Ok(Self { program, uniforms })
    }

    fn at(&self, name: &str) -> Option<&glow::UniformLocation> {
        self.uniforms
            .iter()
            .find(|(n, _)| *n == name)
            .and_then(|(_, loc)| loc.as_ref())
    }
}

struct Resources {
    context: Arc<glow::Context>,
    /// Core profiles refuse to draw without one, even for a shader that reads no
    /// attributes at all.
    vao: glow::VertexArray,
    down: Shader,
    up: Shader,
    glass: Shader,
    bloom: Shader,
    chains: Vec<ScratchChain>,
    use_counter: u64,
}

impl Resources {
    fn new(gl: &Arc<glow::Context>) -> Result<Self, String> {
        unsafe {
            let vao = gl.create_vertex_array()?;
            Ok(Self {
                context: Arc::clone(gl),
                vao,
                down: Shader::new(gl, DOWN, &["u_tex", "u_texel", "u_bright", "u_threshold"])?,
                up: Shader::new(gl, UP, &["u_tex", "u_texel"])?,
                glass: Shader::new(
                    gl,
                    GLASS,
                    &[
                        "u_tex",
                        "u_uv_scale",
                        "u_uv_offset",
                        "u_size",
                        "u_radius",
                        "u_tint",
                        "u_mat_a",
                        "u_mat_b",
                        "u_noise",
                        "u_sheen",
                        "u_sheen_amount",
                    ],
                )?,
                bloom: Shader::new(
                    gl,
                    BLOOM,
                    &[
                        "u_tex",
                        "u_uv_scale",
                        "u_uv_offset",
                        "u_tint",
                        "u_intensity",
                    ],
                )?,
                chains: Vec::new(),
                use_counter: 0,
            })
        }
    }

    /// Grow the chain so level 0 is `width`×`height` and each rung after it is
    /// half the last. Existing rungs of the right size are kept.
    unsafe fn scratch_chain(&mut self, gl: &glow::Context, key: ChainKey) -> Result<usize, String> {
        self.use_counter = self.use_counter.wrapping_add(1);
        let stamp = self.use_counter;

        if let Some(index) = self.chains.iter().position(|chain| chain.key == key) {
            self.chains[index].last_used = stamp;
            return Ok(index);
        }

        if self.chains.len() >= MAX_SCRATCH_CHAINS {
            let evict = self
                .chains
                .iter()
                .enumerate()
                .min_by_key(|(_, chain)| chain.last_used)
                .map(|(index, _)| index)
                .expect("a full scratch cache cannot be empty");
            let old = self.chains.swap_remove(evict);
            unsafe { destroy_levels(gl, old.levels) };
        }

        let levels = unsafe { create_levels(gl, key)? };
        self.chains.push(ScratchChain {
            key,
            levels,
            last_used: stamp,
        });
        Ok(self.chains.len() - 1)
    }

    unsafe fn destroy(&mut self, gl: &glow::Context) {
        unsafe {
            for chain in self.chains.drain(..) {
                destroy_levels(gl, chain.levels);
            }
            gl.delete_program(self.bloom.program);
            gl.delete_program(self.glass.program);
            gl.delete_program(self.up.program);
            gl.delete_program(self.down.program);
            gl.delete_vertex_array(self.vao);
        }
    }

    unsafe fn run(
        &mut self,
        gl: &glow::Context,
        info: &PaintCallbackInfo,
        job: Job,
    ) -> Result<(), String> {
        // Attribute any stale error to the code that ran before this callback.
        // The custom pass is checked again below so a driver-side failure can
        // restore the painted fallback instead of leaving a transparent hole.
        while unsafe { gl.get_error() } != glow::NO_ERROR {}

        let viewport = info.viewport_in_pixels();
        let (screen_w, screen_h) = (info.screen_size_px[0] as i32, info.screen_size_px[1] as i32);
        let (rect_w, rect_h) = (viewport.width_px, viewport.height_px);
        if rect_w < 2 || rect_h < 2 {
            return Ok(());
        }

        // The region actually sampled: the rectangle, grown so the blur has real
        // pixels to reach for, then clipped to the screen it is being read from.
        let pad = job.padding();
        let left = (viewport.left_px - pad).max(0);
        let bottom = (viewport.from_bottom_px - pad).max(0);
        let right = (viewport.left_px + rect_w + pad).min(screen_w);
        let top = (viewport.from_bottom_px + rect_h + pad).min(screen_h);
        let (src_w, src_h) = (right - left, top - bottom);
        if src_w < 4 || src_h < 4 {
            return Ok(());
        }

        // Whatever egui is drawing into. Always the default framebuffer today,
        // but read back rather than assumed so an intermediate target upstream
        // would not silently blur the wrong thing. Read before `ensure`, which
        // binds its own framebuffers while it builds the chain — read after it,
        // this captured one of the scratch targets, the composite went into a
        // texture nobody presents, and the binding leaked into the rest of the
        // frame: egui painted everything after this callback into it too.
        let previous = unsafe { gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) };
        let previous = NonZeroU32::new(previous as u32).map(glow::NativeFramebuffer);

        let key = ChainKey {
            width: src_w,
            height: src_h,
            count: job.levels() as usize + 1,
        };
        let chain = unsafe { self.scratch_chain(gl, key) };
        // Chain creation binds its targets. Always restore the destination,
        // including on an allocation or completeness failure.
        unsafe { gl.bind_framebuffer(glow::FRAMEBUFFER, previous) };
        let chain = chain?;
        let levels = &self.chains[chain].levels;

        unsafe {
            // --- capture ---------------------------------------------------
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, previous);
            gl.bind_texture(glow::TEXTURE_2D, Some(levels[0].texture));
            gl.copy_tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, left, bottom, src_w, src_h);

            // The intermediate passes own the whole of their target and must not
            // be clipped by the scissor egui set for this primitive.
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);

            // --- down ------------------------------------------------------
            gl.use_program(Some(self.down.program));
            gl.uniform_1_i32(self.down.at("u_tex"), 0);
            for i in 1..levels.len() {
                let (source, target) = (&levels[i - 1], &levels[i]);
                // Only the first pass extracts, and only for a bloom: by the
                // second rung the bright pixels have already been isolated.
                let (bright, threshold) = match (job, i) {
                    (Job::Bloom(b), 1) => (1.0, b.threshold),
                    _ => (0.0, 0.0),
                };
                gl.uniform_1_f32(self.down.at("u_bright"), bright);
                gl.uniform_1_f32(self.down.at("u_threshold"), threshold);
                gl.uniform_2_f32(
                    self.down.at("u_texel"),
                    1.0 / source.width as f32,
                    1.0 / source.height as f32,
                );
                blit(gl, source, target);
            }

            // --- up --------------------------------------------------------
            gl.use_program(Some(self.up.program));
            gl.uniform_1_i32(self.up.at("u_tex"), 0);
            for i in (2..levels.len()).rev() {
                let (source, target) = (&levels[i], &levels[i - 1]);
                gl.uniform_2_f32(
                    self.up.at("u_texel"),
                    1.0 / source.width as f32,
                    1.0 / source.height as f32,
                );
                blit(gl, source, target);
            }

            // --- composite -------------------------------------------------
            gl.bind_framebuffer(glow::FRAMEBUFFER, previous);
            gl.viewport(viewport.left_px, viewport.from_bottom_px, rect_w, rect_h);
            gl.enable(glow::SCISSOR_TEST);
            gl.enable(glow::BLEND);
            gl.bind_texture(glow::TEXTURE_2D, Some(levels[1].texture));

            // The blur chain covers the padded region; this rectangle is the
            // window into it that the caller actually asked for.
            let uv_scale = (rect_w as f32 / src_w as f32, rect_h as f32 / src_h as f32);
            let uv_offset = (
                (viewport.left_px - left) as f32 / src_w as f32,
                (viewport.from_bottom_px - bottom) as f32 / src_h as f32,
            );

            match job {
                Job::Glass(g) => {
                    let s = &self.glass;
                    gl.use_program(Some(s.program));
                    gl.uniform_1_i32(s.at("u_tex"), 0);
                    gl.uniform_2_f32(s.at("u_uv_scale"), uv_scale.0, uv_scale.1);
                    gl.uniform_2_f32(s.at("u_uv_offset"), uv_offset.0, uv_offset.1);
                    gl.uniform_2_f32(s.at("u_size"), rect_w as f32, rect_h as f32);
                    gl.uniform_1_f32(
                        s.at("u_radius"),
                        (g.corner_radius * info.pixels_per_point)
                            .min(rect_w.min(rect_h) as f32 * 0.5),
                    );
                    let [r, gr, b, a] = g.tint.to_srgba_unmultiplied();
                    gl.uniform_4_f32(
                        s.at("u_tint"),
                        r as f32 / 255.0,
                        gr as f32 / 255.0,
                        b as f32 / 255.0,
                        a as f32 / 255.0,
                    );
                    gl.uniform_4_f32(
                        s.at("u_mat_a"),
                        g.top_reflection,
                        g.edge_reflection,
                        g.inner_highlight,
                        g.bottom_shadow,
                    );
                    gl.uniform_4_f32(
                        s.at("u_mat_b"),
                        g.specular_width * info.pixels_per_point,
                        g.band,
                        g.rose,
                        g.opacity.clamp(0.0, 1.0),
                    );
                    gl.uniform_1_f32(s.at("u_noise"), g.noise);
                    match g.sheen {
                        // The shader works bottom-up; egui hands us top-down.
                        Some(pos) => {
                            gl.uniform_2_f32(
                                s.at("u_sheen"),
                                pos.x * info.pixels_per_point,
                                rect_h as f32 - pos.y * info.pixels_per_point,
                            );
                            gl.uniform_1_f32(s.at("u_sheen_amount"), g.sheen_amount);
                        }
                        None => {
                            gl.uniform_2_f32(s.at("u_sheen"), 0.0, 0.0);
                            gl.uniform_1_f32(s.at("u_sheen_amount"), 0.0);
                        }
                    }
                }
                Job::Bloom(b) => {
                    let s = &self.bloom;
                    gl.use_program(Some(s.program));
                    gl.uniform_1_i32(s.at("u_tex"), 0);
                    gl.uniform_2_f32(s.at("u_uv_scale"), uv_scale.0, uv_scale.1);
                    gl.uniform_2_f32(s.at("u_uv_offset"), uv_offset.0, uv_offset.1);
                    let [r, g, bl, _] = b.tint.to_srgba_unmultiplied();
                    gl.uniform_3_f32(
                        s.at("u_tint"),
                        r as f32 / 255.0,
                        g as f32 / 255.0,
                        bl as f32 / 255.0,
                    );
                    gl.uniform_1_f32(s.at("u_intensity"), b.intensity);
                }
            }
            gl.draw_arrays(glow::TRIANGLES, 0, 3);

            gl.bind_vertex_array(None);
        }
        let error = unsafe { gl.get_error() };
        if error != glow::NO_ERROR {
            return Err(format!("OpenGL effect pass failed (error 0x{error:04x})"));
        }
        Ok(())
    }
}

/// Allocate a complete render-target chain. Any failure destroys every object
/// created by this attempt, so callers can never observe a partial scale stack.
unsafe fn create_levels(gl: &glow::Context, key: ChainKey) -> Result<Vec<Level>, String> {
    let mut levels = Vec::with_capacity(key.count);
    for i in 0..key.count {
        let w = (key.width >> i).max(1);
        let h = (key.height >> i).max(1);
        let texture = match unsafe { gl.create_texture() } {
            Ok(texture) => texture,
            Err(err) => {
                unsafe { destroy_levels(gl, levels) };
                return Err(format!("could not create {w}x{h} blur texture: {err}"));
            }
        };

        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                w,
                h,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
        }

        let framebuffer = match unsafe { gl.create_framebuffer() } {
            Ok(framebuffer) => framebuffer,
            Err(err) => {
                unsafe {
                    gl.delete_texture(texture);
                    destroy_levels(gl, levels);
                }
                return Err(format!("could not create {w}x{h} blur framebuffer: {err}"));
            }
        };
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
        }
        let status = unsafe { gl.check_framebuffer_status(glow::FRAMEBUFFER) };
        if status != glow::FRAMEBUFFER_COMPLETE {
            unsafe {
                gl.delete_framebuffer(framebuffer);
                gl.delete_texture(texture);
                destroy_levels(gl, levels);
            }
            return Err(format!(
                "{w}x{h} blur framebuffer is incomplete (status 0x{status:04x})"
            ));
        }

        levels.push(Level {
            texture,
            framebuffer,
            width: w,
            height: h,
        });
    }
    Ok(levels)
}

unsafe fn destroy_levels(gl: &glow::Context, levels: Vec<Level>) {
    for level in levels {
        unsafe {
            gl.delete_framebuffer(level.framebuffer);
            gl.delete_texture(level.texture);
        }
    }
}

/// Render `source` over the whole of `target` with the program already bound.
unsafe fn blit(gl: &glow::Context, source: &Level, target: &Level) {
    unsafe {
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.framebuffer));
        gl.viewport(0, 0, target.width, target.height);
        gl.bind_texture(glow::TEXTURE_2D, Some(source.texture));
        gl.draw_arrays(glow::TRIANGLES, 0, 3);
    }
}

unsafe fn link(gl: &glow::Context, vertex: &str, fragment: &str) -> Result<glow::Program, String> {
    unsafe {
        let program = gl.create_program()?;
        let mut stages = Vec::with_capacity(2);
        for (kind, source) in [
            (glow::VERTEX_SHADER, vertex),
            (glow::FRAGMENT_SHADER, fragment),
        ] {
            let shader = match gl.create_shader(kind) {
                Ok(shader) => shader,
                Err(err) => {
                    for stage in stages {
                        gl.delete_shader(stage);
                    }
                    gl.delete_program(program);
                    return Err(err);
                }
            };
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let log = gl.get_shader_info_log(shader);
                gl.delete_shader(shader);
                for stage in stages {
                    gl.delete_shader(stage);
                }
                gl.delete_program(program);
                return Err(log);
            }
            gl.attach_shader(program, shader);
            stages.push(shader);
        }

        gl.link_program(program);
        let linked = gl.get_program_link_status(program);
        for stage in stages {
            gl.detach_shader(program, stage);
            gl.delete_shader(stage);
        }
        if !linked {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(log);
        }
        Ok(program)
    }
}
