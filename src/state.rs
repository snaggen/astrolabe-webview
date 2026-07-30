use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use dpi::PhysicalSize;
use euclid::Scale;
use servo::{
    AuthenticationRequest, BluetoothDeviceSelectionRequest, Cursor, DeviceIndependentPixel,
    DeviceIntRect, DevicePixel, EmbedderControl, InputEvent, JSValue, JavaScriptEvaluationError,
    LoadStatus, Opts, PermissionRequest, PrefValue, Preferences, RgbaImage, ServoBuilder, SiteData,
    StorageType, Theme, WebViewBuilder,
};
use url::Url;

use crate::delegate::{ContentFilterFn, CosmicServoDelegate, CosmicWebViewDelegate, SharedState};
use crate::dialog_handler::{DialogHandler, DialogMsg};
use crate::find_bar::FindResult;
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
    /// Whether `getUserMedia` (camera & microphone) is enabled. Read when the
    /// engine starts and updatable at runtime via [`set_camera_microphone_enabled`].
    static WEBRTC_ENABLED: Cell<bool> = const { Cell::new(false) };
    /// Default web-content fonts. Read when the engine starts and updatable at
    /// runtime via [`set_web_fonts`].
    static WEB_FONTS: RefCell<Option<WebFonts>> = const { RefCell::new(None) };
    /// Port for the Servo remote devtools server, or `None` if disabled.
    /// Read once at engine startup — changing it afterwards has no effect.
    static DEVTOOLS_PORT: Cell<Option<u16>> = const { Cell::new(None) };
}

/// Default fonts for web content — the page's serif, sans-serif, and monospace
/// families and base sizes, like a browser's font settings. An empty family
/// string uses Servo's built-in default for that generic family.
#[derive(Clone, Debug)]
pub struct WebFonts {
    pub serif: String,
    pub sans_serif: String,
    pub monospace: String,
    pub default_size: i64,
    pub monospace_size: i64,
}

impl Default for WebFonts {
    fn default() -> Self {
        // Mirrors Servo's own pref defaults.
        Self {
            serif: String::new(),
            sans_serif: String::new(),
            monospace: String::new(),
            default_size: 16,
            monospace_size: 13,
        }
    }
}

/// How web content's `prefers-color-scheme` should be chosen.
///
/// Pass this to [`ServoState::set_theme_preference`]. `FollowSystem` resolves to
/// the current COSMIC desktop light/dark setting; `Light` and `Dark` force a
/// fixed appearance regardless of the desktop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemePreference {
    /// Follow the COSMIC desktop light/dark setting (the default).
    #[default]
    FollowSystem,
    /// Always report a light theme to web content.
    Light,
    /// Always report a dark theme to web content.
    Dark,
}

impl ThemePreference {
    /// Resolve to the concrete [`Theme`] to report to web content.
    ///
    /// `FollowSystem` reads the current COSMIC desktop theme via
    /// [`libcosmic::theme::active`]; `Light`/`Dark` map directly.
    pub fn resolve(self) -> Theme {
        match self {
            ThemePreference::Light => Theme::Light,
            ThemePreference::Dark => Theme::Dark,
            ThemePreference::FollowSystem => {
                if libcosmic::theme::active().cosmic().is_dark {
                    Theme::Dark
                } else {
                    Theme::Light
                }
            }
        }
    }
}

/// Configure the default fonts used to render web content.
///
/// Call before the first [`ServoState::new`] to apply from the first page load;
/// calling later updates the running engine and takes effect on the next load.
pub fn set_web_fonts(fonts: WebFonts) {
    apply_web_fonts(&fonts);
    WEB_FONTS.with(|f| *f.borrow_mut() = Some(fonts));
}

/// Push web-font preferences into the running engine, if any.
fn apply_web_fonts(fonts: &WebFonts) {
    with_engine(|servo| {
        servo.set_preference("fonts_serif", PrefValue::Str(fonts.serif.clone()));
        servo.set_preference("fonts_sans_serif", PrefValue::Str(fonts.sans_serif.clone()));
        servo.set_preference("fonts_monospace", PrefValue::Str(fonts.monospace.clone()));
        servo.set_preference("fonts_default_size", PrefValue::Int(fonts.default_size));
        servo.set_preference(
            "fonts_default_monospace_size",
            PrefValue::Int(fonts.monospace_size),
        );
    });
}

