//! Minimal libcosmic browser — demonstrates the full WebViewWidget.
//!
//! Run with:
//!   cargo run --example browser --features browser-example
//!   LIBGL_ALWAYS_SOFTWARE=1 cargo run --example browser --features browser-example

use std::collections::HashMap;

use astrolabe_webview::{DialogMsg, ServoState, WebViewWidget};
use dpi::PhysicalSize;
use libcosmic::app::{Core, Settings, Task};
use libcosmic::executor;
use libcosmic::iced::{Event, Length, Size, Subscription, window};
use libcosmic::prelude::*;
use libcosmic::widget;
use libcosmic::widget::menu::{self, action::MenuAction};
use servo::{
    ClipboardAction, ContextMenu, ContextMenuAction, ContextMenuItem, EmbedderControl, InputEvent,
};
use url::Url;

// ── Context menu ──────────────────────────────────────────────────────────────

/// Unified action for both static fallback items and Servo-provided items.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ContextAction {
    // Static fallback actions (used before Servo responds with its items)
    GoBack,
    GoForward,
    Reload,
    Copy,
    Paste,
    // Servo-delegated actions — Servo handles these internally via select()
    Servo(ContextMenuAction),
}

// ContextMenuAction implements PartialEq but not Eq; the enum has no floats or
// other non-reflexive fields, so Eq holds and we declare it manually.
impl Eq for ContextAction {}

impl MenuAction for ContextAction {
    type Message = Message;
    fn message(&self) -> Message {
        match self {
            ContextAction::GoBack => Message::GoBack,
            ContextAction::GoForward => Message::GoForward,
            ContextAction::Reload => Message::Reload,
            ContextAction::Copy => Message::CopySelection,
            ContextAction::Paste => Message::Paste,
            ContextAction::Servo(a) => Message::ServoContextMenuSelected(*a),
        }
    }
}

/// Build the static fallback menu shown while waiting for Servo's items.
fn build_default_menu(can_go_back: bool, can_go_forward: bool) -> Vec<menu::Tree<Message>> {
    let items: Vec<menu::Item<ContextAction, &'static str>> = vec![
        if can_go_back {
            menu::Item::Button("Back", None, ContextAction::GoBack)
        } else {
            menu::Item::ButtonDisabled("Back", None, ContextAction::GoBack)
        },
        if can_go_forward {
            menu::Item::Button("Forward", None, ContextAction::GoForward)
        } else {
            menu::Item::ButtonDisabled("Forward", None, ContextAction::GoForward)
        },
        menu::Item::Button("Reload", None, ContextAction::Reload),
        menu::Item::Divider,
        menu::Item::Button("Copy", None, ContextAction::Copy),
        menu::Item::Button("Paste", None, ContextAction::Paste),
    ];
    menu::items(&HashMap::new(), items)
}

/// Build a context menu from Servo's context-sensitive items.
fn build_servo_menu(items: &[ContextMenuItem]) -> Vec<menu::Tree<Message>> {
    let menu_items: Vec<menu::Item<ContextAction, String>> = items
        .iter()
        .map(|item| match item {
            ContextMenuItem::Item {
                label,
                action,
                enabled,
            } => {
                if *enabled {
                    menu::Item::Button(label.clone(), None, ContextAction::Servo(*action))
                } else {
                    menu::Item::ButtonDisabled(label.clone(), None, ContextAction::Servo(*action))
                }
            }
            ContextMenuItem::Separator => menu::Item::Divider,
        })
        .collect();
    menu::items(&HashMap::new(), menu_items)
}

// ── App ───────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Start the web engine before the window exists, so Servo's constellation
    // and script threads come up while libcosmic builds the window and its GPU
    // context. Start-up-only settings (set_profile_dir, set_devtools_enabled)
    // would have to be applied before this call.
    astrolabe_webview::start_engine();
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
    /// Copy selected text on the page to clipboard.
    CopySelection,
    /// Paste clipboard text into the focused element.
    Paste,
    /// User selected an item from Servo's context menu.
    ServoContextMenuSelected(ContextMenuAction),
    /// Forwarded from the built-in dialog handler in ServoState.
    Dialog(DialogMsg),
    /// The webview widget has been laid out at a new physical size.
    WebViewResized(u32, u32),
    /// HiDPI scale factor changed — update coordinate conversion factor.
    ScaleChanged(f32),
    /// The window opened or was resized; screen geometry needs re-querying.
    WindowGeometryChanged,
    /// Queried monitor size (`None` if unknown) and window size, in logical pixels.
    ScreenGeometry(Option<Size>, Size),
}

