use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use dpi::PhysicalSize;
use euclid::Scale;
use servo::{
    AuthenticationRequest, Cursor, DeviceIndependentPixel, DeviceIntRect, DevicePixel,
    EmbedderControl, InputEvent, LoadStatus, Opts, PermissionRequest, Preferences, RgbaImage,
    ServoBuilder, WebViewBuilder,
};
use url::Url;

use crate::delegate::{ContentFilterFn, CosmicServoDelegate, CosmicWebViewDelegate, SharedState};
use crate::dialog_handler::{DialogHandler, DialogMsg};
use crate::primitive::WebViewFrame;
use crate::rendering::{ActiveRenderingContext, read_frame_into_sink};
use crate::waker::IcedEventLoopWaker;

// The Servo engine is a process-wide singleton on the main thread.
// `Servo(Rc<ServoInner>)` is !Send so thread_local is the right mechanism.
// All ServoState instances share the same engine via cheap Rc clones.
thread_local! {
    static ENGINE: RefCell<Option<(servo::Servo, IcedEventLoopWaker, async_channel::Receiver<()>)>> =
        const { RefCell::new(None) };
    /// Profile directory passed to Servo so it can persist cookies, HSTS, etc.
    /// Set once before the first `ServoState::new`; ignored after the engine starts.
    static PROFILE_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Configure the directory where Servo stores persistent data (cookies, HSTS, auth caches).
///
/// Call this once before the first [`ServoState::new`]. If the engine has already
/// been initialised, this call has no effect.
pub fn set_profile_dir(dir: PathBuf) {
    PROFILE_DIR.with(|d| *d.borrow_mut() = Some(dir));
}

/// Flush Servo's session data to disk and tear down the shared engine.
///
/// Drop all [`ServoState`] instances **before** calling this so no other
/// `Servo` clone is alive. The final drop sends `CoreResourceMsg::Exit` and
/// waits for the resource thread to write `cookie_jar.json` and friends.
pub fn shutdown_engine() {
    ENGINE.with(|c| drop(c.borrow_mut().take()));
}

/// Drives a single browser tab: one WebView, one rendering context, one frame buffer.
///
/// All `ServoState` instances within a thread share a single [`servo::Servo`]
/// engine. Create the first state with [`ServoState::new`]; subsequent tabs use
/// the same engine automatically. Call [`ServoState::spin`] on **any** state
/// once per wake signal — it spins the shared engine and processes events for
/// all active WebViews.
///
/// # Example
///
/// ```no_run
/// use cosmic_webview::ServoState;
/// use dpi::PhysicalSize;
/// use url::Url;
///
/// let url = Url::parse("https://example.com").unwrap();
/// let (state, rx) = ServoState::new(url, PhysicalSize::new(1024_u32, 768), 1.0);
/// ```
pub struct ServoState {
    // Servo engine lives in the thread_local ENGINE; stored here only for
    // method calls that need a &Servo (e.g. resize sends messages via the engine).
    servo: servo::Servo,
    webview: servo::WebView,
    rendering_ctx: ActiveRenderingContext,
    /// Shared pixel buffer filled directly by the engine delegate; pass
    /// [`ServoState::frame`] to [`crate::WebViewWidget::new`].
    frame: WebViewFrame,
    shared: Rc<SharedState>,
    dialog_handler: DialogHandler,
}

impl ServoState {
    /// Creates a tab loaded with `initial_url`.
    ///
    /// On the first call, this initialises the shared Servo engine. Subsequent
    /// calls reuse the same engine — no panic on repeated construction.
    ///
    /// `scale_factor` is the HiDPI scale of the display (e.g. `1.0`, `1.5`, `2.0`).
    ///
    /// Returns the state and a channel receiver. Pass the receiver to a single
    /// iced `Subscription` shared across all tabs. Calling [`ServoState::spin`]
    /// on any state processes events for all WebViews.
    pub fn new(
        initial_url: Url,
        size: PhysicalSize<u32>,
        scale_factor: f32,
    ) -> (Self, async_channel::Receiver<()>) {
        let rendering_ctx = ActiveRenderingContext::create(size);
        Self::new_with_context(initial_url, rendering_ctx, scale_factor)
    }

    /// Creates a tab with a pre-built rendering context.
    ///
    /// Use this in tests to inject a `SoftwareRenderingContext` directly.
    pub(crate) fn new_with_context(
        initial_url: Url,
        rendering_ctx: ActiveRenderingContext,
        scale_factor: f32,
    ) -> (Self, async_channel::Receiver<()>) {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        // Get the shared engine, initialising it on the first call.
        let (servo, waker, rx) = ENGINE.with(|cell| {
            let mut borrow = cell.borrow_mut();
            if let Some((servo, waker, rx)) = borrow.as_ref() {
                (servo.clone(), waker.clone(), rx.clone())
            } else {
                let (waker, rx) = IcedEventLoopWaker::new();
                // GL-accelerated media must be registered before the engine is
                // built. The handles come from the first tab's GPU context; the
                // GStreamer pipelines create their own contexts shared with it,
                // so the per-frame readback/dmabuf paths are unaffected. Without
                // handles (software rendering) media decodes via CPU upload.
                // GL video decoding is opt-in (`gl-media` feature): Servo
                // currently composites GStreamer GL textures upside down.
                // See the feature comment in Cargo.toml.
                let gl_media = cfg!(feature = "gl-media")
                    .then(|| rendering_ctx.media_gl_handles())
                    .flatten();
                let use_gl_media = gl_media.is_some();
                if let Some((display, api, context)) = gl_media {
                    servo::Servo::initialize_gl_accelerated_media(display, api, context);
                }
                let opts = Opts {
                    config_dir: PROFILE_DIR.with(|d| d.borrow().clone()),
                    ..Default::default()
                };
                let preferences = Preferences {
                    // Decode video into GL textures shared with the rendering
                    // context instead of CPU frame upload. Only effective when
                    // initialize_gl_accelerated_media ran above.
                    media_glvideo_enabled: use_gl_media,
                    ..Default::default()
                };
                let servo = ServoBuilder::default()
                    .opts(opts)
                    .preferences(preferences)
                    .event_loop_waker(Box::new(waker.clone()))
                    .build();
                servo.setup_logging();
                servo.set_delegate(Rc::new(CosmicServoDelegate));
                *borrow = Some((servo.clone(), waker.clone(), rx.clone()));
                (servo, waker, rx)
            }
        });

        let shared = SharedState::new();
        let frame = WebViewFrame::new();
        let delegate = Rc::new(CosmicWebViewDelegate::new(
            rendering_ctx.as_rc(),
            shared.clone(),
            // The delegate fills the frame buffer in place — no intermediate copy.
            frame.sink(),
            // All tabs share the same waker sender — one wake covers all WebViews.
            waker.sender(),
        ));

        let webview = WebViewBuilder::new(&servo, rendering_ctx.as_rc())
            .url(initial_url)
            .hidpi_scale_factor(Scale::new(scale_factor))
            .delegate(delegate)
            .build();

        let state = Self {
            servo,
            webview,
            rendering_ctx,
            frame,
            shared,
            dialog_handler: DialogHandler::new(),
        };
        (state, rx)
    }

    /// Spins the shared Servo engine once, processing pending events for all WebViews.
    ///
    /// Call this on any `ServoState` — it does not matter which tab, since they
    /// all share the same underlying engine.
    pub fn spin(&self) {
        self.servo.spin_event_loop();
    }

    pub fn navigate(&self, url: Url) {
        self.webview.load(url);
    }

    pub fn resize(&self, size: PhysicalSize<u32>) {
        self.webview.resize(size);
        self.rendering_ctx.as_rc().resize(size);
    }

    /// Update the HiDPI scale factor at runtime (e.g. when the window moves to
    /// a display with a different DPI). Servo re-derives the CSS viewport size
    /// from the current physical size and the new scale factor.
    pub fn set_scale_factor(&self, factor: f32) {
        self.webview.set_hidpi_scale_factor(
            Scale::<f32, DeviceIndependentPixel, DevicePixel>::new(factor),
        );
    }

    pub fn go_back(&self) {
        self.webview.go_back(1);
    }

    pub fn go_forward(&self) {
        self.webview.go_forward(1);
    }

    pub fn reload(&self) {
        self.webview.reload();
    }

    /// Sets the page zoom factor (e.g. `1.0` = 100 %, `1.5` = 150 %).
    ///
    /// This adjusts the page's `devicePixelRatio` and reflows layout, unlike
    /// pinch zoom which only magnifies the viewport. Servo clamps the value to
    /// the inclusive range `[0.1, 10.0]`.
    pub fn set_page_zoom(&self, zoom: f32) {
        self.webview.set_page_zoom(zoom);
    }

    /// Returns the current page zoom factor (`1.0` = 100 %).
    pub fn page_zoom(&self) -> f32 {
        self.webview.page_zoom()
    }

    pub fn send_input(&self, event: InputEvent) {
        self.webview.notify_input_event(event);
    }

    /// Returns the shared frame buffer for this tab.
    ///
    /// Pass it to [`crate::WebViewWidget::new`] in `view()`. The engine fills
    /// it directly when new frames arrive — the application never needs to
    /// copy pixel data.
    pub fn frame(&self) -> &WebViewFrame {
        &self.frame
    }

    /// Controls whether this tab performs per-frame readback.
    ///
    /// Set `false` for background tabs: compositing and pixel readback are
    /// skipped entirely while hidden. On becoming visible again, the most
    /// recent frame is painted and read back immediately, so there is no
    /// stale-content flash when switching tabs.
    pub fn set_visible(&self, visible: bool) {
        self.shared.visible.set(visible);
        if visible && self.shared.frame_pending.replace(false) {
            self.webview.paint();
            if read_frame_into_sink(&self.rendering_ctx.as_rc(), &self.frame.sink()) {
                self.shared
                    .frame_generation
                    .set(self.shared.frame_generation.get().wrapping_add(1));
            }
        }
    }

    /// Captures the most recently rendered frame as a top-down image.
    ///
    /// This is an on-demand GPU readback intended for occasional use
    /// (screenshots, tab thumbnails) — it is **not** part of the per-frame
    /// render path and allocates a fresh image each call.
    pub fn screenshot(&self) -> Option<RgbaImage> {
        let rect = DeviceIntRect::from_size(self.webview.size().to_i32());
        self.rendering_ctx.as_rc().read_to_image(rect)
    }

    pub fn load_status(&self) -> LoadStatus {
        self.shared.load_status.get()
    }

    /// Install or remove a content filter for this tab.
    ///
    /// The closure receives `(request_url, resource_type, source_url)` and
    /// returns `true` when the request should be blocked. `resource_type` is
    /// the adblock resource-type string ("script", "image", "stylesheet", …).
    /// Pass `None` to disable filtering.
    pub fn set_content_filter(&self, filter: Option<ContentFilterFn>) {
        *self.shared.content_filter.lock().unwrap() = filter;
    }

    /// Returns a monotonically increasing counter that increments each time
    /// a new rendered frame is available.
    pub fn frame_generation(&self) -> u64 {
        self.shared.frame_generation.get()
    }

    /// Returns a clone of the shared wake receiver for use in an iced `Subscription`.
    ///
    /// All `ServoState` instances return the same underlying channel. Create one
    /// iced subscription for the whole application, not one per tab.
    pub fn waker_rx(&self) -> async_channel::Receiver<()> {
        ENGINE.with(|cell| {
            cell.borrow()
                .as_ref()
                .map(|(_, _, rx)| rx.clone())
                .expect("ENGINE is initialised before waker_rx is called")
        })
    }

    /// Takes the pending dialog or file-picker control, if any.
    pub fn take_dialog(&self) -> Option<EmbedderControl> {
        self.shared.pending_dialog.borrow_mut().take()
    }

    /// Route the pending dialog control to the built-in dialog handler.
    pub fn handle_dialog_control(
        &mut self,
        control: EmbedderControl,
    ) -> Option<libcosmic::iced::Task<DialogMsg>> {
        self.dialog_handler.accept(control)
    }

    /// Forward a [`DialogMsg`] produced by the built-in dialog widgets to the handler.
    pub fn update_dialog(&mut self, msg: DialogMsg) {
        self.dialog_handler.update(msg);
    }

    /// Build the current pending dialog element for `Application::dialog()`.
    pub fn dialog_view(&self) -> Option<libcosmic::Element<'_, DialogMsg>> {
        self.dialog_handler.view()
    }

    /// Takes the pending HTTP authentication request, if any.
    pub fn take_auth_request(&self) -> Option<AuthenticationRequest> {
        self.shared.pending_auth.borrow_mut().take()
    }

    /// Takes the pending permission request, if any.
    ///
    /// The embedder responds by calling [`PermissionRequest::allow`] or
    /// [`PermissionRequest::deny`]; dropping the request without responding
    /// sends the default (deny) reply.
    pub fn take_permission_request(&self) -> Option<PermissionRequest> {
        self.shared.pending_permission.borrow_mut().take()
    }

    /// Returns the current cursor shape as last reported by the page.
    pub fn cursor(&self) -> Cursor {
        self.shared.cursor.get()
    }

    /// Returns the URL of the hyperlink currently under the cursor, if any.
    ///
    /// The page reports this on hover; embedders use it to open a link in a new
    /// tab on middle-click or Ctrl+click.
    pub fn hovered_link(&self) -> Option<String> {
        self.shared.status_text.borrow().clone()
    }

    /// Returns `true` when the page has requested fullscreen mode.
    pub fn is_fullscreen(&self) -> bool {
        self.shared.is_fullscreen.get()
    }

    /// Returns the most recent URL reported by the page, if any.
    pub fn current_url(&self) -> Option<Url> {
        self.shared.current_url.borrow().clone()
    }

    /// Returns the most recent page title reported by the page, if any.
    pub fn title(&self) -> Option<String> {
        self.shared.title.borrow().clone()
    }

    /// Returns `true` when there is a previous entry to go back to.
    pub fn can_go_back(&self) -> bool {
        self.shared.can_go_back.get()
    }

    /// Returns `true` when there is a next entry to go forward to.
    pub fn can_go_forward(&self) -> bool {
        self.shared.can_go_forward.get()
    }

    /// Takes the crash info if a pipeline has panicked since the last call.
    pub fn take_crash(&self) -> Option<(String, Option<String>)> {
        self.shared.crashed.borrow_mut().take()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::rendering::ActiveRenderingContext;

    #[test]
    fn loads_about_blank() {
        let size = PhysicalSize::new(400_u32, 300);
        let url = Url::parse("about:blank").expect("valid url");
        // Use software rendering — matches Servo's own integration test suite.
        let ctx = ActiveRenderingContext::software(size);
        let (state, _rx) = ServoState::new_with_context(url, ctx, 1.0);

        let deadline = Instant::now() + Duration::from_secs(30);
        while state.load_status() != LoadStatus::Complete {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for about:blank; status={:?}",
                state.load_status()
            );
            state.spin();
            std::thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(state.load_status(), LoadStatus::Complete);
    }
}
