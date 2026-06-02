use std::cell::{Cell, RefCell};
use std::rc::Rc;

use async_channel::Sender;
use servo::{
    DeviceIntRect, EmbedderControl, EmbedderControlId, LoadStatus, RenderingContext, RgbaImage,
    WebView, WebViewDelegate,
};

/// State shared between [`CosmicWebViewDelegate`] and [`crate::state::ServoState`].
pub(crate) struct SharedState {
    pub current_frame: RefCell<Option<RgbaImage>>,
    pub load_status: Cell<LoadStatus>,
    /// Pending dialog or file-picker control waiting for the app to respond.
    ///
    /// Only one dialog can be pending at a time; a new arrival while one is
    /// outstanding drops the earlier one (auto-reply via `Drop`).
    pub pending_dialog: RefCell<Option<EmbedderControl>>,
}

impl SharedState {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            current_frame: RefCell::new(None),
            load_status: Cell::new(LoadStatus::Started),
            pending_dialog: RefCell::new(None),
        })
    }
}

/// Receives callbacks from Servo and updates shared state.
///
/// [`EmbedderControl::SimpleDialog`] and [`EmbedderControl::FilePicker`]
/// variants are stored in [`SharedState::pending_dialog`] for the app to
/// retrieve via [`crate::state::ServoState::take_dialog`]. All other
/// `EmbedderControl` variants are dropped, which sends a default reply
/// automatically.
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
        }
        let _ = self.waker_tx.try_send(());
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        self.shared.load_status.set(status);
    }

    fn show_embedder_control(&self, _webview: WebView, control: EmbedderControl) {
        match control {
            // Queue dialogs and file pickers for the app to present.
            EmbedderControl::SimpleDialog(_) | EmbedderControl::FilePicker(_) => {
                *self.shared.pending_dialog.borrow_mut() = Some(control);
                let _ = self.waker_tx.try_send(());
            }
            // All other controls: drop = automatic default reply.
            _ => {}
        }
    }

    fn hide_embedder_control(&self, _webview: WebView, _id: EmbedderControlId) {}
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
}