pub struct Browser {
    core: Core,
    servo: ServoState,
    /// Text currently shown in the URL bar.
    url_bar: String,
    /// Whether the URL bar currently holds iced keyboard focus.
    url_bar_focused: bool,
    /// HiDPI scale factor — for converting logical cursor positions to physical.
    scale: f32,
    /// Servo's context menu handler, kept alive until the user responds.
    /// Drop sends an automatic dismiss to Servo.
    servo_context_menu: Option<ContextMenu>,
    /// Items from Servo's last ContextMenu, used to build the popup.
    /// Replaced on each right-click; cleared after response.
    servo_context_items: Vec<ContextMenuItem>,
    /// Last observed media-session generation, to log only new media events.
    last_media_generation: u64,
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

    /// Ask the windowing system for the monitor and window size, which come back
    /// as [`Message::ScreenGeometry`].
    fn refresh_screen_geometry(&self) -> Task<Message> {
        let Some(id) = self.core.main_window_id() else {
            return Task::none();
        };
        window::monitor_size(id)
            .then(move |monitor| {
                window::size(id).map(move |size| Message::ScreenGeometry(monitor, size))
            })
            .map(libcosmic::action::app)
    }

    /// Dismiss the pending Servo context menu (if any) without selecting.
    ///
    /// Drop sends the dismiss automatically, so assigning `None` is enough.
    fn dismiss_context_menu(&mut self) {
        self.servo_context_menu = None;
        self.servo_context_items.clear();
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

        // Demonstrate user content (Fas 22): a script that runs at document-start
        // on every page, and a stylesheet applied as user-origin CSS. Set before
        // the first ServoState so the first page load already has them.
        astrolabe_webview::set_user_content(astrolabe_webview::UserContent {
            scripts: vec![astrolabe_webview::UserContentItem {
                source: "console.log('astrolabe-webview userscript ran at document-start');"
                    .to_owned(),
                path: None,
            }],
            stylesheets: vec![astrolabe_webview::UserContentItem {
                source: ":root { filter: hue-rotate(20deg); }".to_owned(),
                path: None,
            }],
        });

        let (servo, _rx) = ServoState::new(initial_url.clone(), size, 1.0);
        let app = Browser {
            core,
            servo,
            url_bar: initial_url.to_string(),
            url_bar_focused: false,
            scale: 1.0,
            servo_context_menu: None,
            servo_context_items: Vec::new(),
            last_media_generation: 0,
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
            Event::Window(window::Event::Opened { .. } | window::Event::Resized(_)) => {
                Some(Message::WindowGeometryChanged)
            }
            _ => None,
        });

