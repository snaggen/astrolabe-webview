use std::marker::PhantomData;
use std::time::{Duration, Instant};

use iced_wgpu::primitive::Renderer as PrimitiveRenderer;
use libcosmic::iced::advanced::widget::{Tree, tree};
use libcosmic::iced::advanced::{
    Clipboard, Layout, Renderer as _, Shell, Widget, layout, mouse, renderer,
};
use libcosmic::iced::{Background, Border, Color, Event, Length, Rectangle, Shadow, Size, window};
use servo::{InputEvent, WebViewPoint};

use crate::input;
use crate::primitive::{WebViewFrame, WebViewPrimitive};

/// Per-widget state held in the iced [`Tree`].
///
/// Tracks the last physical size sent to the embedder so that `on_resize`
/// only fires when the layout actually changes, and holds kinetic scroll state.
#[derive(Default)]
struct WebViewState {
    last_phys_size: (u32, u32),
    kinetic: KineticScroll,
}

/// Kinetic (momentum) scroll state for mouse-wheel and touchpad coast.
///
/// Velocity is estimated as an exponentially-weighted moving average of the
/// instantaneous scroll speed derived from incoming wheel events.  After the
/// user stops scrolling, [`next_frame`] drives a smooth exponential deceleration
/// until the speed falls below [`MIN_SPEED`].
///
/// Note: libinput does **not** implement kinetic coasting for applications —
/// that is the responsibility of each widget. This struct handles both
/// `Lines`-delta (mouse wheel) and `Pixels`-delta (touchpad) events.
///
/// [`next_frame`]: KineticScroll::next_frame
#[derive(Default)]
struct KineticScroll {
    /// Running scroll velocity in logical pixels per second.
    velocity_x: f64,
    velocity_y: f64,
    /// Servo cursor position where kinetic events are applied.
    cursor_pos: Option<WebViewPoint>,
    /// Timestamp of the most recent wheel event.
    last_wheel: Option<Instant>,
    /// Timestamp of the previous animation frame, used to compute `dt`.
    last_frame: Option<Instant>,
}

impl KineticScroll {
    /// Exponential decay rate — velocity reaches ~1 % of initial after
    /// `ln(100) / LAMBDA ≈ 1.84` seconds.
    const LAMBDA: f64 = 2.5;
    /// Animation stops when speed drops below this threshold (logical px/s).
    const MIN_SPEED: f64 = 8.0;
    /// Delay after the last wheel event before kinetic coasting begins.
    ///
    /// During this window the user's direct scroll events are still arriving;
    /// waiting avoids double-scrolling at the transition.
    const COAST_DELAY: Duration = Duration::from_millis(50);
    /// EWMA weight applied to the incoming instantaneous velocity sample.
    /// Higher values track recent speed more closely but are more jittery.
    const ALPHA: f64 = 0.4;
    /// Cap on instantaneous speed (px/s) to prevent extreme values from
    /// very rapid events or very short inter-event intervals.
    const MAX_SPEED: f64 = 4000.0;

    /// Records a wheel event and updates the running velocity estimate.
    ///
    /// `dx`/`dy` are the scroll deltas in logical pixels.
    fn on_wheel(&mut self, dx: f64, dy: f64, pos: WebViewPoint) {
        let now = Instant::now();
        // Clamp dt so that the first event (no prior timestamp) and very slow
        // deliberate clicks don't produce unrealistically large velocity samples.
        let dt = self
            .last_wheel
            .map(|t| (now - t).as_secs_f64().clamp(0.008, 0.5))
            .unwrap_or(0.1);

        let vx = (dx / dt).clamp(-Self::MAX_SPEED, Self::MAX_SPEED);
        let vy = (dy / dt).clamp(-Self::MAX_SPEED, Self::MAX_SPEED);
        self.velocity_x = Self::ALPHA * vx + (1.0 - Self::ALPHA) * self.velocity_x;
        self.velocity_y = Self::ALPHA * vy + (1.0 - Self::ALPHA) * self.velocity_y;
        self.cursor_pos = Some(pos);
        self.last_wheel = Some(now);
    }

    /// Returns `true` if the coast phase has begun (the last wheel event was
    /// more than [`COAST_DELAY`] ago).
    fn is_coasting(&self) -> bool {
        self.last_wheel
            .map(|t| t.elapsed() >= Self::COAST_DELAY)
            .unwrap_or(false)
    }

    /// Current scroll speed in logical px/s.
    fn speed(&self) -> f64 {
        (self.velocity_x.powi(2) + self.velocity_y.powi(2)).sqrt()
    }

