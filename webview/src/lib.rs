use baseview::{
    Event, Size, Window, WindowEvent, WindowHandle, WindowInfo, WindowOpenOptions, WindowScalePolicy,
};
use nih_plug::prelude::{AtomicF32, Editor, GuiContext, ParamSetter};
use serde_json::Value;
use std::{
    borrow::Cow,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc,
    },
};
use wry::{
    http::{Request, Response},
    WebContext, WebView, WebViewBuilder,
};

use crossbeam::channel::{unbounded, Receiver};

pub use wry::http;
// Added to the vendored copy: a custom protocol handler has to name wry's own
// error type in its signature, and the upstream crate doesn't re-export it.
pub use wry::{Error as WryError, Result as WryResult};

pub use baseview::{DropData, DropEffect, EventStatus, MouseEvent};
pub use keyboard_types::*;

type EventLoopHandler = dyn Fn(&WindowHandler, ParamSetter, &mut Window) + Send + Sync;
type KeyboardHandler = dyn Fn(KeyboardEvent) -> bool + Send + Sync;
type MouseHandler = dyn Fn(MouseEvent) -> EventStatus + Send + Sync;
type CustomProtocolHandler =
    dyn Fn(&Request<Vec<u8>>) -> wry::Result<Response<Cow<'static, [u8]>>> + Send + Sync;

pub struct WebViewEditor {
    source: Arc<HTMLSource>,
    /// Logical pixels throughout — see [`Editor::size`] for the one place that
    /// is converted, and why.
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,
    /// Display scale the window is actually being drawn at.
    scale: Arc<AtomicF32>,
    /// Whether the host told us that scale itself. If it did, the wrapper is
    /// already multiplying our reported size by it and we must not do so too.
    host_scales: Arc<AtomicBool>,
    event_loop_handler: Arc<EventLoopHandler>,
    keyboard_handler: Arc<KeyboardHandler>,
    mouse_handler: Arc<MouseHandler>,
    custom_protocol: Option<(String, Arc<CustomProtocolHandler>)>,
    developer_mode: bool,
    background_color: (u8, u8, u8, u8),
}

