use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use async_channel::Sender;
use content_security_policy::Destination;
use servo::{
    AllowOrDenyRequest, AuthenticationRequest, BluetoothDeviceSelectionRequest, ConsoleLogLevel,
    CreateNewWebViewRequest, Cursor, DeviceIntPoint, DeviceIntSize, EmbedderControl,
    EmbedderControlId, Image, LoadStatus, MediaMetadata, MediaPositionState, MediaSessionEvent,
    MediaSessionPlaybackState, Notification, PermissionRequest, PixelFormat, RenderingContext,
    RgbaImage, ScreenGeometry, ServoDelegate, ServoError, WebResourceLoad, WebResourceResponse,
    WebView, WebViewDelegate,
};
use url::Url;

#[cfg(all(target_os = "linux", feature = "dmabuf"))]
use crate::gpu_share::{GlImporter, GpuPath};
use crate::primitive::{FrameSink, WebViewFrame};
use crate::rendering::{ActiveRenderingContext, read_frame_into_sink};

/// Lazily initialised GL-side importer for the zero-copy path.
///
/// `Ready` is boxed: the importer is much larger than the unit variants and
/// this enum lives in every delegate.
#[cfg(all(target_os = "linux", feature = "dmabuf"))]
#[derive(Default)]
pub(crate) enum GlShareState {
    #[default]
    Untried,
    Ready(Box<GlImporter>),
    Unavailable,
}

/// A content filter callback installed per-tab.
///
/// Returns `true` when a network request should be blocked.
/// Arguments: `(request_url, resource_type, source_url)`.
/// `resource_type` is the adblock resource-type string ("script", "image", …).
pub type ContentFilterFn = Arc<dyn Fn(&Url, &str, Option<&str>) -> bool + Send + Sync>;

/// A WebView the engine created for the embedder (via `window.open()`,
/// `target="_blank"`, etc.), together with the rendering context, frame buffer,
/// and shared state that back it. Queued on the parent's [`SharedState`] until
/// the app adopts it through [`crate::state::ServoState::take_pending_new_tab`].
pub(crate) struct PendingChildWebView {
    pub webview: WebView,
    pub rendering_ctx: ActiveRenderingContext,
    pub frame: WebViewFrame,
    pub shared: Rc<SharedState>,
}