    /// Returns `true` if the animation loop should keep requesting redraws.
    ///
    /// Stays `true` during the pre-coast window (so the loop is alive when
    /// coasting actually starts) and during the active coast phase until speed
    /// drops below [`MIN_SPEED`].
    fn should_continue_animating(&self) -> bool {
        self.last_wheel
            .map(|t| t.elapsed() < Self::COAST_DELAY || self.speed() >= Self::MIN_SPEED)
            .unwrap_or(false)
    }

    /// Advances one animation frame.  Returns `(dx, dy, pos)` to scroll this
    /// frame, or `None` if the coast phase has not yet started or has ended.
    fn next_frame(&mut self, now: Instant) -> Option<(f64, f64, WebViewPoint)> {
        let pos = self.cursor_pos?;
        if !self.is_coasting() || self.speed() < Self::MIN_SPEED {
            return None;
        }

        // Clamp dt so a stalled animation (window hidden, etc.) doesn't cause
        // a huge jump when it resumes.
        let dt = self
            .last_frame
            .map(|t| (now - t).as_secs_f64().min(0.05))
            .unwrap_or(0.016);
        self.last_frame = Some(now);

        let decay = (-Self::LAMBDA * dt).exp();
        self.velocity_x *= decay;
        self.velocity_y *= decay;

        if self.speed() < Self::MIN_SPEED {
            self.velocity_x = 0.0;
            self.velocity_y = 0.0;
            return None;
        }

        Some((self.velocity_x * dt, self.velocity_y * dt, pos))
    }

    /// Immediately stops any in-progress kinetic scroll and resets all state.
    fn stop(&mut self) {
        *self = KineticScroll::default();
    }
}

/// A read-only iced widget that displays the current Servo WebView frame and
/// forwards mouse and keyboard input to Servo.
///
/// Create it in your application's `view()` by passing a reference to the
/// [`WebViewFrame`] stored in your app state. When no frame has been rendered
/// yet, the widget shows a dark-grey loading placeholder.
///
/// # Example
///
/// ```ignore
/// use cosmic_webview::{ServoState, WebViewFrame, WebViewWidget};
///
/// struct App {
///     servo: ServoState,
///     frame: WebViewFrame,
///     scale: f32,
/// }
///
/// fn view(state: &App) -> libcosmic::Element<'_, Message> {
///     WebViewWidget::new(&state.frame)
///         .scale(state.scale)
///         .on_input(Message::Input)
///         .into()
/// }
/// ```
pub struct WebViewWidget<'a, Message> {
    frame: &'a WebViewFrame,
    width: Length,
    height: Length,
    /// HiDPI scale factor — converts logical cursor positions to device pixels.
    scale: f32,
    /// When `false`, keyboard events are not forwarded to Servo.
    keyboard_enabled: bool,
    /// Fired with `(physical_width, physical_height)` when the widget's layout
    /// size changes. Use this to call [`ServoState::resize`] with exact bounds
    /// rather than the full window size.
    on_resize: Option<Box<dyn Fn(u32, u32) -> Message + 'a>>,
    /// Callback that wraps a [`InputEvent`] into the app's `Message` type.
    on_input: Option<Box<dyn Fn(InputEvent) -> Message + 'a>>,
    _phantom: PhantomData<Message>,
}

impl<'a, Message> WebViewWidget<'a, Message> {
    /// Creates a new [`WebViewWidget`] bound to the given frame.
    ///
    /// Shows a dark placeholder until the first Servo frame is available.
    pub fn new(frame: &'a WebViewFrame) -> Self {
        Self {
            frame,
            width: Length::Fill,
            height: Length::Fill,
            scale: 1.0,
            keyboard_enabled: true,
            on_resize: None,
            on_input: None,
            _phantom: PhantomData,
        }
    }