pub enum HTMLSource {
    String(&'static str),
    URL(&'static str),
}

impl WebViewEditor {
    pub fn new(source: HTMLSource, size: (u32, u32)) -> Self {
        let width = Arc::new(AtomicU32::new(size.0));
        let height = Arc::new(AtomicU32::new(size.1));
        Self {
            source: Arc::new(source),
            width,
            height,
            scale: Arc::new(AtomicF32::new(1.0)),
            host_scales: Arc::new(AtomicBool::new(false)),
            developer_mode: false,
            background_color: (255, 255, 255, 255),
            event_loop_handler: Arc::new(|_, _, _| {}),
            keyboard_handler: Arc::new(|_| false),
            mouse_handler: Arc::new(|_| EventStatus::Ignored),
            custom_protocol: None,
        }
    }

    pub fn with_background_color(mut self, background_color: (u8, u8, u8, u8)) -> Self {
        self.background_color = background_color;
        self
    }

    pub fn with_custom_protocol<F>(mut self, name: String, handler: F) -> Self
    where
        F: Fn(&Request<Vec<u8>>) -> wry::Result<Response<Cow<'static, [u8]>>>
            + 'static
            + Send
            + Sync,
    {
        self.custom_protocol = Some((name, Arc::new(handler)));
        self
    }

    pub fn with_event_loop<F>(mut self, handler: F) -> Self
    where
        F: Fn(&WindowHandler, ParamSetter, &mut baseview::Window) + 'static + Send + Sync,
    {
        self.event_loop_handler = Arc::new(handler);
        self
    }

    pub fn with_developer_mode(mut self, mode: bool) -> Self {
        self.developer_mode = mode;
        self
    }

    pub fn with_keyboard_handler<F>(mut self, handler: F) -> Self
    where
        F: Fn(KeyboardEvent) -> bool + Send + Sync + 'static,
    {
        self.keyboard_handler = Arc::new(handler);
        self
    }

    pub fn with_mouse_handler<F>(mut self, handler: F) -> Self
    where
        F: Fn(MouseEvent) -> EventStatus + Send + Sync + 'static,
    {
        self.mouse_handler = Arc::new(handler);
        self
    }
}

pub struct WindowHandler {
    context: Arc<dyn GuiContext>,
    event_loop_handler: Arc<EventLoopHandler>,
    keyboard_handler: Arc<KeyboardHandler>,
    mouse_handler: Arc<MouseHandler>,
    webview: WebView,
    events_receiver: Receiver<Value>,
    pub width: Arc<AtomicU32>,
    pub height: Arc<AtomicU32>,
    scale: Arc<AtomicF32>,
    /// The size [`Self::resize`] last asked for, packed, or zero once it has
    /// been answered. Only a size that answers a request can be said to have
    /// overridden one — a window settling on open, or a host resizing its own
    /// frame, is not the host disagreeing with us.
    pending: AtomicU64,
    /// Last override that was logged, so a drag the host is clamping reports
    /// once rather than once a frame.
    last_mismatch: AtomicU64,
}

impl WindowHandler {
    /// Ask for a new editor size, in logical pixels.
    ///
    /// Everything here is asynchronous, and that is the whole point. This is
    /// called from the event-loop callback, which runs inside baseview's window
    /// procedure with its handler `RefCell` mutably borrowed — so anything that
    /// synchronously provokes a window message lands back in that same window
    /// procedure, hits the same `RefCell`, and panics. The panic happens inside
    /// an `extern "system"` function that cannot unwind, so the process aborts
    /// rather than merely losing the editor: a hard crash, taking the host with
    /// it.
    ///
    /// That is why `baseview::Window::resize` queues a deferred task instead of
    /// calling `SetWindowPos` directly, and why the webview's own bounds are
    /// *not* set here — see [`Self::stretch_webview`], which does it once the
    /// window has actually arrived at its new size.
    pub fn resize(&self, window: &mut baseview::Window, width: u32, height: u32) {
        // A drag sends one of these a frame; without this, every one of them
        // would be a round trip to the host asking for the size it already has.
        if self.width.load(Ordering::Relaxed) == width
            && self.height.load(Ordering::Relaxed) == height
        {
            return;
        }
        self.width.store(width, Ordering::Relaxed);
        self.height.store(height, Ordering::Relaxed);
        self.pending
            .store(((width as u64) << 32) | height as u64, Ordering::Relaxed);

        // Both of these are deferred: every wrapper posts `request_resize` to a
        // queue, and baseview runs the resize at the end of the current window
        // procedure call.
        self.context.request_resize();
        window.resize(Size {
            width: width as f64,
            height: height as f64,
        });
    }

    /// Stretch the webview over the window, at the size the window now is.
    ///
    /// Safe to call from a window-message handler precisely because it does not
    /// change the window: the `WM_SIZE` this may bounce back carries a size
    /// baseview has already recorded, and baseview drops such an event before it
    /// touches the handler it is currently inside.
    ///
    /// Physical pixels, because that is what wry's bounds are in — the previous
    /// code passed logical size to both this and the window, which came apart on
    /// any display not at 100%.
    fn stretch_webview(&self, info: &WindowInfo) {
        let size = info.physical_size();

        // The DPI fix, in one line.
        //
        // WebView2 renders a CSS pixel as a physical one — `devicePixelRatio`
        // is 1 no matter what the display is doing — so on a scaled display the
        // page laid out at physical size while everything around it worked in
        // logical pixels. Every size then disagreed with every other by exactly
        // the scale factor: ask a DPI-aware host for a 1400x900 editor at 150%
        // and roughly 900x600 of UI is what comes back.
        //
        // Zooming the webview by the display's scale makes a CSS pixel and a
        // logical pixel the same size again, which is the assumption the rest of
        // this file, the UI, and the host all already shared.
        let scale = info.scale() as f32;
        if (self.scale.load(Ordering::Relaxed) - scale).abs() > f32::EPSILON {
            self.scale.store(scale, Ordering::Relaxed);
            self.webview.zoom(scale as f64);
        }

        // Whoever owns the frame has the last word on how big it is. A host may
        // refuse a size outright, clamp it to the display, or apply a scale of
        // its own, and when it does, the size we asked for is fiction: reporting
        // it back through `Editor::size` would have us arguing with the host on
        // every subsequent resize, and would persist a size the user never got.
        // Adopt what actually happened instead — in logical pixels, which is
        // what everything outside this function deals in.
        let logical = info.logical_size();
        let (got_w, got_h) = (logical.width.round() as u32, logical.height.round() as u32);
        self.width.store(got_w, Ordering::Relaxed);
        self.height.store(got_h, Ordering::Relaxed);

        let pending = self.pending.swap(0, Ordering::Relaxed);
        let asked = ((pending >> 32) as u32, pending as u32);
        if pending != 0 && asked != (got_w, got_h) {
            // Only when it changes, or a drag would log sixty times a second.
            let seen = ((got_w as u64) << 32) | got_h as u64;
            if self.last_mismatch.swap(seen, Ordering::Relaxed) != seen {
                nih_plug::nih_log!(
                    "nih_plug_webview: host sized the editor to {}x{} logical \
                     ({}x{} physical at {}x scale), {}x{} was requested",
                    got_w,
                    got_h,
                    size.width,
                    size.height,
                    scale,
                    asked.0,
                    asked.1
                );
            }
        }

        self.webview.set_bounds(wry::Rect {
            x: 0,
            y: 0,
            width: size.width,
            height: size.height,
        });
    }

    /// The display scale the editor is being drawn at, 1.0 at 100%.
    ///
    /// The UI needs this to reason about the screen: `window.screen` reports
    /// device pixels whatever the webview's zoom is, while every size the UI
    /// works in is logical.
    pub fn scale(&self) -> f32 {
        self.scale.load(Ordering::Relaxed)
    }

    pub fn send_json(&self, json: Value) {
        let json_str = json.to_string();
        let json_str_quoted =
            serde_json::to_string(&json_str).expect("Should not fail: the value is always string");
        // A failed script evaluation is a message the UI didn't get — a dropped
        // frame of meter movement, at worst. It used to be an unwrap, which made
        // it the end of the host process instead.
        if let Err(e) = self
            .webview
            .evaluate_script(&format!("onPluginMessageInternal({});", json_str_quoted))
        {
            nih_plug::nih_debug_assert_failure!("nih_plug_webview: evaluate_script failed: {}", e);
        }
    }

    pub fn next_event(&self) -> Result<Value, crossbeam::channel::TryRecvError> {
        self.events_receiver.try_recv()
    }
}

/// Dispatch the messages nobody else on this thread will.
///
/// Added to the vendored copy, and the difference between a working editor and a
/// blank one under the standalone wrapper.
///
/// WebView2 does its asynchronous work — completing a navigation, answering a
/// resource request, running a script — through window messages, and most of
/// them go to hidden windows the runtime owns rather than to anything in our
/// window tree. baseview's blocking loop calls `GetMessageW` with an HWND
/// filter, which by definition only returns messages for that window and its
/// children, so those messages are never dispatched: the webview is created,
/// reports the right URL, and then sits on a blank document forever.
///
/// So each frame, drain the messages that are *not* ours and dispatch them.
/// Anything belonging to our own window tree is deliberately left in the queue —
/// baseview owns those, and dispatching one from inside its own frame callback
/// would re-enter its handler. A host with a normal unfiltered loop has already
/// drained everything by the time this runs, so there it costs a single
/// `PeekMessage` that finds nothing.
#[cfg(target_os = "windows")]
fn pump_foreign_messages(window: &baseview::Window) {
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, IsChild, PeekMessageW, TranslateMessage, MSG, PM_NOREMOVE, PM_REMOVE,
    };

    let ours = match window.raw_window_handle() {
        RawWindowHandle::Win32(handle) => HWND(handle.hwnd as _),
        _ => return,
    };
    // -1 is the documented "thread messages only" filter.
    const THREAD_MESSAGES: HWND = HWND(-1isize as _);

    // Bounded so a message storm can't hold the frame callback hostage.
    for _ in 0..64 {
        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, HWND(0), 0, 0, PM_NOREMOVE) };
        if !found.as_bool() {
            break;
        }