/// State shared between [`CosmicWebViewDelegate`] and [`crate::state::ServoState`].
pub(crate) struct SharedState {
    pub load_status: Cell<LoadStatus>,
    /// Incremented whenever a load starts. Lets a caller that recorded the
    /// value before asking for a navigation tell the new load from the old one.
    pub load_generation: Cell<u64>,
    /// Incremented each time a new frame is committed to the frame sink.
    /// Callers can compare against a stored value to detect actual frame changes.
    pub frame_generation: Cell<u64>,
    /// `false` while the tab is hidden (e.g. a background tab): readback is
    /// skipped entirely and `frame_pending` records that content changed.
    pub visible: Cell<bool>,
    /// Set when a frame became ready while the tab was hidden, so the next
    /// `set_visible(true)` can paint and read it back immediately.
    pub frame_pending: Cell<bool>,
    /// Pending dialog or picker waiting for the app to respond.
    ///
    /// Only one control can be pending at a time; a new arrival replaces the
    /// previous one (which is dropped, sending its default reply automatically).
    pub pending_dialog: RefCell<Option<EmbedderControl>>,
    /// Pending HTTP authentication request.
    pub pending_auth: RefCell<Option<AuthenticationRequest>>,
    /// Pending permission request (geolocation, camera, etc.) waiting for the
    /// app to allow or deny. A new arrival replaces any previous one, which is
    /// dropped and sends its default (deny) reply automatically.
    pub pending_permission: RefCell<Option<PermissionRequest>>,
    /// Pending Bluetooth device-selection request waiting for the user to pick a
    /// device or cancel. A new arrival replaces any previous one, which is dropped
    /// and cancels automatically.
    pub pending_bluetooth: RefCell<Option<BluetoothDeviceSelectionRequest>>,
    /// Pending `beforeunload` request waiting for the user to confirm leaving a
    /// page that says it has unsaved work. Dropping it without answering allows
    /// the unload, which is Servo's default.
    pub pending_unload: RefCell<Option<AllowOrDenyRequest>>,
    /// Desktop notifications the page asked to display, oldest first. A page can
    /// post several before the app next polls, so they queue rather than replace.
    pub pending_notifications: RefCell<Vec<Notification>>,
    /// Most recent cursor shape reported by the page.
    pub cursor: Cell<Cursor>,
    /// URL of the hyperlink currently under the cursor, or `None`. Reported by
    /// the page on hover; used to open links in a new tab via middle-click.
    pub status_text: RefCell<Option<String>>,
    /// Whether the page is currently in fullscreen mode.
    pub is_fullscreen: Cell<bool>,
    /// Crash info set when a pipeline panics: `(reason, backtrace)`.
    pub crashed: RefCell<Option<(String, Option<String>)>>,
    /// Most recent URL reported by the page.
    pub current_url: RefCell<Option<Url>>,
    /// Most recent page title reported by the page.
    pub title: RefCell<Option<String>>,
    /// Whether the session history has a previous entry to go back to.
    pub can_go_back: Cell<bool>,
    /// Whether the session history has a next entry to go forward to.
    pub can_go_forward: Cell<bool>,
    /// Optional content filter: returns `true` when a request should be blocked.
    /// `(request_url, resource_type, source_url)` — resource_type is the adblock
    /// resource type string ("script", "image", "stylesheet" etc.).
    pub content_filter: Mutex<Option<ContentFilterFn>>,
    /// WebViews the engine created for the embedder (popups, `target="_blank"`),
    /// awaiting adoption by the app as new tabs. Drained in FIFO order via
    /// [`crate::state::ServoState::take_pending_new_tab`].
    pub pending_new_tabs: RefCell<Vec<PendingChildWebView>>,
    /// Latest favicon the engine reported for the page (from `<link rel="icon">`
    /// or the default location), decoded to RGBA. Drained via
    /// [`crate::state::ServoState::take_favicon`].
    pub pending_favicon: RefCell<Option<RgbaImage>>,
    /// Set when page content closed this WebView via `window.close()`. Consumed
    /// once via [`crate::state::ServoState::take_closed`]; the app then removes
    /// the tab.
    pub closed: Cell<bool>,
    /// Latest Media Session metadata (title/artist/album) reported by the page.
    pub media_metadata: RefCell<Option<MediaMetadata>>,
    /// Latest Media Session playback state (playing/paused/none).
    pub media_playback: RefCell<Option<MediaSessionPlaybackState>>,
    /// Latest Media Session position state (duration/rate/position).
    pub media_position: RefCell<Option<MediaPositionState>>,
    /// Bumped on every media-session event so the app can detect changes; the
    /// media types do not implement `PartialEq`, so a generation counter is the
    /// simplest change signal. Read via
    /// [`crate::state::ServoState::media_generation`].
    pub media_generation: Cell<u64>,
}

impl SharedState {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            load_status: Cell::new(LoadStatus::Started),
            load_generation: Cell::new(0),
            frame_generation: Cell::new(0),
            visible: Cell::new(true),
            frame_pending: Cell::new(false),
            pending_dialog: RefCell::new(None),
            pending_auth: RefCell::new(None),
            pending_permission: RefCell::new(None),
            pending_bluetooth: RefCell::new(None),
            pending_unload: RefCell::new(None),
            pending_notifications: RefCell::new(Vec::new()),
            cursor: Cell::new(Cursor::Default),
            status_text: RefCell::new(None),
            is_fullscreen: Cell::new(false),
            crashed: RefCell::new(None),
            current_url: RefCell::new(None),
            title: RefCell::new(None),
            can_go_back: Cell::new(false),
            can_go_forward: Cell::new(false),
            content_filter: Mutex::new(None),
            pending_new_tabs: RefCell::new(Vec::new()),
            pending_favicon: RefCell::new(None),
            closed: Cell::new(false),
            media_metadata: RefCell::new(None),
            media_playback: RefCell::new(None),
            media_position: RefCell::new(None),
            media_generation: Cell::new(0),
        })
    }
}

