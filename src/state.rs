use std::cell::Ref;
use std::rc::Rc;

use dpi::PhysicalSize;
use servo::{
    AuthenticationRequest, Cursor, EmbedderControl, InputEvent, LoadStatus, RgbaImage,
    ServoBuilder, WebViewBuilder,
};
use url::Url;

use crate::delegate::{CosmicServoDelegate, CosmicWebViewDelegate, SharedState};
use crate::rendering::ActiveRenderingContext;
use crate::waker::IcedEventLoopWaker;

/// Owns and drives the Servo engine.
///
/// Create with [`ServoState::new`], then call [`ServoState::spin`] whenever a
/// wake signal arrives (i.e. from `Message::ServoWake` in `App::update`).
///
/// # Example
///
/// ```no_run
/// use cosmic_webview::ServoState;
/// use dpi::PhysicalSize;
/// use url::Url;
///
/// let url = Url::parse("https://example.com").unwrap();
/// let (state, rx) = ServoState::new(url, PhysicalSize::new(1024_u32, 768));
/// ```
pub struct ServoState {
    servo: servo::Servo,
    webview: servo::WebView,
    rendering_ctx: ActiveRenderingContext,
    shared: Rc<SharedState>,
    waker_rx: async_channel::Receiver<()>,
}

impl ServoState {
    /// Creates a Servo instance loaded with `initial_url`.
    ///
    /// Returns the state and a channel receiver. Pass the receiver to an iced
    /// `Subscription` that emits `Message::ServoWake`, then call
    /// [`ServoState::spin`] in `App::update` on each wake.
    pub fn new(initial_url: Url, size: PhysicalSize<u32>) -> (Self, async_channel::Receiver<()>) {
        let rendering_ctx = ActiveRenderingContext::create(size);
        Self::new_with_context(initial_url, rendering_ctx)
    }

    /// Creates a Servo instance with a pre-built rendering context.
    ///
    /// Use this in tests to inject a `SoftwareRenderingContext` directly,
    /// avoiding GPU WebRender paths that may not be available in CI.
    pub(crate) fn new_with_context(
        initial_url: Url,
        rendering_ctx: ActiveRenderingContext,
    ) -> (Self, async_channel::Receiver<()>) {
        // Ignore error if a provider is already installed (e.g. in tests).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let (waker, rx) = IcedEventLoopWaker::new();

        let shared = SharedState::new();
        let delegate = Rc::new(CosmicWebViewDelegate::new(
            rendering_ctx.as_rc(),
            shared.clone(),
            waker.sender(),
        ));

        let servo = ServoBuilder::default()
            .event_loop_waker(Box::new(waker))
            .build();
        servo.setup_logging();
        servo.set_delegate(Rc::new(CosmicServoDelegate));

        let webview = WebViewBuilder::new(&servo, rendering_ctx.as_rc())
            .url(initial_url)
            .delegate(delegate)
            .build();

        let state = Self {
            servo,
            webview,
            rendering_ctx,
            shared,
            waker_rx: rx.clone(),
        };
        (state, rx)
    }

    /// Spins the Servo event loop once, processing any pending events.
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

    pub fn go_back(&self) {
        self.webview.go_back(1);
    }

    pub fn go_forward(&self) {
        self.webview.go_forward(1);
    }

    pub fn reload(&self) {
        self.webview.reload();
    }

    pub fn send_input(&self, event: InputEvent) {
        self.webview.notify_input_event(event);
    }

    /// Returns a reference to the most recently rendered frame, or `None` if
    /// no frame has been received yet.
    pub fn current_frame(&self) -> Ref<'_, Option<RgbaImage>> {
        self.shared.current_frame.borrow()
    }

    pub fn load_status(&self) -> LoadStatus {
        self.shared.load_status.get()
    }

    /// Returns a clone of the wake receiver for use in an iced `Subscription`.
    pub fn waker_rx(&self) -> async_channel::Receiver<()> {
        self.waker_rx.clone()
    }

    /// Takes the pending dialog or file-picker control, if any.
    ///
    /// Call this after [`Self::spin`] to check whether Servo has requested a
    /// dialog. Present the dialog using libcosmic widgets, then call
    /// [`servo::SimpleDialog::confirm`] / [`servo::SimpleDialog::dismiss`] or
    /// [`servo::FilePicker::submit`] / [`servo::FilePicker::dismiss`] when
    /// the user responds. Dropping the control sends the default reply.
    pub fn take_dialog(&self) -> Option<EmbedderControl> {
        self.shared.pending_dialog.borrow_mut().take()
    }

    /// Takes the pending HTTP authentication request, if any.
    ///
    /// Show a username/password dialog, then call
    /// [`servo::AuthenticationRequest::authenticate`] with the credentials, or
    /// drop the request to cancel authentication.
    pub fn take_auth_request(&self) -> Option<AuthenticationRequest> {
        self.shared.pending_auth.borrow_mut().take()
    }

    /// Returns the current cursor shape as last reported by the page.
    ///
    /// The app should update the window cursor whenever this changes.
    pub fn cursor(&self) -> Cursor {
        self.shared.cursor.get()
    }

    /// Returns `true` when the page has requested fullscreen mode.
    ///
    /// The app should transition the window to or from fullscreen accordingly.
    pub fn is_fullscreen(&self) -> bool {
        self.shared.is_fullscreen.get()
    }

    /// Takes the crash info if a pipeline has panicked since the last call.
    ///
    /// Returns `Some((reason, backtrace))`. The app should show a reload
    /// prompt or an error page.
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
        let (state, _rx) = ServoState::new_with_context(url, ctx);

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
