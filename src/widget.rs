use std::marker::PhantomData;

use iced_wgpu::primitive::Renderer as PrimitiveRenderer;
use libcosmic::iced::advanced::widget::Tree;
use libcosmic::iced::advanced::{Layout, Renderer as _, Widget, layout, mouse, renderer};
use libcosmic::iced::{Background, Border, Color, Length, Rectangle, Shadow, Size};

use crate::primitive::{WebViewFrame, WebViewPrimitive};

/// A read-only iced widget that displays the current Servo WebView frame.
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
/// }
///
/// fn view(state: &App) -> libcosmic::Element<'_, Message> {
///     WebViewWidget::new(&state.frame).into()
/// }
/// ```
pub struct WebViewWidget<'a, Message> {
    frame: &'a WebViewFrame,
    width: Length,
    height: Length,
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
}

impl<'a, Message> Widget<Message, libcosmic::Theme, libcosmic::Renderer>
    for WebViewWidget<'a, Message>
{
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
}
