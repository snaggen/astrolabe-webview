use std::collections::VecDeque;
use std::marker::PhantomData;
use std::time::{Duration, Instant};

use iced_wgpu::primitive::Renderer as PrimitiveRenderer;
use libcosmic::iced::advanced::widget::{Tree, tree};
use libcosmic::iced::advanced::{
    Clipboard, Layout, Renderer as _, Shell, Widget, layout, mouse, renderer,
};
use libcosmic::iced::{
    Background, Border, Color, Event, Length, Rectangle, Shadow, Size, keyboard, window,
};
use libcosmic::widget::menu;
use servo::{Cursor, InputEvent, WebViewPoint};

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
    /// Button whose release must be dropped because its press opened a link in
    /// a new tab (so the click never reaches Servo as a navigation/autoscroll).
    swallow_release: Option<mouse::Button>,
}

/// Kinetic (momentum) scroll state for mouse-wheel and touchpad coast.
///
/// The release velocity is measured as total displacement over a short trailing
/// time window (see [`VELOCITY_WINDOW`]). This yields the true recent scroll
/// speed at the moment the finger lifts, regardless of event rate or batching —
/// avoiding the lag of a moving average, which would make the coast start
/// noticeably slower than the active drag. After the user stops scrolling,
/// [`next_frame`] drives a smooth exponential deceleration until the speed falls
/// below [`MIN_SPEED`].
///
/// Note: libinput does **not** implement kinetic coasting for applications —
/// that is the responsibility of each widget. This struct handles both
/// `Lines`-delta (mouse wheel) and `Pixels`-delta (touchpad) events.
///
/// # Known limitation: cannot stop the coast by resting fingers on the trackpad
///
/// On other platforms, resting two fingers on the trackpad halts momentum
/// scrolling. That cannot be implemented here, because the COSMIC stack delivers
/// **no event of any kind** when fingers land on the trackpad without moving.
/// Verified against cosmic-comp + pop-os winit 0.31 (tag `cosmic-0.14`) + the
/// libcosmic iced fork:
///
/// - No `WheelScrolled`, `Touch`, or `Mouse(ButtonPressed)` fires on a finger
///   rest. A two-finger rest during a coast produced a multi-second gap of total
///   silence in the event stream.
/// - No gesture event fires either. libinput's *hold* gesture (which is what
///   normally cancels momentum) has no `zwp_pointer_gestures` counterpart, so
///   winit never receives it. Pinch and swipe *are* in that protocol and winit
///   does report them, but iced has no event to carry them — see
///   [`WebViewWidget`]'s note on pinch zoom, and pop-os/libcosmic#1315, which
///   asks for the whole protocol and was filed over this very limitation.
/// - Single-finger taps also produce nothing (tap-to-click clicks do not reach
///   the app). A touchpad is a Wayland *pointer* device, so no `Touch` events
///   ever fire.
/// - Raw `winit::DeviceEvent`s do not help: the Wayland backend only emits
///   `DeviceEvent::PointerMotion` (relative motion, on actual movement) and a
///   sandboxed Wayland client cannot read libinput/evdev directly.
///
/// The zero-delta `Pixels { 0, 0 }` event seen on the wire is libinput's
/// axis_stop on finger *lift* after motion (it arrives within ~30 ms of the last
/// motion, inside `COAST_DELAY`) — it is the natural flick-release, not a
/// finger-down signal, so it must not be used to stop the coast.
///
/// A real fix would require cosmic-comp to expose the hold gesture (e.g. via
/// `wp_pointer_gesture_hold`) and winit + iced to forward it — upstream work
/// outside this crate.
///
/// [`next_frame`]: KineticScroll::next_frame
/// [`VELOCITY_WINDOW`]: KineticScroll::VELOCITY_WINDOW
#[derive(Default)]
struct KineticScroll {
    /// Coast velocity in logical pixels per second, decayed each frame.
    velocity_x: f64,
    velocity_y: f64,
    /// Recent `(time, dx, dy)` wheel samples within [`VELOCITY_WINDOW`], used to
    /// measure the release velocity. Oldest first.
    samples: VecDeque<(Instant, f64, f64)>,
    /// Servo cursor position where kinetic events are applied.
    cursor_pos: Option<WebViewPoint>,
    /// Timestamp of the most recent wheel event.
    last_wheel: Option<Instant>,
    /// Timestamp of the previous animation frame, used to compute `dt`.
    last_frame: Option<Instant>,
}