/// Receives callbacks from Servo and updates shared state.
///
/// [`EmbedderControl::SimpleDialog`], [`EmbedderControl::FilePicker`],
/// [`EmbedderControl::ColorPicker`], [`EmbedderControl::SelectElement`],
/// and [`EmbedderControl::ContextMenu`] are stored in
/// [`SharedState::pending_dialog`] for the app to retrieve via
/// [`crate::state::ServoState::take_dialog`]. All other `EmbedderControl`
/// variants are dropped, which sends a default reply automatically.
pub(crate) struct CosmicWebViewDelegate {
    rendering_ctx: Rc<dyn RenderingContext>,
    shared: Rc<SharedState>,
    frame_sink: FrameSink,
    waker_tx: Sender<()>,
    #[cfg(all(target_os = "linux", feature = "dmabuf"))]
    gl_share: RefCell<GlShareState>,
}

impl CosmicWebViewDelegate {
    pub(crate) fn new(
        rendering_ctx: Rc<dyn RenderingContext>,
        shared: Rc<SharedState>,
        frame_sink: FrameSink,
        waker_tx: Sender<()>,
    ) -> Self {
        Self {
            rendering_ctx,
            shared,
            frame_sink,
            waker_tx,
            #[cfg(all(target_os = "linux", feature = "dmabuf"))]
            gl_share: RefCell::new(GlShareState::Untried),
        }
    }

    /// Produces the next displayable frame, preferring the zero-copy path.
    ///
    /// Returns `true` when a new frame was committed (either into a shared
    /// GPU buffer or the CPU pixel buffer).
    fn produce_frame(&self) -> bool {
        #[cfg(all(target_os = "linux", feature = "dmabuf"))]
        if self.try_gpu_frame() {
            return true;
        }
        read_frame_into_sink(&self.rendering_ctx, &self.frame_sink)
    }

    /// Drives the GL side of the zero-copy state machine.
    ///
    /// Returns `true` when a frame was committed to a shared GPU buffer;
    /// `false` means the caller should fall back to CPU readback (which is
    /// also the permanent outcome after `set_gpu_failed`).
    #[cfg(all(target_os = "linux", feature = "dmabuf"))]
    fn try_gpu_frame(&self) -> bool {
        let size = self.rendering_ctx.size();
        let (width, height) = (size.width, size.height);

        match self.frame_sink.gpu_path() {
            GpuPath::Active { offer, index } => {
                if offer.width != width || offer.height != height {
                    // Resized: shared buffers no longer match. Back to CPU
                    // readback; the wgpu side re-offers at the new size.
                    self.release_importer();
                    self.frame_sink.set_gpu_inactive();
                    return false;
                }
                let next = (index + 1) % crate::gpu_share::BUFFER_COUNT;
                if self.blit_shared(next) {
                    self.frame_sink.set_gpu_active(next);
                    true
                } else {
                    self.release_importer();
                    self.frame_sink.set_gpu_failed();
                    false
                }
            }
            GpuPath::Offered(offer) => {
                if offer.width != width || offer.height != height {
                    // Stale offer from before a resize; request a fresh one.
                    self.frame_sink.set_gpu_inactive();
                    return false;
                }
                if !self.import_offer(&offer) {
                    self.frame_sink.set_gpu_failed();
                    return false;
                }
                if self.blit_shared(0) {
                    self.frame_sink.set_gpu_active(0);
                    true
                } else {
                    self.release_importer();
                    self.frame_sink.set_gpu_failed();
                    false
                }
            }
            GpuPath::Inactive | GpuPath::Failed => false,
        }
    }