    /// Sets the widget width. Defaults to [`Length::Fill`].
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    /// Sets the widget height. Defaults to [`Length::Fill`].
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }

    /// Sets the HiDPI scale factor used to convert logical cursor coordinates
    /// to Servo device pixels. Defaults to `1.0`.
    pub fn scale(mut self, scale: f32) -> Self {
        self.scale = scale;
        self
    }

    /// Controls whether keyboard events are forwarded to Servo.
    ///
    /// Set to `false` while a URL bar or other text field in the embedding
    /// app has focus, so that typing goes to the right place. Defaults to
    /// `true`.
    pub fn keyboard_enabled(mut self, enabled: bool) -> Self {
        self.keyboard_enabled = enabled;
        self
    }

    /// Registers a callback that fires with the widget's physical pixel size
    /// whenever the layout changes.
    ///
    /// Use this to keep Servo's rendering resolution in sync with the actual
    /// widget bounds — which may differ from the full window size if the app
    /// has a header bar or other chrome.
    pub fn on_resize(mut self, f: impl Fn(u32, u32) -> Message + 'a) -> Self {
        self.on_resize = Some(Box::new(f));
        self
    }

    /// Registers a callback that fires whenever a mouse or keyboard event
    /// occurs inside the widget.
    ///
    /// The callback converts a Servo [`InputEvent`] into the app's `Message`
    /// type. The app should forward the event to Servo via
    /// [`ServoState::send_input`].
    pub fn on_input(mut self, f: impl Fn(InputEvent) -> Message + 'a) -> Self {
        self.on_input = Some(Box::new(f));
        self
    }
}

impl<'a, Message> Widget<Message, libcosmic::Theme, libcosmic::Renderer>
    for WebViewWidget<'a, Message>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<WebViewState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(WebViewState::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(self.width, self.height)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &libcosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(self.width, self.height, Size::ZERO))
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut libcosmic::Renderer,
        _theme: &libcosmic::Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();

        if self.frame.has_frame() {
            PrimitiveRenderer::draw_primitive(
                renderer,
                bounds,
                WebViewPrimitive::from_frame(self.frame),
            );
        } else {
            // Dark placeholder shown while the first frame loads.
            renderer.fill_quad(
                renderer::Quad {
                    bounds,
                    border: Border::default(),
                    shadow: Shadow::default(),
                    snap: true,
                },
                Background::Color(Color::from_rgb(0.08, 0.08, 0.08)),
            );
        }
    }

    fn update(
        &mut self,
        state: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &libcosmic::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let scale = self.scale;
        let wstate = state.state.downcast_mut::<WebViewState>();

        // Notify the embedder when the widget's physical size changes so
        // Servo renders at the exact widget resolution, not the full window size.
        if let Some(on_resize) = &self.on_resize {
            let phys_w = (bounds.width * scale).round() as u32;
            let phys_h = (bounds.height * scale).round() as u32;
            if (phys_w, phys_h) != wstate.last_phys_size && phys_w > 0 && phys_h > 0 {
                wstate.last_phys_size = (phys_w, phys_h);
                shell.publish(on_resize(phys_w, phys_h));
            }
        }

        let Some(on_input) = &self.on_input else {
            return;
        };

        // Drive the kinetic scroll animation on each display refresh.
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            if let Some((dx, dy, pos)) = wstate.kinetic.next_frame(*now) {
                shell.publish(on_input(input::kinetic_scroll_event(dx, dy, pos)));
            }
            if wstate.kinetic.should_continue_animating() {
                shell.request_redraw();
            }
            return;
        }

        // A mouse button press cancels any in-progress kinetic coast so the
        // user's click is not mixed with residual momentum.
        if matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_))) {
            wstate.kinetic.stop();
        }

        // Accumulate scroll velocity for kinetic coasting.  Both Lines (mouse
        // wheel) and Pixels (touchpad) events are handled: libinput does not
        // implement kinetic coasting itself — that is the widget's job.
        if let Event::Mouse(mouse::Event::WheelScrolled { delta }) = event
            && let Some(p) = cursor.position_in(bounds)
        {
            let pos = input::servo_point(p, scale);
            let (dx, dy) = match delta {
                mouse::ScrollDelta::Lines { x, y } => {
                    (*x as f64 * input::LINE_PX, *y as f64 * input::LINE_PX)
                }
                mouse::ScrollDelta::Pixels { x, y } => (
                    *x as f64 * input::TOUCHPAD_SCROLL_SCALE,
                    *y as f64 * input::TOUCHPAD_SCROLL_SCALE,
                ),
            };
            wstate.kinetic.on_wheel(dx, dy, pos);
            shell.request_redraw();
        }

        // Forward the event to Servo unchanged.
        let servo_event = match event {
            Event::Mouse(me) => {
                let pos_in_bounds = cursor.position_in(bounds);
                input::mouse_event_to_servo(me, pos_in_bounds, scale)
            }
            Event::Keyboard(ke) if self.keyboard_enabled => input::keyboard_event_to_servo(ke),
            _ => None,
        };

        if let Some(e) = servo_event {
            shell.publish(on_input(e));
        }
    }
}