impl KineticScroll {
    /// Exponential decay rate — velocity reaches ~1 % of initial after
    /// `ln(100) / LAMBDA ≈ 3.07` seconds. Lower values mean less friction and
    /// a longer coast.
    const LAMBDA: f64 = 1.5;
    /// Animation stops when speed drops below this threshold (logical px/s).
    const MIN_SPEED: f64 = 8.0;
    /// Delay after the last wheel event before kinetic coasting begins.
    ///
    /// During this window the user's direct scroll events are still arriving;
    /// waiting avoids double-scrolling at the transition.
    const COAST_DELAY: Duration = Duration::from_millis(50);
    /// Trailing window over which the release velocity is averaged. Long enough
    /// to smooth out per-event jitter, short enough to reflect the speed at the
    /// instant the finger lifts.
    const VELOCITY_WINDOW: Duration = Duration::from_millis(100);
    /// Safety cap on the measured release speed (px/s).
    const MAX_SPEED: f64 = 6000.0;

    /// Records a wheel event and recomputes the release velocity from the
    /// trailing sample window.
    ///
    /// `dx`/`dy` are the scroll deltas in logical pixels; `now` is the event
    /// timestamp (a parameter so the velocity tracker is deterministically
    /// testable).
    fn on_wheel(&mut self, dx: f64, dy: f64, pos: WebViewPoint, now: Instant) {
        self.samples.push_back((now, dx, dy));
        // Drop samples that have aged out of the averaging window.
        while let Some(&(t, _, _)) = self.samples.front() {
            if now.duration_since(t) > Self::VELOCITY_WINDOW {
                self.samples.pop_front();
            } else {
                break;
            }
        }

        let (vx, vy) = self.window_velocity(now);
        self.velocity_x = vx;
        self.velocity_y = vy;
        self.cursor_pos = Some(pos);
        self.last_wheel = Some(now);
    }

