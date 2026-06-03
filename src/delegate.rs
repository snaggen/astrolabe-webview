use std::cell::{Cell, RefCell};
use std::rc::Rc;

use async_channel::Sender;
use servo::{
    AuthenticationRequest, ConsoleLogLevel, Cursor, DeviceIntRect, EmbedderControl,
    EmbedderControlId, LoadStatus, RenderingContext, RgbaImage, ServoDelegate, ServoError, WebView,
    WebViewDelegate,
};

/// State shared between [`CosmicWebViewDelegate`] and [`crate::state::ServoState`].
pub(crate) struct SharedState {
    pub current_frame: RefCell<Option<RgbaImage>>,
    pub load_status: Cell<LoadStatus>,
    /// Incremented each time [`notify_new_frame_ready`] produces a new frame.
    /// Callers can compare against a stored value to detect actual frame changes
    /// without cloning pixel data on every wake signal.
    pub frame_generation: Cell<u64>,
    /// Pending dialog or picker waiting for the app to respond.
    ///
    /// Only one control can be pending at a time; a new arrival replaces the
    /// previous one (which is dropped, sending its default reply automatically).
    pub pending_dialog: RefCell<Option<EmbedderControl>>,
    /// Pending HTTP authentication request.
    pub pending_auth: RefCell<Option<AuthenticationRequest>>,
    /// Most recent cursor shape reported by the page.
    pub cursor: Cell<Cursor>,
    /// Whether the page is currently in fullscreen mode.
    pub is_fullscreen: Cell<bool>,
    /// Crash info set when a pipeline panics: `(reason, backtrace)`.
    pub crashed: RefCell<Option<(String, Option<String>)>>,
}

impl SharedState {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            current_frame: RefCell::new(None),
            load_status: Cell::new(LoadStatus::Started),
            frame_generation: Cell::new(0),
            pending_dialog: RefCell::new(None),
            pending_auth: RefCell::new(None),
            cursor: Cell::new(Cursor::Default),
            is_fullscreen: Cell::new(false),
            crashed: RefCell::new(None),
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
    waker_tx: Sender<()>,
}

impl CosmicWebViewDelegate {
    pub(crate) fn new(
        rendering_ctx: Rc<dyn RenderingContext>,
        shared: Rc<SharedState>,
        waker_tx: Sender<()>,
    ) -> Self {
        Self {
            rendering_ctx,
            shared,
            waker_tx,
        }
    }
}

impl WebViewDelegate for CosmicWebViewDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        webview.paint();
        let rect = DeviceIntRect::from_size(webview.size().to_i32());
        if let Some(img) = self.rendering_ctx.read_to_image(rect) {
            *self.shared.current_frame.borrow_mut() = Some(img);
            self.shared
                .frame_generation
                .set(self.shared.frame_generation.get().wrapping_add(1));
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
}

/// Handles engine-level errors and forwards them to the tracing subscriber.
pub(crate) struct CosmicServoDelegate;

impl ServoDelegate for CosmicServoDelegate {
    fn notify_error(&self, error: ServoError) {
        tracing::error!("Servo engine error: {error:?}");
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