    /// Imports the offer's buffers, initialising the EGL importer on first use.
    /// Requires (and ensures) Servo's GL context is current.
    #[cfg(all(target_os = "linux", feature = "dmabuf"))]
    fn import_offer(&self, offer: &crate::gpu_share::GpuOffer) -> bool {
        if self.rendering_ctx.make_current().is_err() {
            return false;
        }
        let mut state = self.gl_share.borrow_mut();
        if matches!(*state, GlShareState::Untried) {
            *state = match GlImporter::try_new() {
                Some(importer) => GlShareState::Ready(Box::new(importer)),
                None => GlShareState::Unavailable,
            };
        }
        let GlShareState::Ready(importer) = &mut *state else {
            return false;
        };
        if importer.imported_epoch() == Some(offer.epoch) {
            return true;
        }
        let gl = self.rendering_ctx.gleam_gl_api();
        importer.import(&gl, offer)
    }

    /// Blits Servo's rendered surface into shared buffer `index`.
    #[cfg(all(target_os = "linux", feature = "dmabuf"))]
    fn blit_shared(&self, index: usize) -> bool {
        if self.rendering_ctx.make_current().is_err() {
            return false;
        }
        let state = self.gl_share.borrow();
        let GlShareState::Ready(importer) = &*state else {
            return false;
        };
        // Bind Servo's surface FBO, then recover its id generically — works
        // for any RenderingContext implementation without exposing internals.
        self.rendering_ctx.prepare_for_rendering();
        let gl = self.rendering_ctx.gleam_gl_api();
        let mut binding = [0i32; 1];
        // SAFETY: gleam's get_integer_v writes one value for FRAMEBUFFER_BINDING.
        unsafe { gl.get_integer_v(gleam::gl::FRAMEBUFFER_BINDING, &mut binding) };
        importer.blit(&gl, binding[0] as u32, index)
    }

    /// Drops imported GL resources (best effort; needs the context current).
    #[cfg(all(target_os = "linux", feature = "dmabuf"))]
    fn release_importer(&self) {
        if self.rendering_ctx.make_current().is_err() {
            return;
        }
        if let GlShareState::Ready(importer) = &mut *self.gl_share.borrow_mut() {
            let gl = self.rendering_ctx.gleam_gl_api();
            importer.release(&gl);
        }
    }
}