        let target = msg.hwnd;
        let is_ours =
            target.0 != 0 && (target == ours || unsafe { IsChild(ours, target) }.as_bool());
        if is_ours {
            // Stop rather than skip: peeking past it would mean removing it, and
            // it isn't ours to remove.
            break;
        }

        // Take exactly the message just inspected. A thread message has no window
        // to filter on, so it needs the thread-message filter instead.
        let filter = if target.0 == 0 {
            THREAD_MESSAGES
        } else {
            target
        };
        let taken = unsafe { PeekMessageW(&mut msg, filter, msg.message, msg.message, PM_REMOVE) };
        if !taken.as_bool() {
            break;
        }

        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

impl baseview::WindowHandler for WindowHandler {
    fn on_frame(&mut self, window: &mut baseview::Window) {
        #[cfg(target_os = "windows")]
        pump_foreign_messages(window);

        let setter = ParamSetter::new(&*self.context);
        (self.event_loop_handler)(&self, setter, window);
    }

    fn on_event(&mut self, _window: &mut baseview::Window, event: Event) -> EventStatus {
        match event {
            Event::Keyboard(event) => {
                if (self.keyboard_handler)(event) {
                    EventStatus::Captured
                } else {
                    EventStatus::Ignored
                }
            }
            Event::Mouse(mouse_event) => (self.mouse_handler)(mouse_event),
            // The window has settled at a new size — ours to follow, whether the
            // request came from the plugin or from the host dragging its own
            // frame. The latter never used to reach the webview at all.
            Event::Window(WindowEvent::Resized(info)) => {
                self.stretch_webview(&info);
                EventStatus::Ignored
            }
            _ => EventStatus::Ignored,
        }
    }
}

struct Instance {
    window_handle: WindowHandle,
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.window_handle.close();
    }
}

