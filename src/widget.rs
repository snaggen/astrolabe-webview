use std::marker::PhantomData;

use iced_wgpu::primitive::Renderer as PrimitiveRenderer;
use libcosmic::iced::advanced::widget::{Tree, tree};
use libcosmic::iced::advanced::{
    Clipboard, Layout, Renderer as _, Shell, Widget, layout, mouse, renderer,
};
use libcosmic::iced::{Background, Border, Color, Event, Length, Rectangle, Shadow, Size};
use servo::InputEvent;

use crate::input;
use crate::primitive::{WebViewFrame, WebViewPrimitive};

/// Per-widget state held in the iced [`Tree`].
///
/// Tracks the last physical size sent to the embedder so that `on_resize`
/// only fires when the layout actually changes.
#[derive(Default)]
struct WebViewState {
    last_phys_size: (u32, u32),
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

        // Notify the embedder when the widget's physical size changes so
        // Servo renders at the exact widget resolution, not the full window size.
        if let Some(on_resize) = &self.on_resize {
            let phys_w = (bounds.width * scale).round() as u32;
            let phys_h = (bounds.height * scale).round() as u32;
            let wstate = state.state.downcast_mut::<WebViewState>();
            if (phys_w, phys_h) != wstate.last_phys_size && phys_w > 0 && phys_h > 0 {
                wstate.last_phys_size = (phys_w, phys_h);
                shell.publish(on_resize(phys_w, phys_h));
            }
        }

        let Some(on_input) = &self.on_input else {
            return;
        };

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
}
