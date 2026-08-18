//! WebDriver server, behind the `webdriver` feature.
//!
//! Servo ships the protocol server itself; what it cannot do is answer the
//! commands that are about *windows and tabs*, since it does not know how the
//! embedder arranges them. So the server runs on its own thread and posts
//! [`WebDriverCommandMsg`]s here, the embedder drains them with
//! [`WebDriverCommands::try_recv`], answers the ones it owns, and hands the rest
//! to [`ServoState::execute_webdriver_command`], which forwards them into the
//! engine.
//!
//! [`ServoState::execute_webdriver_command`]: crate::ServoState::execute_webdriver_command

use crossbeam_channel::{Receiver, unbounded};
use servo::WebDriverCommandMsg;

/// Commands from the WebDriver server, waiting to be answered.
///
/// Drain this on a regular tick: a client blocks on its HTTP request until the
/// browser answers, so a command left in the queue is a hung test.
pub struct WebDriverCommands {
    receiver: Receiver<WebDriverCommandMsg>,
}

impl WebDriverCommands {
    /// Takes the next pending command, or `None` when the queue is empty.
    pub fn try_recv(&self) -> Option<WebDriverCommandMsg> {
        self.receiver.try_recv().ok()
    }
}

/// Builds the rectangle the window commands answer with, in logical pixels.
///
/// Saves embedders from depending on euclid just to name the type: the shape is
/// Servo's, and the origin is always zero because Wayland never tells a client
/// where its window sits.
pub fn window_rect(width: i32, height: i32) -> servo::DeviceIndependentIntRect {
    servo::DeviceIndependentIntRect::from_origin_and_size(
        euclid::Point2D::origin(),
        euclid::Size2D::new(width, height),
    )
}

/// The viewport size the size command answers with, in logical pixels.
///
/// Same reason as [`window_rect`]: the unit type is Servo's, not the embedder's.
pub fn viewport_size(
    width: f32,
    height: f32,
) -> euclid::Size2D<f32, servo::DeviceIndependentPixel> {
    euclid::Size2D::new(width, height)
}

/// Starts the WebDriver server on `port`.
///
/// Call after the first [`ServoState::new`], which starts the engine this needs
/// a waker and preferences from; returns `None` before that. The server listens
/// on all interfaces, as Servo's own does — only start it when automation is
/// actually wanted.
///
/// [`ServoState::new`]: crate::ServoState::new
pub fn start_webdriver(port: u16) -> Option<WebDriverCommands> {
    let (waker, preferences) = crate::state::webdriver_engine_handles()?;
    let (sender, receiver) = unbounded();
    webdriver_server::start_server(port, sender, Box::new(waker), preferences);
    Some(WebDriverCommands { receiver })
}