impl WebViewDelegate for CosmicWebViewDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        if self.shared.visible.get() {
            webview.paint();
            if self.produce_frame() {
                self.shared
                    .frame_generation
                    .set(self.shared.frame_generation.get().wrapping_add(1));
            }
        } else {
            // Hidden tab: skip compositing and readback entirely; remember
            // that content changed so set_visible(true) can catch up.
            self.shared.frame_pending.set(true);
        }
        // Deliberately no waker signal here. Frame-ready must not drive the event
        // loop: doing so pumps a spin→frame-ready→spin loop that runs far faster
        // than the display (a background tab even re-emits the same unpainted
        // frame every spin). Instead, the frame produced during this spin is
        // presented by the redraw that follows the spin, and continuous
        // animation is clocked at the display refresh rate by a `window::frames`
        // subscription gated on `ServoState::animating`. Genuine engine progress
        // (network, timers, input results) still wakes the loop through Servo's
        // own EventLoopWaker.
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        // Count starts, not changes: a caller waiting for *this* navigation to
        // finish needs to tell it apart from the page that was already loaded.
        if status == LoadStatus::Started {
            let generation = self.shared.load_generation.get();
            self.shared.load_generation.set(generation.wrapping_add(1));
        }
        self.shared.load_status.set(status);
    }

    fn show_embedder_control(&self, _webview: WebView, control: EmbedderControl) {
        match control {
            // Queue controls that require user interaction for the app to present.
            EmbedderControl::SimpleDialog(_)
            | EmbedderControl::FilePicker(_)
            | EmbedderControl::ColorPicker(_)
            | EmbedderControl::SelectElement(_)
            | EmbedderControl::ContextMenu(_) => {
                *self.shared.pending_dialog.borrow_mut() = Some(control);
                let _ = self.waker_tx.try_send(());
            }
            // All other controls: drop = automatic default reply.
            _ => {}
        }
    }

    fn hide_embedder_control(&self, _webview: WebView, _id: EmbedderControlId) {}

    fn screen_geometry(&self, _webview: WebView) -> Option<ScreenGeometry> {
        crate::state::servo_screen_geometry()
    }

    /// Ignored: WebViews are tabs in one host window, so a page must not be able
    /// to move it. The request is dropped rather than failed, as the DOM APIs
    /// (`window.moveTo`, `window.moveBy`) have no way to report a refusal.
    fn request_move_to(&self, _webview: WebView, point: DeviceIntPoint) {
        tracing::debug!("ignoring page request to move the window to {point:?}");
    }

    /// Ignored, for the same reason as [`Self::request_move_to`].
    fn request_resize_to(&self, _webview: WebView, size: DeviceIntSize) {
        tracing::debug!("ignoring page request to resize the window to {size:?}");
    }

    fn notify_cursor_changed(&self, _webview: WebView, cursor: Cursor) {
        self.shared.cursor.set(cursor);
        let _ = self.waker_tx.try_send(());
    }

    fn notify_status_text_changed(&self, _webview: WebView, status: Option<String>) {
        *self.shared.status_text.borrow_mut() = status;
        let _ = self.waker_tx.try_send(());
    }

    fn notify_fullscreen_state_changed(&self, _webview: WebView, is_fullscreen: bool) {
        self.shared.is_fullscreen.set(is_fullscreen);
        let _ = self.waker_tx.try_send(());
    }

    fn notify_crashed(&self, _webview: WebView, reason: String, backtrace: Option<String>) {
        tracing::error!("WebView crashed: {reason}");
        if let Some(bt) = &backtrace {
            tracing::error!("Backtrace:\n{bt}");
        }
        *self.shared.crashed.borrow_mut() = Some((reason, backtrace));
        let _ = self.waker_tx.try_send(());
    }

    fn request_authentication(
        &self,
        _webview: WebView,
        authentication_request: AuthenticationRequest,
    ) {
        *self.shared.pending_auth.borrow_mut() = Some(authentication_request);
        let _ = self.waker_tx.try_send(());
    }

    fn request_permission(&self, _webview: WebView, request: PermissionRequest) {
        *self.shared.pending_permission.borrow_mut() = Some(request);
        let _ = self.waker_tx.try_send(());
    }

    /// A page's `beforeunload` handler asked whether it may be discarded.
    ///
    /// Queued for the app to confirm with the user. A second request replaces an
    /// unanswered one, which then allows its own unload as it drops — the newer
    /// question is the one the user is looking at.
    fn request_unload(&self, _webview: WebView, request: AllowOrDenyRequest) {
        *self.shared.pending_unload.borrow_mut() = Some(request);
        let _ = self.waker_tx.try_send(());
    }

    /// The page posted a desktop notification through the Notification API.
    ///
    /// Servo has already checked the permission, so reaching here means the site
    /// is allowed to notify.
    fn show_notification(&self, _webview: WebView, notification: Notification) {
        self.shared
            .pending_notifications
            .borrow_mut()
            .push(notification);
        let _ = self.waker_tx.try_send(());
    }

    fn show_bluetooth_device_dialog(
        &self,
        _webview: WebView,
        request: BluetoothDeviceSelectionRequest,
    ) {
        *self.shared.pending_bluetooth.borrow_mut() = Some(request);
        let _ = self.waker_tx.try_send(());
    }

    fn request_create_new(&self, parent_webview: WebView, request: CreateNewWebViewRequest) {
        // The child gets its own rendering context, frame buffer, shared state,
        // and delegate — the same one-context-per-tab invariant every tab holds.
        // Building the WebView here (rather than dropping the request) accepts the
        // popup; dropping would deny it and make `window.open()` return null.
        let child_ctx = ActiveRenderingContext::create(self.rendering_ctx.size());
        let child_shared = SharedState::new();
        let child_frame = WebViewFrame::new();
        let child_delegate = Rc::new(CosmicWebViewDelegate::new(
            child_ctx.as_rc(),
            child_shared.clone(),
            child_frame.sink(),
            self.waker_tx.clone(),
        ));
        let mut child_builder = request
            .builder(child_ctx.as_rc())
            .hidpi_scale_factor(parent_webview.hidpi_scale_factor())
            .delegate(child_delegate);
        // Popups share the same user scripts/stylesheets as normal tabs.
        if let Some(user_content) = crate::state::user_content_manager() {
            child_builder = child_builder.user_content_manager(user_content);
        }
        let webview = child_builder.build();

        self.shared
            .pending_new_tabs
            .borrow_mut()
            .push(PendingChildWebView {
                webview,
                rendering_ctx: child_ctx,
                frame: child_frame,
                shared: child_shared,
            });
        let _ = self.waker_tx.try_send(());
    }

    fn notify_url_changed(&self, _webview: WebView, url: Url) {
        *self.shared.current_url.borrow_mut() = Some(url);
        let _ = self.waker_tx.try_send(());
    }

    fn notify_page_title_changed(&self, _webview: WebView, title: Option<String>) {
        *self.shared.title.borrow_mut() = title;
        let _ = self.waker_tx.try_send(());
    }

    fn notify_favicon_changed(&self, webview: WebView) {
        // The engine resolves <link rel="icon"> (including SVG and data: URIs) and
        // the default location, so this is more accurate than fetching /favicon.ico.
        let rgba = webview.favicon().as_deref().and_then(favicon_to_rgba);
        *self.shared.pending_favicon.borrow_mut() = rgba;
        let _ = self.waker_tx.try_send(());
    }

    fn notify_closed(&self, _webview: WebView) {
        // Page content called window.close(). Record it and wake the app so it
        // can drain the flag via `take_closed` and remove the tab.
        self.shared.closed.set(true);
        let _ = self.waker_tx.try_send(());
    }

    fn notify_media_session_event(&self, _webview: WebView, event: MediaSessionEvent) {
        // Store the latest value for whichever field the event carries; the app
        // reads them to drive system media controls (MPRIS).
        match event {
            MediaSessionEvent::SetMetadata(metadata) => {
                *self.shared.media_metadata.borrow_mut() = Some(metadata);
            }
            MediaSessionEvent::PlaybackStateChange(state) => {
                *self.shared.media_playback.borrow_mut() = Some(state);
            }
            MediaSessionEvent::SetPositionState(position) => {
                *self.shared.media_position.borrow_mut() = Some(position);
            }
        }
        self.shared
            .media_generation
            .set(self.shared.media_generation.get().wrapping_add(1));
        let _ = self.waker_tx.try_send(());
    }

    fn notify_history_changed(&self, _webview: WebView, entries: Vec<Url>, current: usize) {
        self.shared.can_go_back.set(current > 0);
        self.shared.can_go_forward.set(current + 1 < entries.len());
    }

    fn show_console_message(&self, _webview: WebView, level: ConsoleLogLevel, message: String) {
        match level {
            ConsoleLogLevel::Trace => tracing::trace!(target: "webview::console", "{message}"),
            ConsoleLogLevel::Debug => tracing::debug!(target: "webview::console", "{message}"),
            ConsoleLogLevel::Warn => tracing::warn!(target: "webview::console", "{message}"),
            ConsoleLogLevel::Error => tracing::error!(target: "webview::console", "{message}"),
            // Log, Info, Dir all map to info level.
            _ => tracing::info!(target: "webview::console", "{message}"),
        }
    }

    fn load_web_resource(&self, _webview: WebView, load: WebResourceLoad) {
        let filter = self.shared.content_filter.lock().unwrap();
        let Some(f) = filter.as_ref() else {
            return; // no filter — DoNotIntercept (load is dropped)
        };

        // Extract everything we need before consuming `load` via intercept().
        let (url, resource_type, source) = {
            let req = load.request();
            let url = req.url.clone();
            let resource_type = destination_to_resource_type(&req.destination);
            let source = req.referrer_url.as_ref().map(|u| u.to_string());
            (url, resource_type, source)
        };

        if f(&url, resource_type, source.as_deref()) {
            tracing::debug!(
                url = %url,
                resource_type,
                source = source.as_deref().unwrap_or("-"),
                "blocked"
            );
            // Return an empty 200 rather than cancelling: a network error on a
            // cancelled sub-resource causes noisy console output on many pages.
            load.intercept(WebResourceResponse::new(url)).finish();
        }
        // else: drop load → DoNotIntercept
    }
}

