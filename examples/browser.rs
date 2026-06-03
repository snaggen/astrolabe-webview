//! Minimal libcosmic browser — verifies that WebViewWidget renders a Servo frame.
//!
//! Run with:
//!   cargo run --example browser --features browser-example
//!   LIBGL_ALWAYS_SOFTWARE=1 cargo run --example browser --features browser-example

use cosmic_webview::{ServoState, WebViewFrame, WebViewWidget};
use dpi::PhysicalSize;
use libcosmic::app::{Core, Settings, Task};
use libcosmic::executor;
use libcosmic::iced::{Event, Subscription, window};
use libcosmic::prelude::*;
use servo::InputEvent;
use url::Url;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = Settings::default();
    libcosmic::app::run::<Browser>(settings, ())?;
    Ok(())
}

#[derive(Clone, Debug)]
pub enum Message {
    ServoWake,
    /// A mouse or keyboard event from the webview widget to forward to Servo.
    Input(InputEvent),
    /// Window was resized to new logical pixel dimensions.
    WindowResized(f32, f32),
    /// HiDPI scale factor changed.
    ScaleChanged(f32),
}

pub struct Browser {
    core: Core,
    servo: ServoState,
    /// Persistent GPU texture buffer — updated in-place, no double-buffering needed.
    frame: WebViewFrame,
    /// Current window size in logical pixels.
    logical_size: (f32, f32),
    /// Current HiDPI scale factor (logical → physical).
    scale: f32,
}

// Newtype wrapper so async_channel::Receiver<()> satisfies the Hash bound
// required by Subscription::run_with.
#[derive(Clone)]
struct ServoRx(async_channel::Receiver<()>);

impl std::hash::Hash for ServoRx {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // One subscription per app; type ID provides stable identity.
        std::any::TypeId::of::<Self>().hash(state);
    }
}

impl Browser {
    /// Computes the current physical size and tells Servo to re-render at that size.
    fn sync_servo_size(&self) {
        let (lw, lh) = self.logical_size;
        let w = (lw * self.scale).round() as u32;
        let h = (lh * self.scale).round() as u32;
        if w > 0 && h > 0 {
            self.servo.resize(PhysicalSize::new(w, h));
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
        // Use a reasonable default; Servo will be resized on the first
        // WindowResized event before any frame is visible.
        let size = PhysicalSize::new(1024u32, 768);
        let url = Url::parse("https://servo.org").expect("valid url");
        let (servo, _rx) = ServoState::new(url, size);
        let app = Browser {
            core,
            servo,
            frame: WebViewFrame::new(),
            logical_size: (1024.0, 768.0),
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
            Event::Window(window::Event::Resized(size)) => {
                Some(Message::WindowResized(size.width, size.height))
            }
            Event::Window(window::Event::Rescaled(factor)) => Some(Message::ScaleChanged(factor)),
            _ => None,
        });

        Subscription::batch([servo_sub, window_sub])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ServoWake => {
                self.servo.spin();

                // Push the newest frame into the persistent GPU buffer.
                if let Some(img) = self.servo.current_frame().as_ref() {
                    let (w, h) = img.dimensions();
                    self.frame.update(w, h, img.as_raw());
                }
            }
            Message::Input(event) => {
                self.servo.send_input(event);
            }
            Message::WindowResized(w, h) => {
                self.logical_size = (w, h);
                self.sync_servo_size();
            }
            Message::ScaleChanged(factor) => {
                self.scale = factor;
                self.sync_servo_size();
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        WebViewWidget::new(&self.frame)
            .scale(self.scale)
            .on_input(Message::Input)
            .into()
    }
}