    /// Average scroll velocity (px/s) over the trailing sample window.
    ///
    /// Computed as the displacement that arrived *after* the oldest sample,
    /// divided by the span from that sample to `now`. This is the page's true
    /// scroll speed over the window, immune to event batching and to flooring of
    /// per-event intervals. Returns zero with fewer than two samples — a single
    /// isolated event (e.g. one mouse-wheel notch) carries no measurable
    /// momentum and should not coast.
    fn window_velocity(&self, now: Instant) -> (f64, f64) {
        if self.samples.len() < 2 {
            return (0.0, 0.0);
        }
        let span = now.duration_since(self.samples[0].0).as_secs_f64();
        if span < 1e-4 {
            return (0.0, 0.0);
        }
        let (sx, sy) = self
            .samples
            .iter()
            .skip(1)
            .fold((0.0, 0.0), |(ax, ay), &(_, dx, dy)| (ax + dx, ay + dy));
        let vx = (sx / span).clamp(-Self::MAX_SPEED, Self::MAX_SPEED);
        let vy = (sy / span).clamp(-Self::MAX_SPEED, Self::MAX_SPEED);
        (vx, vy)
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
/// Predicate that flags host keyboard shortcuts so they are not forwarded to
/// Servo. See [`WebViewWidget::shortcut_filter`].
type ShortcutFilter<'a> = Box<dyn Fn(&keyboard::Key, keyboard::Modifiers) -> bool + 'a>;

/// Read-only iced widget that displays the current Servo [`WebViewFrame`] and
/// forwards mouse, keyboard, and scroll input to the engine.
///
/// Build it in `view()` from [`ServoState::frame`](crate::ServoState::frame),
/// then attach callbacks with the builder methods.
///
/// # Example
///
/// ```no_run
/// # use cosmic_webview::{InputEvent, ServoState, WebViewWidget};
/// # enum Message { Input(InputEvent), Resized(u32, u32) }
/// # fn view(state: &ServoState) -> WebViewWidget<'_, Message> {
/// WebViewWidget::new(state.frame())
///     .scale(1.0)
///     .on_input(Message::Input)
///     .on_resize(Message::Resized)
/// # }
/// ```
///
/// # Known limitation: no pinch-to-zoom (blocked upstream)
///
/// Touchpad pinch does not reach the page. The compositor and winit both carry
/// it — Wayland's `zwp_pointer_gestures_v1` has a pinch interface, and pop-os
/// winit 0.31 (tag `cosmic-0.14`) turns it into `WindowEvent::PinchGesture` —
/// but iced has no event of its own for gestures, so `iced_winit`'s conversion
/// drops it. There is nothing to hook on this side of the boundary: carrying it
/// through needs a new variant in iced's `Event` plus a conversion arm, both in
/// the libcosmic fork. Zoom therefore comes from the embedder's own controls
/// ([`ServoState::set_page_zoom`]).
///
/// Tracked upstream (both open as of August 2026):
/// - <https://github.com/pop-os/libcosmic/issues/1315> — gesture support missing
///   from libcosmic/iced, covering all of `zwp_pointer_gestures_v1`
/// - <https://github.com/iced-rs/iced/pull/3377> — adds
///   `mouse::Event::PinchGesture` and the conversion iced_winit is missing
///   (15 lines); pop-os/iced would still have to pick it up
pub struct WebViewWidget<'a, Message> {
    frame: &'a WebViewFrame,
    width: Length,
    height: Length,
    /// HiDPI scale factor — converts logical cursor positions to device pixels.
    scale: f32,
    /// When `false`, keyboard events are not forwarded to Servo.
    keyboard_enabled: bool,
    /// Current cursor shape as reported by the page via [`ServoState::cursor`].
    cursor: Cursor,
    /// Context menu items shown on right-click. When set, the widget wraps
    /// itself in libcosmic's `context_menu` (and re-applies the page cursor,
    /// which that wrapper would otherwise swallow) on conversion to `Element`.
    context_menu: Option<Vec<menu::Tree<Message>>>,
    /// URL of the hyperlink under the cursor, from [`ServoState::hovered_link`].
    /// Required for the middle-click "open in new tab" gesture.
    hovered_link: Option<String>,
    /// Fired with `(physical_width, physical_height)` when the widget's layout
    /// size changes. Use this to call [`ServoState::resize`] with exact bounds
    /// rather than the full window size.
    on_resize: Option<Box<dyn Fn(u32, u32) -> Message + 'a>>,
    /// Callback that wraps a [`InputEvent`] into the app's `Message` type.
    on_input: Option<Box<dyn Fn(InputEvent) -> Message + 'a>>,
    /// Fired with a link URL when the user middle-clicks or Ctrl+clicks a link,
    /// for the embedder to open it in a new tab.
    on_open_link: Option<Box<dyn Fn(String) -> Message + 'a>>,
    /// Predicate identifying host keyboard shortcuts. When it returns `true` for
    /// a key press, the event is handled by the embedder (e.g. Ctrl+= zoom) and
    /// is not forwarded to Servo, so the focused page element never sees it.
    is_shortcut: Option<ShortcutFilter<'a>>,
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
            cursor: Cursor::Default,
            context_menu: None,
            hovered_link: None,
            on_resize: None,
            on_input: None,
            on_open_link: None,
            is_shortcut: None,
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

    /// Registers a predicate that identifies host keyboard shortcuts.
    ///
    /// When it returns `true` for a key press, the event is treated as a window
    /// shortcut (e.g. Ctrl+= to zoom) and is *not* forwarded to Servo, so the
    /// focused page element does not also receive the keystroke. The embedder
    /// still handles the shortcut through its own event subscription.
    pub fn shortcut_filter(
        mut self,
        f: impl Fn(&keyboard::Key, keyboard::Modifiers) -> bool + 'a,
    ) -> Self {
        self.is_shortcut = Some(Box::new(f));
        self
    }

    /// Whether `event` is a host keyboard shortcut that must not reach Servo.
    fn is_host_shortcut(&self, event: &keyboard::Event) -> bool {
        let Some(is_shortcut) = &self.is_shortcut else {
            return false;
        };
        match event {
            keyboard::Event::KeyPressed { key, modifiers, .. }
            | keyboard::Event::KeyReleased { key, modifiers, .. } => is_shortcut(key, *modifiers),
            _ => false,
        }
    }

    /// Sets the current cursor shape as reported by
    /// [`ServoState::cursor`](crate::ServoState::cursor).
    ///
    /// Call this each frame so that the widget returns the correct
    /// [`mouse::Interaction`] when the page requests a non-default cursor
    /// (e.g. a pointer over a link). Defaults to [`Cursor::Default`].
    pub fn cursor(mut self, cursor: Cursor) -> Self {
        self.cursor = cursor;
        self
    }

    /// Sets the context menu items shown when the user right-clicks the page.
    ///
    /// The widget wraps itself in libcosmic's `context_menu` when converted to
    /// an [`Element`](libcosmic::Element), and re-applies the page cursor that
    /// the wrapper would otherwise discard — no extra plumbing needed in the
    /// embedding application.
    pub fn context_menu(mut self, items: Vec<menu::Tree<Message>>) -> Self {
        self.context_menu = Some(items);
        self
    }

    /// Sets the URL of the hyperlink currently under the cursor, from
    /// [`ServoState::hovered_link`](crate::ServoState::hovered_link).
    ///
    /// Required for the middle-click / Ctrl+click "open in new tab" gesture
    /// registered with [`on_open_link`](Self::on_open_link).
    pub fn hovered_link(mut self, url: Option<String>) -> Self {
        self.hovered_link = url;
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
    /// [`ServoState::send_input`](crate::ServoState::send_input).
    pub fn on_input(mut self, f: impl Fn(InputEvent) -> Message + 'a) -> Self {
        self.on_input = Some(Box::new(f));
        self
    }

    /// Registers a callback fired with a link URL when the user middle-clicks or
    /// Ctrl+clicks a hyperlink, so the embedder can open it in a new tab.
    ///
    /// Requires the hovered link to be supplied via
    /// [`hovered_link`](Self::hovered_link). The triggering click is not
    /// forwarded to Servo, so the current page does not also navigate.
    pub fn on_open_link(mut self, f: impl Fn(String) -> Message + 'a) -> Self {
        self.on_open_link = Some(Box::new(f));
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

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &libcosmic::Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            cursor_interaction(self.cursor)
        } else {
            mouse::Interaction::None
        }
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
            wstate.kinetic.on_wheel(dx, dy, pos, Instant::now());
            shell.request_redraw();
        }

        // Middle-click on a link opens it in a new tab. The press is consumed
        // (and its release dropped) so Servo does not also navigate the current
        // page or start middle-button autoscroll.
        if let Event::Mouse(mouse::Event::ButtonPressed(button)) = event
            && let Some(on_open_link) = &self.on_open_link
            && cursor.is_over(bounds)
        {
            // Only middle-click opens a link in a new tab here. Ctrl+click is
            // handled by the engine itself (it treats the link as target=_blank),
            // so duplicating it would open two tabs.
            let new_tab_gesture = matches!(button, mouse::Button::Middle);
            if new_tab_gesture && let Some(url) = &self.hovered_link {
                shell.publish(on_open_link(url.clone()));
                wstate.swallow_release = Some(*button);
                return;
            }
        }
        if let Event::Mouse(mouse::Event::ButtonReleased(button)) = event
            && wstate.swallow_release == Some(*button)
        {
            wstate.swallow_release = None;
            return;
        }

        // Forward the event to Servo unchanged.
        let servo_event = match event {
            Event::Mouse(me) => {
                let pos_in_bounds = cursor.position_in(bounds);
                input::mouse_event_to_servo(me, pos_in_bounds, scale)
            }
            // Bare modifier keys are forwarded at the window level (see
            // `ServoState::send_modifier_key`) so a release is never missed while
            // this widget is off-screen; forwarding them here too would double them.
            Event::Keyboard(ke)
                if self.keyboard_enabled
                    && !self.is_host_shortcut(ke)
                    && !input::is_bare_modifier_key(ke) =>
            {
                input::keyboard_event_to_servo(ke)
            }
            _ => None,
        };

        if let Some(e) = servo_event {
            shell.publish(on_input(e));
        }
    }
}