        Subscription::batch([servo_sub, window_sub])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ServoWake => {
                self.servo.spin();

                // Demonstrate request_create_new: window.open() and target="_blank"
                // produce a new engine WebView. A real browser adds a tab; this
                // minimal example swaps the visible webview to the newest popup so
                // the adopted WebView can be seen rendering.
                if let Some(new_tab) = self.servo.take_pending_new_tab() {
                    tracing::info!("Adopted a new WebView from window.open()/target=_blank");
                    self.servo = new_tab;
                }

                // Pick up context-sensitive menu items from Servo. These arrive
                // one frame after the right-click. The context_menu overlay reads
                // items on every render, so the popup updates automatically.
                // Other controls are routed to the built-in dialog handler.
                match self.servo.take_dialog() {
                    Some(EmbedderControl::ContextMenu(menu)) => {
                        self.servo_context_items = menu.items().to_vec();
                        self.servo_context_menu = Some(menu);
                    }
                    Some(control) => {
                        if let Some(task) = self.servo.handle_dialog_control(control) {
                            return task.map(|msg| libcosmic::action::app(Message::Dialog(msg)));
                        }
                    }
                    None => {}
                }

                if !self.url_bar_focused
                    && let Some(url) = self.servo.current_url()
                {
                    self.url_bar = url.to_string();
                }

                let title = self.servo.title().unwrap_or_default();
                self.set_header_title(title);

                // No frame copying needed: the engine fills the shared frame
                // buffer directly; the widget picks it up on the next draw.

                if let Some((reason, _bt)) = self.servo.take_crash() {
                    tracing::error!("WebView crashed: {reason}");
                }

                // Log Media Session events (metadata / playback / position) as
                // they change, demonstrating the engine API a real browser would
                // wire to MPRIS.
                let media_generation = self.servo.media_generation();
                if media_generation != self.last_media_generation {
                    self.last_media_generation = media_generation;
                    if let Some(m) = self.servo.media_metadata() {
                        tracing::info!(
                            "Media metadata: title={:?} artist={:?} album={:?}",
                            m.title,
                            m.artist,
                            m.album
                        );
                    }
                    if let Some(state) = self.servo.media_playback_state() {
                        tracing::info!("Media playback state: {state:?}");
                    }
                    if let Some(p) = self.servo.media_position() {
                        tracing::info!(
                            "Media position: {:.1}s / {:.1}s (rate {})",
                            p.position,
                            p.duration,
                            p.playback_rate
                        );
                    }
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
                self.dismiss_context_menu();
                self.servo.go_back();
            }
            Message::GoForward => {
                self.dismiss_context_menu();
                self.servo.go_forward();
            }
            Message::Reload => {
                self.dismiss_context_menu();
                self.servo.reload();
            }
            Message::CopySelection => {
                self.dismiss_context_menu();
                self.servo
                    .send_input(InputEvent::EditingAction(ClipboardAction::Copy));
            }
            Message::Paste => {
                self.dismiss_context_menu();
                self.servo
                    .send_input(InputEvent::EditingAction(ClipboardAction::Paste));
            }
            Message::ServoContextMenuSelected(action) => {
                // Let Servo execute the action (copy link, open image, etc.)
                if let Some(menu) = self.servo_context_menu.take() {
                    menu.select(action);
                }
                self.servo_context_items.clear();
            }
            Message::Dialog(msg) => {
                self.servo.update_dialog(msg);
            }
            Message::WebViewResized(w, h) => {
                self.servo.resize(PhysicalSize::new(w, h));
            }
            Message::ScaleChanged(factor) => {
                self.scale = factor;
                return self.refresh_screen_geometry();
            }
            Message::WindowGeometryChanged => return self.refresh_screen_geometry(),
            // Demonstrate screen geometry (Fas 27): what `window.screen` and
            // `window.outerWidth`/`outerHeight` report. iced measures in logical
            // pixels, the engine in physical ones.
            Message::ScreenGeometry(monitor, window_size) => {
                let to_physical = |size: Size| {
                    PhysicalSize::new(
                        (size.width * self.scale).round() as u32,
                        (size.height * self.scale).round() as u32,
                    )
                };
                let window = to_physical(window_size);
                let screen = monitor.map_or(window, to_physical);
                astrolabe_webview::set_screen_geometry(astrolabe_webview::ScreenGeometry {
                    screen_size: screen,
                    // No work area is available, so panels are not subtracted.
                    available_size: screen,
                    // Wayland does not report where a window sits on screen.
                    window_position: dpi::PhysicalPosition::new(0, 0),
                    window_size: window,
                });
            }
        }
        Task::none()
    }

    fn header_start(&self) -> Vec<Element<'_, Message>> {
        let back = widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
            .on_press_maybe(self.servo.can_go_back().then_some(Message::GoBack));

        let forward = widget::button::icon(widget::icon::from_name("go-next-symbolic"))
            .on_press_maybe(self.servo.can_go_forward().then_some(Message::GoForward));

        let reload = widget::button::icon(widget::icon::from_name("view-refresh-symbolic"))
            .on_press(Message::Reload);

        vec![back.into(), forward.into(), reload.into()]
    }

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

    fn dialog(&self) -> Option<Element<'_, Message>> {
        self.servo.dialog_view().map(|e| e.map(Message::Dialog))
    }

    fn view(&self) -> Element<'_, Message> {
        let webview = WebViewWidget::new(self.servo.frame())
            .scale(self.scale)
            .on_input(Message::Input)
            .on_resize(Message::WebViewResized)
            .keyboard_enabled(!self.url_bar_focused);

        // Use Servo's context-sensitive items when available (arrives ~1 frame
        // after right-click). Falls back to a static browser menu initially.
        let menu_items = if self.servo_context_items.is_empty() {
            build_default_menu(self.servo.can_go_back(), self.servo.can_go_forward())
        } else {
            build_servo_menu(&self.servo_context_items)
        };

        widget::context_menu(webview, Some(menu_items)).into()
    }
}