/// Handles engine-level errors and forwards them to the tracing subscriber.
pub(crate) struct CosmicServoDelegate;

impl ServoDelegate for CosmicServoDelegate {
    fn notify_error(&self, error: ServoError) {
        tracing::error!("Servo engine error: {error:?}");
    }

    fn request_devtools_connection(&self, request: AllowOrDenyRequest) {
        // The devtools server is loopback-only; still prompt so the user can
        // decline an unexpected connection. Stored engine-level (one server per
        // process) and drained via `take_devtools_request`.
        crate::state::store_devtools_request(request);
    }
}

/// Converts an engine [`Image`] favicon to an [`RgbaImage`].
fn favicon_to_rgba(image: &Image) -> Option<RgbaImage> {
    convert_pixels(image.width, image.height, image.format, image.data())
}

/// Normalises a pixel buffer of any [`PixelFormat`] to RGBA8 and wraps it in an
/// [`RgbaImage`]. Returns `None` if the dimensions and buffer length disagree
/// (a malformed image).
fn convert_pixels(width: u32, height: u32, format: PixelFormat, data: &[u8]) -> Option<RgbaImage> {
    let rgba: Vec<u8> = match format {
        PixelFormat::K8 => data.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        PixelFormat::KA8 => data
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[k, a]| [k, k, k, a])
            .collect(),
        PixelFormat::RGB8 => data
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        PixelFormat::RGBA8 => data.to_vec(),
        PixelFormat::BGRA8 => data
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[b, g, r, a]| [r, g, b, a])
            .collect(),
    };
    RgbaImage::from_raw(width, height, rgba)
}