/// Maps a Servo [`Cursor`] to the closest iced [`mouse::Interaction`].
///
/// [`WebViewWidget`] applies this automatically — both via
/// `Widget::mouse_interaction` and around its built-in context-menu wrapper.
/// Use this function only when wrapping the webview in an additional container
/// that does not forward `mouse_interaction` from its child; re-apply the
/// cursor outside it with a `mouse_area`:
///
/// ```ignore
/// widget::mouse_area(some_wrapper(webview))
///     .interaction(cosmic_webview::cursor_interaction(servo.cursor()))
/// ```
///
/// Diagonal resize cursors are approximated: NE/SW → `ResizingDiagonallyUp`,
/// NW/SE → `ResizingDiagonallyDown`. Vertical-text falls back to `Text`.
pub fn cursor_interaction(cursor: Cursor) -> mouse::Interaction {
    match cursor {
        Cursor::None => mouse::Interaction::Hidden,
        Cursor::Default => mouse::Interaction::None,
        Cursor::Pointer => mouse::Interaction::Pointer,
        Cursor::ContextMenu => mouse::Interaction::ContextMenu,
        Cursor::Help => mouse::Interaction::Help,
        Cursor::Progress => mouse::Interaction::Progress,
        Cursor::Wait => mouse::Interaction::Wait,
        Cursor::Cell => mouse::Interaction::Cell,
        Cursor::Crosshair => mouse::Interaction::Crosshair,
        Cursor::Text | Cursor::VerticalText => mouse::Interaction::Text,
        Cursor::Alias => mouse::Interaction::Alias,
        Cursor::Copy => mouse::Interaction::Copy,
        Cursor::Move => mouse::Interaction::Move,
        Cursor::NoDrop => mouse::Interaction::NoDrop,
        Cursor::NotAllowed => mouse::Interaction::NotAllowed,
        Cursor::Grab => mouse::Interaction::Grab,
        Cursor::Grabbing => mouse::Interaction::Grabbing,
        Cursor::EResize | Cursor::WResize | Cursor::EwResize => {
            mouse::Interaction::ResizingHorizontally
        }
        Cursor::NResize | Cursor::SResize | Cursor::NsResize => {
            mouse::Interaction::ResizingVertically
        }
        Cursor::NeResize | Cursor::SwResize | Cursor::NeswResize => {
            mouse::Interaction::ResizingDiagonallyUp
        }
        Cursor::NwResize | Cursor::SeResize | Cursor::NwseResize => {
            mouse::Interaction::ResizingDiagonallyDown
        }
        Cursor::ColResize => mouse::Interaction::ResizingColumn,
        Cursor::RowResize => mouse::Interaction::ResizingRow,
        Cursor::AllScroll => mouse::Interaction::AllScroll,
        Cursor::ZoomIn => mouse::Interaction::ZoomIn,
        Cursor::ZoomOut => mouse::Interaction::ZoomOut,
    }
}

