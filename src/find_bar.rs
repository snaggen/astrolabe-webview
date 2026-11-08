use std::sync::LazyLock;

use libcosmic::iced::{Alignment, Length};
use libcosmic::widget;

/// Stable widget ID for the find-bar text input, allowing focus management
/// across re-renders (analogous to `URL_BAR_ID` in embedder applications).
pub static FIND_BAR_ID: LazyLock<widget::Id> = LazyLock::new(|| widget::Id::new("find_bar_input"));

/// The result of a [`ServoState::find`](crate::ServoState::find) operation.
#[derive(Clone, Debug, Default)]
pub struct FindResult {
    /// Index of the current match (1-based). Zero when there are no matches.
    pub current: usize,
    /// Total number of matches in the page.
    pub total: usize,
}

/// Persistent UI state for an in-page text search session.
#[derive(Clone, Debug, Default)]
pub struct FindBarState {
    /// The current search query as typed by the user.
    pub query: String,
    /// Whether `find_next`/`find_prev` wrap around at the ends of the match list.
    pub wrap: bool,
    /// Whether matching is case-sensitive.
    pub case_sensitive: bool,
    /// The most recent result from a find or navigate call.
    pub result: FindResult,
}

/// Messages emitted by [`FindBar::view`].
#[derive(Clone, Debug)]
pub enum FindBarMsg {
    QueryChanged(String),
    Next,
    Prev,
    Close,
    ToggleWrap,
    ToggleCaseSensitive,
}

/// In-page text search bar widget.
///
/// Render this in your application's `view()` whenever find mode is active,
/// mapping [`FindBarMsg`] to your own message type. Call the corresponding
/// [`crate::ServoState`] find methods in your `update()` function.
///
/// # Example
///
/// ```ignore
/// // In view():
/// if let Some(state) = &tab.find_bar {
///     let bar = astrolabe_webview::FindBar::view(state).map(Message::FindBar);
///     // Insert `bar` between toolbar and webview in your layout.
/// }
///
/// // In update():
/// Message::FindBar(FindBarMsg::QueryChanged(q)) => {
///     let opts = state.find_bar.case_sensitive;
///     tab.servo.find(&q, opts, state.find_bar.wrap)
///         .map(Message::FindResult)
/// }
/// ```
pub struct FindBar;

impl FindBar {
    /// Build the find bar element for the given state.
    pub fn view(state: &FindBarState) -> libcosmic::Element<'_, FindBarMsg> {
        let count_label = if state.query.is_empty() {
            String::new()
        } else if state.result.total == 0 {
            "No matches".to_owned()
        } else {
            format!("{} of {}", state.result.current, state.result.total)
        };

        let input = widget::text_input("Search\u{2026}", &state.query)
            .id(FIND_BAR_ID.clone())
            .on_input(FindBarMsg::QueryChanged)
            .on_submit(|_| FindBarMsg::Next)
            .width(Length::Fixed(250.0));

        let prev_btn =
            widget::button::icon(widget::icon::from_name("go-previous-symbolic").size(16))
                .on_press(FindBarMsg::Prev);

        let next_btn = widget::button::icon(widget::icon::from_name("go-next-symbolic").size(16))
            .on_press(FindBarMsg::Next);

        let close_btn =
            widget::button::icon(widget::icon::from_name("window-close-symbolic").size(16))
                .on_press(FindBarMsg::Close);

        let wrap_cb = widget::checkbox(state.wrap)
            .label("Wrap")
            .on_toggle(|_| FindBarMsg::ToggleWrap);

        let case_cb = widget::checkbox(state.case_sensitive)
            .label("Aa")
            .on_toggle(|_| FindBarMsg::ToggleCaseSensitive);

        widget::row::with_children(vec![
            input.into(),
            prev_btn.into(),
            next_btn.into(),
            widget::text(count_label).into(),
            wrap_cb.into(),
            case_cb.into(),
            close_btn.into(),
        ])
        .spacing(8)
        .align_y(Alignment::Center)
        .padding([4, 8])
        .into()
    }
}