impl<'a, Message: 'a> From<WebViewWidget<'a, Message>> for libcosmic::Element<'a, Message> {
    fn from(widget: WebViewWidget<'a, Message>) -> Self {
        libcosmic::iced::core::Element::new(widget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dimensions_are_fill() {
        let frame = WebViewFrame::new();
        let w: WebViewWidget<'_, ()> = WebViewWidget::new(&frame);
        let size = Widget::<(), libcosmic::Theme, libcosmic::Renderer>::size(&w);
        assert_eq!(size.width, Length::Fill);
        assert_eq!(size.height, Length::Fill);
    }

    #[test]
    fn custom_dimensions_are_stored() {
        let frame = WebViewFrame::new();
        let w: WebViewWidget<'_, ()> = WebViewWidget::new(&frame)
            .width(Length::Fixed(800.0))
            .height(Length::Fixed(600.0));
        let size = Widget::<(), libcosmic::Theme, libcosmic::Renderer>::size(&w);
        assert_eq!(size.width, Length::Fixed(800.0));
        assert_eq!(size.height, Length::Fixed(600.0));
    }

    #[test]
    fn new_with_empty_frame_is_valid() {
        let frame = WebViewFrame::new();
        let _w: WebViewWidget<'_, ()> = WebViewWidget::new(&frame);
    }

    #[test]
    fn scale_is_stored() {
        let frame = WebViewFrame::new();
        let w: WebViewWidget<'_, ()> = WebViewWidget::new(&frame).scale(2.0);
        assert!((w.scale - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn on_input_sets_callback() {
        let frame = WebViewFrame::new();
        let w: WebViewWidget<'_, ()> = WebViewWidget::new(&frame).on_input(|_| ());
        assert!(w.on_input.is_some());
    }

    // ── KineticScroll ──────────────────────────────────────────────────────────

    fn test_point() -> WebViewPoint {
        input::servo_point(libcosmic::iced::Point { x: 0.0, y: 0.0 }, 1.0)
    }

    #[test]
    fn kinetic_not_coasting_immediately_after_wheel() {
        let mut k = KineticScroll::default();
        k.on_wheel(0.0, 38.0, test_point());
        // Coast delay has not elapsed yet.
        assert!(!k.is_coasting());
    }

    #[test]
    fn kinetic_single_slow_click_builds_low_velocity() {
        let mut k = KineticScroll::default();
        // First event — dt defaults to 0.1 s (slow click).
        k.on_wheel(0.0, 38.0, test_point());
        // Expected: ALPHA * (38/0.1) = 0.3 * 380 = 114 px/s
        // A single slow click shouldn't feel like a fast flick.
        assert!(
            k.speed() < 200.0,
            "speed should be moderate for a single click"
        );
        assert!(k.speed() > 0.0);
    }

    #[test]
    fn kinetic_rapid_spin_builds_higher_velocity() {
        let mut k = KineticScroll::default();
        // Simulate several rapid wheel events (each with default 100ms dt).
        for _ in 0..8 {
            k.on_wheel(0.0, 38.0, test_point());
        }
        // After many rapid events velocity should approach the EWMA ceiling.
        // With dt≈0.1 the instantaneous sample is 380 px/s; the EWMA converges
        // toward 380.  Eight events with ALPHA=0.3 bring it close to that.
        assert!(
            k.speed() > 200.0,
            "speed should be high after rapid spinning, got {}",
            k.speed()
        );
    }

    #[test]
    fn kinetic_stop_resets_state() {
        let mut k = KineticScroll::default();
        k.on_wheel(0.0, 38.0, test_point());
        assert!(k.should_continue_animating());

        k.stop();

        assert!(!k.should_continue_animating());
        assert!(!k.is_coasting());
        assert_eq!(k.velocity_x, 0.0);
        assert_eq!(k.velocity_y, 0.0);
    }

    #[test]
    fn kinetic_next_frame_returns_none_before_coast() {
        let mut k = KineticScroll::default();
        k.on_wheel(0.0, 38.0, test_point());
        // next_frame should return None immediately — coast hasn't started yet.
        assert!(k.next_frame(Instant::now()).is_none());
    }

    #[test]
    fn kinetic_should_continue_after_wheel() {
        let mut k = KineticScroll::default();
        k.on_wheel(0.0, 38.0, test_point());
        // Immediately after a wheel event the animation loop must stay alive so
        // it is present when the coast phase begins.
        assert!(k.should_continue_animating());
    }
}