/// Enable the Servo remote devtools server on the given port.
///
/// Call once before the first [`ServoState::new`]; the engine is initialised
/// only once per process, so later calls have no effect.
///
/// The server implements the Firefox Remote Debugging Protocol. Once running,
/// connect from Firefox via *Remote Debugging → Connect to localhost:PORT*.
/// The default port used by servoshell is 7000; a common alternative is 9222.
pub fn set_devtools_enabled(port: u16) {
    DEVTOOLS_PORT.with(|c| c.set(Some(port)));
}

/// Configure the directory where Servo stores persistent data (cookies, HSTS, auth caches).
///
/// Call this once before the first [`ServoState::new`]. If the engine has already
/// been initialised, this call has no effect.
pub fn set_profile_dir(dir: PathBuf) {
    PROFILE_DIR.with(|d| *d.borrow_mut() = Some(dir));
}

/// Enable or disable `getUserMedia` (camera & microphone access). Off by default.
///
/// Servo grants media access without a per-request permission prompt, so this is
/// an all-or-nothing switch that should be exposed to the user as an explicit
/// opt-in. Call before the first [`ServoState::new`] to apply from the first page
/// load; calling afterwards updates the running engine and takes effect on the
/// next page load.
pub fn set_camera_microphone_enabled(enabled: bool) {
    WEBRTC_ENABLED.with(|c| c.set(enabled));
    with_engine(|servo| servo.set_preference("dom_webrtc_enabled", PrefValue::Bool(enabled)));
}

/// Flush Servo's session data to disk and tear down the shared engine.
///
/// Drop all [`ServoState`] instances **before** calling this so no other
/// `Servo` clone is alive. The final drop sends `CoreResourceMsg::Exit` and
/// waits for the resource thread to write `cookie_jar.json` and friends.
pub fn shutdown_engine() {
    ENGINE.with(|c| drop(c.borrow_mut().take()));
}

/// Run `f` with the shared engine if it has been initialised; otherwise do nothing.
fn with_engine<R>(f: impl FnOnce(&servo::Servo) -> R) -> Option<R> {
    ENGINE.with(|c| c.borrow().as_ref().map(|(servo, ..)| f(servo)))
}

/// Site data categories that can be listed and cleared: cookies, `localStorage`,
/// and `sessionStorage`. Servo's engine does not expose HTTP cache or IndexedDB
/// clearing, so those are intentionally absent.
pub(crate) fn all_storage_types() -> StorageType {
    StorageType::Cookies | StorageType::Local | StorageType::Session
}

/// List the sites that currently hold cookies or web storage, with the data
/// categories each one uses. Returns an empty list if the engine is not running.
pub fn list_site_data() -> Vec<SiteData> {
    with_engine(|servo| servo.site_data_manager().site_data(all_storage_types()))
        .unwrap_or_default()
}

/// Delete cookies and web storage for the given registrable domains.
///
/// The in-memory cookie jar is updated immediately; the (now reduced) jar is
/// written to disk on the next [`shutdown_engine`].
pub fn clear_site_data(sites: &[&str]) {
    with_engine(|servo| {
        servo
            .site_data_manager()
            .clear_site_data(sites, all_storage_types());
    });
}

