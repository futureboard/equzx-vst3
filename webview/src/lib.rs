use baseview::{
    Event, Size, Window, WindowEvent, WindowHandle, WindowInfo, WindowOpenOptions, WindowScalePolicy,
};
use nih_plug::prelude::{Editor, GuiContext, ParamSetter};
use serde_json::Value;
use std::{
    borrow::Cow,
    sync::{
        atomic::{AtomicU32, Ordering},
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
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,
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
        self.webview.set_bounds(wry::Rect {
            x: 0,
            y: 0,
            width: size.width,
            height: size.height,
        });
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
            // Added to the vendored copy.
            //
            // The webview is not DPI-aware on these platforms — WebView2 reports
            // a `devicePixelRatio` of 1 and renders CSS pixels as physical ones,
            // which is why `set_scale_factor` below refuses the host's scale
            // factor outright. Letting baseview apply the *system* scale anyway
            // made the two disagree by exactly that factor: at 150% the window
            // opened half again as large as the webview inside it, so a third of
            // it was bare background, and every size the UI asked for came back
            // 1.5x too big — a resize grip that overshot, and a persisted size
            // that grew by half on every reopen.
            //
            // macOS is left alone: WKWebView does handle scaling, so there the
            // system factor is the right one.
            #[cfg(target_os = "macos")]
            scale: WindowScalePolicy::SystemScaleFactor,
            #[cfg(not(target_os = "macos"))]
            scale: WindowScalePolicy::ScaleFactor(1.0),
            size: Size {
                width: self.width.load(Ordering::Relaxed) as f64,
                height: self.height.load(Ordering::Relaxed) as f64,
            },
            title: "Plug-in".to_owned(),
        };

        let width = self.width.clone();
        let height = self.height.clone();
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
            }
        });
        return Box::new(Instance { window_handle });
    }

    fn size(&self) -> (u32, u32) {
        (
            self.width.load(Ordering::Relaxed),
            self.height.load(Ordering::Relaxed),
        )
    }

    fn set_scale_factor(&self, _factor: f32) -> bool {
        // TODO: implement for Windows and Linux
        return false;
    }

    fn param_values_changed(&self) {}

    fn param_value_changed(&self, _id: &str, _normalized_value: f32) {}

    fn param_modulation_changed(&self, _id: &str, _modulation_offset: f32) {}
}
