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
}

impl SharedState {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            current_frame: RefCell::new(None),
            load_status: Cell::new(LoadStatus::Started),
        })
    }
}

/// Receives callbacks from Servo and updates shared state.
///
/// All `EmbedderControl` variants (dialogs, file pickers, etc.) are handled by
/// drop, which sends a default reply automatically. Full implementations come
/// in a later phase.
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

    fn show_embedder_control(&self, _webview: WebView, _control: EmbedderControl) {
        // Drop = automatic default reply. Full implementations come in a later phase.
    }

    fn hide_embedder_control(&self, _webview: WebView, _id: EmbedderControlId) {}
}