/// Delete all cookies from the engine's cookie jar.
///
/// Web storage is left untouched — call [`clear_site_data`] for that. The empty
/// jar is persisted on the next [`shutdown_engine`].
pub fn clear_all_cookies() {
    with_engine(|servo| servo.site_data_manager().clear_cookies(None));
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
                let web_fonts = WEB_FONTS.with(|f| f.borrow().clone()).unwrap_or_default();
                let devtools_port = DEVTOOLS_PORT.with(Cell::get);
                let preferences = Preferences {
                    // Decode video into GL textures shared with the rendering
                    // context instead of CPU frame upload. Only effective when
                    // initialize_gl_accelerated_media ran above.
                    media_glvideo_enabled: use_gl_media,
                    // Permission-gated DOM APIs are off by default in Servo. Only
                    // enable those that actually route through the embedder's
                    // permission prompt (`EmbedderMsg::PromptPermission`):
                    // notifications, persistent storage, and `navigator
                    // .permissions.request()`. Other features are deliberately
                    // left off: Servo's getUserMedia grants camera/microphone
                    // WITHOUT any prompt (a privacy hole), geolocation's prompt
                    // step is unimplemented (it hangs), and Bluetooth is gated by
                    // a Cargo feature that is not built in. Re-enable each here
                    // once Servo wires it to the permission prompt.
                    dom_notification_enabled: true,
                    dom_permissions_enabled: true,
                    dom_storage_manager_api_enabled: true,
                    // Bluetooth routes through the embedder's device-selection
                    // dialog (built with the `bluetooth` Cargo feature).
                    dom_bluetooth_enabled: true,
                    // getUserMedia is an explicit opt-in (auto-grants, no prompt).
                    dom_webrtc_enabled: WEBRTC_ENABLED.with(Cell::get),
                    // Remote devtools server (Firefox Remote Debugging Protocol).
                    devtools_server_enabled: devtools_port.is_some(),
                    devtools_server_listen_address: devtools_port
                        .map(|p| format!("127.0.0.1:{p}"))
                        .unwrap_or_default(),
                    // Default web-content fonts (empty family = Servo default).
                    fonts_serif: web_fonts.serif,
                    fonts_sans_serif: web_fonts.sans_serif,
                    fonts_monospace: web_fonts.monospace,
                    fonts_default_size: web_fonts.default_size,
                    fonts_default_monospace_size: web_fonts.monospace_size,
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

    /// Loads `url` in this tab, replacing the current page.
    pub fn navigate(&self, url: Url) {
        self.webview.load(url);
    }

    /// Resizes the WebView and its rendering context to `size` (physical pixels).
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

    /// Sets the theme reported to web content, controlling the CSS
    /// `prefers-color-scheme` media feature (light vs. dark).
    ///
    /// Call this when the desktop switches between light and dark so pages that
    /// adapt their colors follow the system. New pages loaded afterwards inherit
    /// the last value set.
    ///
    /// ```no_run
    /// use cosmic_webview::{ServoState, Theme};
    /// use url::Url;
    /// use dpi::PhysicalSize;
    ///
    /// let url = Url::parse("https://example.com").unwrap();
    /// let (state, _rx) = ServoState::new(url, PhysicalSize::new(800_u32, 600), 1.0);
    /// state.set_theme(Theme::Dark);
    /// ```
    pub fn set_theme(&self, theme: Theme) {
        self.webview.notify_theme_change(theme);
    }

    /// Sets the theme reported to web content from a [`ThemePreference`],
    /// resolving `FollowSystem` against the current COSMIC desktop theme.
    ///
    /// Prefer this over [`ServoState::set_theme`] when the embedder exposes a
    /// light/dark/follow-system choice. Call it again when the desktop theme
    /// changes (while `FollowSystem`) so pages keep following it.
    ///
    /// ```no_run
    /// use cosmic_webview::{ServoState, ThemePreference};
    /// use url::Url;
    /// use dpi::PhysicalSize;
    ///
    /// let url = Url::parse("https://example.com").unwrap();
    /// let (state, _rx) = ServoState::new(url, PhysicalSize::new(800_u32, 600), 1.0);
    /// state.set_theme_preference(ThemePreference::FollowSystem);
    /// ```
    pub fn set_theme_preference(&self, preference: ThemePreference) {
        self.set_theme(preference.resolve());
    }

    /// Navigates back one entry in this tab's session history.
    pub fn go_back(&self) {
        self.webview.go_back(1);
    }

    /// Navigates forward one entry in this tab's session history.
    pub fn go_forward(&self) {
        self.webview.go_forward(1);
    }

    /// Reloads the current page.
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

    /// Forwards a mouse or keyboard [`InputEvent`] to the page.
    pub fn send_input(&self, event: InputEvent) {
        self.webview.notify_input_event(event);
    }

    /// Forwards a keyboard event to the engine **only if it is a bare modifier
    /// key** (Control/Shift/Alt/Super); any other key is ignored.
    ///
    /// The engine tracks a single global keyboard-modifier state (used e.g. for
    /// its built-in Ctrl+click "open in new tab"). Feed this from a window-level
    /// key listener so a modifier release is not missed when a non-webview page
    /// is shown — otherwise the modifier sticks and ordinary clicks are treated
    /// as Ctrl+click. Because only modifier keys pass through, this never
    /// forwards typed content and cannot leak keystrokes; route real input
    /// through [`crate::WebViewWidget`], which gates it to the focused page.
    pub fn send_modifier_key(&self, event: &libcosmic::iced::keyboard::Event) {
        if crate::input::is_bare_modifier_key(event)
            && let Some(input_event) = crate::input::keyboard_event_to_servo(event)
        {
            self.webview.notify_input_event(input_event);
        }
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

    /// Returns the load status of the page currently loading or loaded in this tab.
    pub fn load_status(&self) -> LoadStatus {
        self.shared.load_status.get()
    }

    /// Whether the page has active animations (CSS transitions/animations or
    /// `requestAnimationFrame` callbacks) and therefore needs the event loop
    /// spun at the display refresh rate to advance them.
    ///
    /// Drive a `libcosmic::iced::window::frames` subscription with this so a
    /// foreground animating tab is clocked at vsync rather than as fast as the
    /// CPU allows.
    pub fn animating(&self) -> bool {
        self.webview.animating()
    }

    /// Install or remove a content filter for this tab.
    ///
    /// The closure receives `(request_url, resource_type, source_url)` and
    /// returns `true` when the request should be blocked. `resource_type` is
    /// the adblock resource-type string ("script", "image", "stylesheet", …).
    /// Pass `None` to disable filtering.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use cosmic_webview::ServoState;
    /// # use std::sync::Arc;
    /// # fn f(state: &ServoState) {
    /// // Block all image requests.
    /// state.set_content_filter(Some(Arc::new(|_url, resource_type, _source| {
    ///     resource_type == "image"
    /// })));
    /// # }
    /// ```
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

    /// Takes the pending Bluetooth device-selection request, if any.
    ///
    /// The embedder responds by calling
    /// [`BluetoothDeviceSelectionRequest::pick_device`] or
    /// [`BluetoothDeviceSelectionRequest::cancel`]; dropping it without
    /// responding cancels the request.
    pub fn take_bluetooth_request(&self) -> Option<BluetoothDeviceSelectionRequest> {
        self.shared.pending_bluetooth.borrow_mut().take()
    }

    /// Takes the next WebView the engine opened for the embedder — via
    /// `window.open()`, a `target="_blank"` link, or a Ctrl/middle-click that
    /// Servo routes through the engine — as a ready-to-use tab.
    ///
    /// Returns one [`ServoState`] per call in FIFO order; call it in a loop until
    /// it yields `None`. Each returned state shares the process-wide engine and
    /// owns its own rendering context and frame buffer, exactly like one from
    /// [`ServoState::new`]. Drop the returned state to reject the popup.
    ///
    /// The new page's URL is not known synchronously; read it from
    /// [`ServoState::current_url`] once the engine reports it.
    pub fn take_pending_new_tab(&self) -> Option<ServoState> {
        let pending = {
            let mut queue = self.shared.pending_new_tabs.borrow_mut();
            if queue.is_empty() {
                return None;
            }
            queue.remove(0)
        };
        Some(ServoState {
            servo: self.servo.clone(),
            webview: pending.webview,
            rendering_ctx: pending.rendering_ctx,
            frame: pending.frame,
            shared: pending.shared,
            dialog_handler: DialogHandler::new(),
        })
    }

    /// Takes the favicon the engine last reported for the page, if a new one has
    /// arrived since the previous call.
    ///
    /// The engine resolves `<link rel="icon">` (including SVG and `data:` URIs)
    /// and the default location, so this is more accurate than fetching
    /// `/favicon.ico`. Returns an [`RgbaImage`]; `None` when nothing new is
    /// pending. Prefer this over a network fetch and fall back to fetching only
    /// when the engine never reports one.
    pub fn take_favicon(&self) -> Option<RgbaImage> {
        self.shared.pending_favicon.borrow_mut().take()
    }

    /// Returns `true` once if page content closed this WebView via
    /// `window.close()`.
    ///
    /// Consumes the signal: subsequent calls return `false` until another close
    /// occurs. The app should remove the corresponding tab, though it may refuse
    /// to close the last remaining one.
    pub fn take_closed(&self) -> bool {
        self.shared.closed.replace(false)
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

    // ── Find-in-page ─────────────────────────────────────────────────────────

    /// Start a new find session: inject `<mark>` elements for all matches,
    /// highlight the current match in orange, and scroll it into view.
    ///
    /// Returns a [`FindResult`] with `total` and `current = 1` (or both zero).
    ///
    /// `wrap` is ignored here (a fresh search always lands on the first match);
    /// it applies only to [`find_next`](Self::find_next) and
    /// [`find_prev`](Self::find_prev).
    ///
    /// Uses DOM mutation (`<mark>` elements + injected CSS) rather than
    /// `window.find()` (absent in Servo) or `Selection` (not rendered visually
    /// in Servo's embedder mode).
    pub fn find(
        &self,
        query: &str,
        case_sensitive: bool,
        _wrap: bool,
    ) -> libcosmic::iced::Task<FindResult> {
        if query.is_empty() {
            self.find_clear();
            return libcosmic::iced::Task::done(FindResult::default());
        }
        let q = escape_js(query);
        let flags = if case_sensitive { "g" } else { "gi" };
        let script = format!(
            r#"(function(){{
                var old=document.querySelectorAll('mark.cf');
                for(var i=0;i<old.length;i++){{
                    var par=old[i].parentNode;
                    if(par)par.replaceChild(document.createTextNode(old[i].textContent),old[i]);
                }}
                if(document.body)document.body.normalize();
                window.__cosmicFind=null;
                if(!document.body)return 0;
                if(!document.getElementById('cf-style')){{
                    var s=document.createElement('style');
                    s.id='cf-style';
                    s.textContent='mark.cf{{background:#ffff00!important;color:#000000!important;border-radius:2px}}mark.cfc{{background:#ff8000!important;color:#000000!important}}';
                    document.head.appendChild(s);
                }}
                var regex=new RegExp("{q}".replace(/[.*+?^${{}}()|[\]\\]/g,'\\$&'),'{flags}');
                var walker=document.createTreeWalker(document.body,4);
                var textNodes=[];
                var node;
                while((node=walker.nextNode())){{
                    var pn=node.parentNode;
                    var pname=pn?pn.nodeName:'';
                    if(pname!=='SCRIPT'&&pname!=='STYLE'&&pname!=='NOSCRIPT'&&pname!=='MARK'){{
                        textNodes.push(node);
                    }}
                }}
                var marks=[];
                for(var ti=0;ti<textNodes.length;ti++){{
                    var tn=textNodes[ti];
                    if(!tn.parentNode)continue;
                    var text=tn.nodeValue;
                    var parts=[];
                    regex.lastIndex=0;
                    var m;
                    var lastIdx=0;
                    var hasMatch=false;
                    while((m=regex.exec(text))!==null){{
                        hasMatch=true;
                        if(m.index>lastIdx)parts.push(document.createTextNode(text.slice(lastIdx,m.index)));
                        var mark=document.createElement('mark');
                        mark.className='cf';
                        mark.textContent=m[0];
                        parts.push(mark);
                        marks.push(mark);
                        lastIdx=m.index+m[0].length;
                    }}
                    if(hasMatch){{
                        if(lastIdx<text.length)parts.push(document.createTextNode(text.slice(lastIdx)));
                        var parent=tn.parentNode;
                        if(parent){{
                            for(var pi=0;pi<parts.length;pi++)parent.insertBefore(parts[pi],tn);
                            parent.removeChild(tn);
                        }}
                    }}
                }}
                window.__cosmicFind={{marks:marks,current:0}};
                if(marks.length>0){{
                    marks[0].className='cf cfc';
                    marks[0].scrollIntoView({{block:'nearest'}});
                }}
                return marks.length;
            }})()"#,
        );
        self.eval_js(script, |val| {
            let total = match val {
                JSValue::Number(n) => n as usize,
                _ => 0,
            };
            FindResult {
                current: if total > 0 { 1 } else { 0 },
                total,
            }
        })
    }

    /// Navigate to the next match in the page.
    ///
    /// Updates the `cfc` CSS class on `<mark>` elements to advance the
    /// highlighted match and scrolls it into view. Returns the new 1-based
    /// current index, or `0` if there are no matches or wrap is disabled and
    /// the last match is already selected.
    pub fn find_next(&self, wrap: bool) -> libcosmic::iced::Task<usize> {
        let script = format!(
            r#"(function(){{
                if(!window.__cosmicFind)return 0;
                var f=window.__cosmicFind;
                var total=f.marks.length;
                if(total===0)return 0;
                var next=f.current+1;
                if(next>=total){{
                    if(!{wrap})return f.current+1;
                    next=0;
                }}
                f.marks[f.current].className='cf';
                f.current=next;
                f.marks[next].className='cf cfc';
                f.marks[next].scrollIntoView({{block:'nearest'}});
                return next+1;
            }})()"#,
        );
        self.eval_js(script, |val| match val {
            JSValue::Number(n) => n as usize,
            _ => 0,
        })
    }

    /// Navigate to the previous match in the page.
    ///
    /// Like [`find_next`](Self::find_next) but searches backwards. Returns the new 1-based
    /// current index, or `0` if there are no matches.
    pub fn find_prev(&self, wrap: bool) -> libcosmic::iced::Task<usize> {
        let script = format!(
            r#"(function(){{
                if(!window.__cosmicFind)return 0;
                var f=window.__cosmicFind;
                var total=f.marks.length;
                if(total===0)return 0;
                var prev=f.current-1;
                if(prev<0){{
                    if(!{wrap})return f.current+1;
                    prev=total-1;
                }}
                f.marks[f.current].className='cf';
                f.current=prev;
                f.marks[prev].className='cf cfc';
                f.marks[prev].scrollIntoView({{block:'nearest'}});
                return prev+1;
            }})()"#,
        );
        self.eval_js(script, |val| match val {
            JSValue::Number(n) => n as usize,
            _ => 0,
        })
    }

    /// Clear the current find session: remove all injected `<mark>` elements
    /// and restore the original text nodes.
    pub fn find_clear(&self) {
        self.webview.evaluate_javascript(
            r#"(function(){
                if(window.__cosmicFind){
                    var marks=window.__cosmicFind.marks;
                    for(var i=0;i<marks.length;i++){
                        var m=marks[i];
                        var parent=m.parentNode;
                        if(parent)parent.replaceChild(document.createTextNode(m.textContent),m);
                    }
                    window.__cosmicFind=null;
                }
                if(document.body)document.body.normalize();
            })()"#,
            |_| {},
        );
    }

    /// Evaluate a JS snippet and map its result to `T`.
    ///
    /// The task resolves when Servo calls the evaluation callback. Script errors
    /// or evaluation failures resolve to `T::default()`.
    fn eval_js<T: Default + Send + 'static>(
        &self,
        script: String,
        parse: impl FnOnce(JSValue) -> T + Send + 'static,
    ) -> libcosmic::iced::Task<T> {
        let (tx, rx) = async_channel::bounded::<T>(1);
        self.webview.evaluate_javascript(
            script,
            move |result: Result<JSValue, JavaScriptEvaluationError>| {
                let val = result.unwrap_or(JSValue::Null);
                let _ = tx.try_send(parse(val));
            },
        );
        libcosmic::iced::Task::future(async move { rx.recv().await.unwrap_or_default() })
    }
}

/// Escape a Rust string for safe embedding in a JavaScript double-quoted string literal.
fn escape_js(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::rendering::ActiveRenderingContext;

    #[test]
    fn theme_preference_resolves_explicit_choices() {
        assert_eq!(ThemePreference::Light.resolve(), Theme::Light);
        assert_eq!(ThemePreference::Dark.resolve(), Theme::Dark);
    }

    #[test]
    fn theme_preference_follow_system_resolves_without_panicking() {
        // Depends on the ambient desktop theme; assert only that it resolves to a
        // valid variant rather than hard-coding light or dark.
        let theme = ThemePreference::FollowSystem.resolve();
        assert!(matches!(theme, Theme::Light | Theme::Dark));
    }

    #[test]
    fn theme_preference_defaults_to_follow_system() {
        assert_eq!(ThemePreference::default(), ThemePreference::FollowSystem);
    }

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
