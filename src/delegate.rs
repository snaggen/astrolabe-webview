use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use async_channel::Sender;
use content_security_policy::Destination;
use servo::{
    AuthenticationRequest, ConsoleLogLevel, Cursor, EmbedderControl, EmbedderControlId, LoadStatus,
    PermissionRequest, RenderingContext, ServoDelegate, ServoError, WebResourceLoad,
    WebResourceResponse, WebView, WebViewDelegate,
};
use url::Url;

#[cfg(all(target_os = "linux", feature = "dmabuf"))]
use crate::gpu_share::{GlImporter, GpuPath};
use crate::primitive::FrameSink;
use crate::rendering::read_frame_into_sink;

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

/// State shared between [`CosmicWebViewDelegate`] and [`crate::state::ServoState`].
pub(crate) struct SharedState {
    pub load_status: Cell<LoadStatus>,
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
}

impl SharedState {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            load_status: Cell::new(LoadStatus::Started),
            frame_generation: Cell::new(0),
            visible: Cell::new(true),
            frame_pending: Cell::new(false),
            pending_dialog: RefCell::new(None),
            pending_auth: RefCell::new(None),
            pending_permission: RefCell::new(None),
            cursor: Cell::new(Cursor::Default),
            status_text: RefCell::new(None),
            is_fullscreen: Cell::new(false),
            crashed: RefCell::new(None),
            current_url: RefCell::new(None),
            title: RefCell::new(None),
            can_go_back: Cell::new(false),
            can_go_forward: Cell::new(false),
            content_filter: Mutex::new(None),
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
        let _ = self.waker_tx.try_send(());
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
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

    fn notify_url_changed(&self, _webview: WebView, url: Url) {
        *self.shared.current_url.borrow_mut() = Some(url);
        let _ = self.waker_tx.try_send(());
    }

    fn notify_page_title_changed(&self, _webview: WebView, title: Option<String>) {
        *self.shared.title.borrow_mut() = title;
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
    fn crashed_stores_and_takes_info() {
        let shared = SharedState::new();
        *shared.crashed.borrow_mut() = Some(("oops".to_owned(), None));
        let taken = shared.crashed.borrow_mut().take();
        assert_eq!(taken, Some(("oops".to_owned(), None)));
        assert!(shared.crashed.borrow().is_none());
    }
}