impl<'a, Message: Clone + 'static> From<WebViewWidget<'a, Message>>
    for libcosmic::Element<'a, Message>
{
    fn from(mut widget: WebViewWidget<'a, Message>) -> Self {
        let cursor = widget.cursor;
        let Some(items) = widget.context_menu.take() else {
            return libcosmic::iced::core::Element::new(widget);
        };

        // ContextMenu does not forward mouse_interaction from its child, so the
        // page cursor the inner widget reports would be lost. Re-apply it with
        // a mouse_area outside the wrapper.
        libcosmic::widget::mouse_area(libcosmic::widget::context_menu(
            libcosmic::iced::core::Element::new(widget),
            Some(items),
        ))
        .interaction(cursor_interaction(cursor))
        .into()
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

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn kinetic_not_coasting_immediately_after_wheel() {
        let mut k = KineticScroll::default();
        k.on_wheel(0.0, 38.0, test_point(), Instant::now());
        // Coast delay has not elapsed yet.
        assert!(!k.is_coasting());
    }

    #[test]
    fn kinetic_single_event_carries_no_momentum() {
        let mut k = KineticScroll::default();
        // A single isolated event has no measurable time span, so no coast.
        k.on_wheel(0.0, 38.0, test_point(), Instant::now());
        assert_eq!(k.speed(), 0.0);
    }

    #[test]
    fn kinetic_window_velocity_matches_release_speed() {
        let mut k = KineticScroll::default();
        let base = Instant::now();
        // Three samples 10 ms apart, 30 px each. Displacement after the first
        // sample is 60 px over a 20 ms span → 3000 px/s.
        k.on_wheel(0.0, 30.0, test_point(), base);
        k.on_wheel(0.0, 30.0, test_point(), base + ms(10));
        k.on_wheel(0.0, 30.0, test_point(), base + ms(20));
        assert!(
            (k.speed() - 3000.0).abs() < 1.0,
            "coast should start at the release speed, got {}",
            k.speed()
        );
    }

    #[test]
    fn kinetic_evicts_stale_samples() {
        let mut k = KineticScroll::default();
        let base = Instant::now();
        k.on_wheel(0.0, 30.0, test_point(), base);
        k.on_wheel(0.0, 30.0, test_point(), base + ms(10));
        assert!(k.speed() > 0.0);
        // A later event past the window evicts the old samples, leaving one
        // sample — which carries no measurable velocity.
        k.on_wheel(0.0, 60.0, test_point(), base + ms(200));
        assert_eq!(k.speed(), 0.0);
    }

    #[test]
    fn kinetic_stop_resets_state() {
        let mut k = KineticScroll::default();
        let base = Instant::now();
        k.on_wheel(0.0, 30.0, test_point(), base);
        k.on_wheel(0.0, 30.0, test_point(), base + ms(10));
        assert!(k.should_continue_animating());

        k.stop();

        assert!(!k.should_continue_animating());
        assert!(!k.is_coasting());
        assert_eq!(k.velocity_x, 0.0);
        assert_eq!(k.velocity_y, 0.0);
        assert!(k.samples.is_empty());
    }

    #[test]
    fn kinetic_next_frame_returns_none_before_coast() {
        let mut k = KineticScroll::default();
        let base = Instant::now();
        k.on_wheel(0.0, 30.0, test_point(), base);
        k.on_wheel(0.0, 30.0, test_point(), base + ms(10));
        // next_frame should return None immediately — coast hasn't started yet.
        assert!(k.next_frame(Instant::now()).is_none());
    }

    #[test]
    fn kinetic_should_continue_after_wheel() {
        let mut k = KineticScroll::default();
        k.on_wheel(0.0, 38.0, test_point(), Instant::now());
        // Immediately after a wheel event the animation loop must stay alive so
        // it is present when the coast phase begins.
        assert!(k.should_continue_animating());
    }
}