unsafe impl Send for Instance {}

impl Editor for WebViewEditor {
    fn spawn(
        &self,
        parent: nih_plug::prelude::ParentWindowHandle,
        context: Arc<dyn GuiContext>,
    ) -> Box<dyn std::any::Any + Send> {
        let options = WindowOpenOptions {
            // Sizes here and everywhere else in this file are logical pixels;
            // baseview turns them into physical ones using the display's scale,
            // and [`WindowHandler::stretch_webview`] zooms the webview to match
            // so a CSS pixel and a logical pixel stay the same thing.
            scale: WindowScalePolicy::SystemScaleFactor,
            size: Size {
                width: self.width.load(Ordering::Relaxed) as f64,
                height: self.height.load(Ordering::Relaxed) as f64,
            },
            title: "Plug-in".to_owned(),
        };

        let width = self.width.clone();
        let height = self.height.clone();
        let scale = self.scale.clone();
        let developer_mode = self.developer_mode;
        let source = self.source.clone();
        let background_color = self.background_color;
        let custom_protocol = self.custom_protocol.clone();
        let event_loop_handler = self.event_loop_handler.clone();
        let keyboard_handler = self.keyboard_handler.clone();
        let mouse_handler = self.mouse_handler.clone();

        let window_handle = baseview::Window::open_parented(&parent, options, move |window| {
            let (events_sender, events_receiver) = unbounded();

            let mut web_context = WebContext::new(Some(std::env::temp_dir()));

            let mut webview_builder = WebViewBuilder::new_as_child(window)
                .with_bounds(wry::Rect {
                    x: 0,
                    y: 0,
                    width: width.load(Ordering::Relaxed) as u32,
                    height: height.load(Ordering::Relaxed) as u32,
                })
                .with_accept_first_mouse(true)
                .with_devtools(developer_mode)
                .with_web_context(&mut web_context)
                .with_initialization_script(include_str!("script.js"))
                .with_ipc_handler(move |msg: String| {
                    if let Ok(json_value) = serde_json::from_str(&msg) {
                        let _ = events_sender.send(json_value);
                    } else {
                        panic!("Invalid JSON from web view: {}.", msg);
                    }
                })
                .with_background_color(background_color);

            if let Some(custom_protocol) = custom_protocol.as_ref() {
                let handler = custom_protocol.1.clone();
                webview_builder = webview_builder
                    .with_custom_protocol(custom_protocol.0.to_owned(), move |request| {
                        handler(&request).unwrap()
                    });
            }

            let webview = match source.as_ref() {
                HTMLSource::String(html_str) => webview_builder.with_html(*html_str),
                HTMLSource::URL(url) => webview_builder.with_url(*url),
            }
            .unwrap()
            .build();

            // Added to the vendored copy: a failure here used to panic inside the
            // window thread, which leaves a live but permanently blank editor and
            // no clue why. Log it on the way past.
            if let Err(e) = &webview {
                nih_plug::nih_log!("nih_plug_webview: webview failed: {}", e);
            }

            WindowHandler {
                context,
                event_loop_handler,
                webview: webview.unwrap_or_else(|e| panic!("Failed to construct webview. {}", e)),
                events_receiver,
                keyboard_handler,
                mouse_handler,
                width,
                height,
                scale,
                pending: AtomicU64::new(0),
                last_mismatch: AtomicU64::new(0),
            }
        });
        return Box::new(Instance { window_handle });
    }