/// Maps a CSP `Destination` to the resource-type string expected by the
/// adblock crate. Unknown/worklet types fall back to `"other"`.
fn destination_to_resource_type(dest: &Destination) -> &'static str {
    match dest {
        // Destination::None is the fetch/XHR/beacon destination (empty string in the
        // CSP spec). Tracking pixels and analytics almost always use this path, so
        // mapping to "xmlhttprequest" is essential for EasyPrivacy rules to match.
        Destination::None => "xmlhttprequest",
        Destination::Audio | Destination::Video | Destination::Track => "media",
        Destination::Document | Destination::Frame | Destination::IFrame => "subdocument",
        Destination::Embed | Destination::Object => "object",
        Destination::Font => "font",
        Destination::Image => "image",
        Destination::Script
        | Destination::Worker
        | Destination::ServiceWorker
        | Destination::SharedWorker => "script",
        Destination::Style | Destination::Xslt => "stylesheet",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_state_pending_dialog_initially_none() {
        let shared = SharedState::new();
        assert!(shared.pending_dialog.borrow().is_none());
    }

    #[test]
    fn shared_state_take_pending_dialog_is_idempotent() {
        let shared = SharedState::new();
        // Taking from an empty slot never panics and always returns None.
        assert!(shared.pending_dialog.borrow_mut().take().is_none());
        assert!(shared.pending_dialog.borrow_mut().take().is_none());
    }

    #[test]
    fn shared_state_cursor_defaults_to_default() {
        let shared = SharedState::new();
        assert_eq!(shared.cursor.get(), Cursor::Default);
    }

    #[test]
    fn shared_state_fullscreen_defaults_to_false() {
        let shared = SharedState::new();
        assert!(!shared.is_fullscreen.get());
    }

    #[test]
    fn shared_state_crashed_initially_none() {
        let shared = SharedState::new();
        assert!(shared.crashed.borrow().is_none());
    }

    #[test]
    fn shared_state_pending_auth_initially_none() {
        let shared = SharedState::new();
        assert!(shared.pending_auth.borrow().is_none());
    }

    #[test]
    fn shared_state_pending_permission_initially_none() {
        let shared = SharedState::new();
        assert!(shared.pending_permission.borrow().is_none());
    }

    #[test]
    fn cursor_cell_can_be_updated() {
        let shared = SharedState::new();
        shared.cursor.set(Cursor::Pointer);
        assert_eq!(shared.cursor.get(), Cursor::Pointer);
    }

    #[test]
    fn convert_pixels_rgba8_passes_through() {
        let src = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let img = convert_pixels(2, 1, PixelFormat::RGBA8, &src).expect("valid");
        assert_eq!(img.as_raw(), &src);
    }

    #[test]
    fn convert_pixels_rgb8_adds_opaque_alpha() {
        let img = convert_pixels(1, 1, PixelFormat::RGB8, &[10, 20, 30]).expect("valid");
        assert_eq!(img.as_raw(), &[10, 20, 30, 255]);
    }

    #[test]
    fn convert_pixels_k8_expands_to_opaque_gray() {
        let img = convert_pixels(2, 1, PixelFormat::K8, &[7, 9]).expect("valid");
        assert_eq!(img.as_raw(), &[7, 7, 7, 255, 9, 9, 9, 255]);
    }

    #[test]
    fn convert_pixels_bgra8_swaps_red_and_blue() {
        let img = convert_pixels(1, 1, PixelFormat::BGRA8, &[30, 20, 10, 128]).expect("valid");
        assert_eq!(img.as_raw(), &[10, 20, 30, 128]);
    }

    #[test]
    fn convert_pixels_rejects_size_mismatch() {
        // 2x2 needs 16 RGBA bytes; only 4 provided.
        assert!(convert_pixels(2, 2, PixelFormat::RGBA8, &[0, 0, 0, 255]).is_none());
    }

    #[test]
    fn shared_state_pending_new_tabs_initially_empty() {
        let shared = SharedState::new();
        assert!(shared.pending_new_tabs.borrow().is_empty());
    }

    #[test]
    fn shared_state_drain_pending_new_tabs_is_fifo_and_idempotent() {
        // We cannot build a WebView without a running engine, so exercise the
        // queue's ordering and drain semantics with the Vec directly.
        let mut queue: Vec<u32> = vec![1, 2, 3];
        let first = (!queue.is_empty()).then(|| queue.remove(0));
        assert_eq!(first, Some(1)); // FIFO: oldest popup first
        assert_eq!(queue, vec![2, 3]);
        queue.clear();
        // Draining an empty queue never panics and yields nothing.
        let none = (!queue.is_empty()).then(|| queue.remove(0));
        assert_eq!(none, None);
    }

    #[test]
    fn crashed_stores_and_takes_info() {
        let shared = SharedState::new();
        *shared.crashed.borrow_mut() = Some(("oops".to_owned(), None));
        let taken = shared.crashed.borrow_mut().take();
        assert_eq!(taken, Some(("oops".to_owned(), None)));
        assert!(shared.crashed.borrow().is_none());
    }
}
