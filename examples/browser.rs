//! Minimal libcosmic browser — demonstrates the full WebViewWidget.
//!
//! Run with:
//!   cargo run --example browser --features browser-example
//!   LIBGL_ALWAYS_SOFTWARE=1 cargo run --example browser --features browser-example

use cosmic_webview::{ServoState, WebViewFrame, WebViewWidget};
use dpi::PhysicalSize;
use libcosmic::app::{Core, Settings, Task};
use libcosmic::executor;
use libcosmic::iced::{Event, Length, Subscription, window};
use libcosmic::prelude::*;
use libcosmic::widget;
use servo::InputEvent;
use url::Url;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = Settings::default().size((1280, 800).into());
    libcosmic::app::run::<Browser>(settings, ())?;
    Ok(())
}

#[derive(Clone, Debug)]
pub enum Message {
    /// Servo needs to spin its event loop.
    ServoWake,
    /// A mouse or keyboard event to forward to Servo.
    Input(InputEvent),
    /// The URL bar gained focus — suspend keyboard forwarding to Servo.
    UrlBarFocused,
    /// The URL bar lost focus — resume keyboard forwarding to Servo.
    UrlBarUnfocused,
    /// The user edited the URL bar.
    UrlBarChanged(String),
    /// The user pressed Enter in the URL bar.
    Navigate,
    GoBack,
    GoForward,
    Reload,
    /// The webview widget has been laid out at a new physical size.
    /// Used to keep Servo's rendering resolution in sync with actual widget bounds.
    WebViewResized(u32, u32),
    /// HiDPI scale factor changed — update coordinate conversion factor.
    ScaleChanged(f32),
}

pub struct Browser {
    core: Core,
    servo: ServoState,
    /// Persistent GPU texture — updated in-place each frame.
    frame: WebViewFrame,
    /// Text currently shown in the URL bar.
    url_bar: String,
    /// Whether the URL bar currently holds iced keyboard focus.
    ///
    /// Driven symmetrically by `on_focus`/`on_unfocus` on the text input.
    /// When `true`, keyboard events are suppressed in the webview widget.
    url_bar_focused: bool,
    /// HiDPI scale factor — for converting logical cursor positions to physical.
    scale: f32,
}

// Newtype so async_channel::Receiver<()> satisfies Hash for Subscription::run_with.
#[derive(Clone)]
struct ServoRx(async_channel::Receiver<()>);

impl std::hash::Hash for ServoRx {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<Self>().hash(state);
    }
}

impl Browser {
    fn try_navigate(&mut self) {
        let raw = self.url_bar.trim().to_owned();
        // Treat bare hostnames as https:// URLs.
        let with_scheme = if raw.contains("://") {
            raw.clone()
        } else {
            format!("https://{raw}")
        };
        if let Ok(url) = Url::parse(&with_scheme) {
            self.url_bar = url.to_string();
            self.servo.navigate(url);
        }
    }
}

impl libcosmic::Application for Browser {
    type Executor = executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = "org.cosmic.WebViewBrowser";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        let initial_url = Url::parse("https://servo.org").expect("valid url");
        let size = PhysicalSize::new(1280u32, 800);
        let (servo, _rx) = ServoState::new(initial_url.clone(), size);
        let app = Browser {
            core,
            servo,
            frame: WebViewFrame::new(),
            url_bar: initial_url.to_string(),
            url_bar_focused: false,
            scale: 1.0,
        };
        (app, Task::none())
    }

    fn subscription(&self) -> Subscription<Message> {
        use libcosmic::iced::event;
        use libcosmic::iced::futures::StreamExt as _;

        let servo_sub = Subscription::run_with(ServoRx(self.servo.waker_rx()), |s| {
            s.0.clone().map(|_| Message::ServoWake)
        });

        let window_sub = event::listen_with(|event, _status, _id| match event {
            Event::Window(window::Event::Rescaled(factor)) => Some(Message::ScaleChanged(factor)),
            _ => None,
        });

        Subscription::batch([servo_sub, window_sub])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ServoWake => {
                self.servo.spin();

                // Only update the URL bar when it is not focused — otherwise
                // Servo's URL reports would overwrite what the user is typing.
                if !self.url_bar_focused
                    && let Some(url) = self.servo.current_url()
                {
                    self.url_bar = url.to_string();
                }

                // Update window title from page title.
                let title = self.servo.title().unwrap_or_default();
                self.set_header_title(title);

                // Push the newest frame into the persistent GPU buffer.
                if let Some(img) = self.servo.current_frame().as_ref() {
                    let (w, h) = img.dimensions();
                    self.frame.update(w, h, img.as_raw());
                }

                // Log crashes — a real browser would show an error page.
                if let Some((reason, _bt)) = self.servo.take_crash() {
                    tracing::error!("WebView crashed: {reason}");
                }
            }
            Message::Input(event) => {
                self.servo.send_input(event);
            }
            Message::UrlBarFocused => {
                self.url_bar_focused = true;
            }
            Message::UrlBarUnfocused => {
                self.url_bar_focused = false;
            }
            Message::UrlBarChanged(text) => {
                self.url_bar = text;
            }
            Message::Navigate => {
                self.try_navigate();
            }
            Message::GoBack => {
                self.servo.go_back();
            }
            Message::GoForward => {
                self.servo.go_forward();
            }
            Message::Reload => {
                self.servo.reload();
            }
            Message::WebViewResized(w, h) => {
                self.servo.resize(PhysicalSize::new(w, h));
            }
            Message::ScaleChanged(factor) => {
                self.scale = factor;
            }
        }
        Task::none()
    }

    // Nav buttons go in the header bar start section.
    fn header_start(&self) -> Vec<Element<'_, Message>> {
        let back = widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
            .on_press_maybe(self.servo.can_go_back().then_some(Message::GoBack));

        let forward = widget::button::icon(widget::icon::from_name("go-next-symbolic"))
            .on_press_maybe(self.servo.can_go_forward().then_some(Message::GoForward));

        let reload = widget::button::icon(widget::icon::from_name("view-refresh-symbolic"))
            .on_press(Message::Reload);

        vec![back.into(), forward.into(), reload.into()]
    }

    // URL bar goes in the header center so it stretches to fill available space.
    fn header_center(&self) -> Vec<Element<'_, Message>> {
        let url_bar = widget::text_input("https://example.com", &self.url_bar)
            .on_focus(Message::UrlBarFocused)
            .on_unfocus(Message::UrlBarUnfocused)
            .on_input(Message::UrlBarChanged)
            .on_submit(|_| Message::Navigate)
            .select_on_focus(true)
            .width(Length::Fill);

        vec![url_bar.into()]
    }

    fn view(&self) -> Element<'_, Message> {
        WebViewWidget::new(&self.frame)
            .scale(self.scale)
            .on_input(Message::Input)
            .on_resize(Message::WebViewResized)
            .keyboard_enabled(!self.url_bar_focused)
            .into()
    }
}