    /// The editor's size, in whichever units this host is going to read it in.
    ///
    /// A wrapper multiplies whatever comes back by the scale factor the plugin
    /// accepted — so when the host told us its scale, logical is the right
    /// answer and the wrapper does the rest. Hosts that never call
    /// `set_scale_factor` leave that multiplier at one (nih-plug's own notes
    /// name Ableton Live), and reporting logical to one of those asks for a
    /// window a scale factor too small. So do the multiplication here instead,
    /// exactly when nobody else is going to.
    fn size(&self) -> (u32, u32) {
        let (width, height) = (
            self.width.load(Ordering::Relaxed),
            self.height.load(Ordering::Relaxed),
        );
        if self.host_scales.load(Ordering::Relaxed) {
            return (width, height);
        }
        let scale = self.scale.load(Ordering::Relaxed);
        (
            (width as f32 * scale).round() as u32,
            (height as f32 * scale).round() as u32,
        )
    }

    /// Accept the host's scale factor.
    ///
    /// Refusing it, as this used to, does not make the problem go away — it
    /// just moves the scaling somewhere the plugin has no say over, and leaves
    /// the wrapper reporting sizes a scale factor out from the window that
    /// actually exists. The webview is zoomed to match in `stretch_webview`.
    ///
    /// macOS is the exception: there the OS scales the window and every size in
    /// the API is already logical, so there is nothing to apply.
    fn set_scale_factor(&self, factor: f32) -> bool {
        if cfg!(target_os = "macos") || !factor.is_finite() || factor <= 0.0 {
            return false;
        }
        self.scale.store(factor, Ordering::Relaxed);
        self.host_scales.store(true, Ordering::Relaxed);
        true
    }

    fn param_values_changed(&self) {}

    fn param_value_changed(&self, _id: &str, _normalized_value: f32) {}

    fn param_modulation_changed(&self, _id: &str, _modulation_offset: f32) {}
}
