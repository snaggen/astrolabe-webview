//! Helper utilities for presenting Servo dialog controls in a libcosmic application.
//!
//! Servo delivers interactive browser dialogs via
//! [`EmbedderControl`](servo::EmbedderControl). This module covers all five
//! variants that require user interaction:
//!
//! | Variant | Web trigger | Helpers |
//! |---|---|---|
//! | [`servo::SimpleDialog`] | `alert()` / `confirm()` / `prompt()` | [`title`], [`has_cancel`], [`prompt_value`] |
//! | [`servo::FilePicker`] | `<input type=file>` | [`filter_globs`], [`cancel_file_picker`] |
//! | [`servo::ColorPicker`] | `<input type=color>` | [`rgb_to_hex`] |
//! | [`servo::SelectElement`] | `<select>` | [`flat_options`] |
//! | [`servo::ContextMenu`] | right-click | [`action_label`] |
//!
//! The embedder (your libcosmic `App`) is responsible for rendering the actual
//! dialog widgets. The functions here extract the information the app needs and
//! document the expected response call.
//!
//! # Workflow
//!
//! ```ignore
//! // In App::update, after calling state.spin():
//! if let Some(control) = state.take_dialog() {
//!     match control {
//!         EmbedderControl::SimpleDialog(dialog) => {
//!             let title = dialogs::title(&dialog);
//!             let message = dialog.message().to_owned();
//!             self.dialog = Some(PendingDialog::Simple(dialog, message, title));
//!         }
//!         EmbedderControl::FilePicker(picker) => {
//!             self.dialog = Some(PendingDialog::File(picker));
//!         }
//!         _ => {} // other variants dropped → default reply
//!     }
//! }
//! ```

use servo::{
    ContextMenuAction, ContextMenuItem, FilePicker, FilterPattern, RgbColor, SelectElement,
    SelectElementOptionOrOptgroup, SimpleDialog,
};

/// Returns a short, localizable title for a [`SimpleDialog`].
///
/// Maps to `"Alert"`, `"Confirm"`, or `"Prompt"` depending on the variant.
pub fn title(dialog: &SimpleDialog) -> &'static str {
    match dialog {
        SimpleDialog::Alert(_) => "Alert",
        SimpleDialog::Confirm(_) => "Confirm",
        SimpleDialog::Prompt(_) => "Prompt",
    }
}

/// Returns `true` if the dialog should show a Cancel button.
///
/// Alert dialogs have only an OK button. Confirm and Prompt dialogs have
/// both OK and Cancel.
pub fn has_cancel(dialog: &SimpleDialog) -> bool {
    matches!(dialog, SimpleDialog::Confirm(_) | SimpleDialog::Prompt(_))
}

/// Returns the current text-field value for a [`SimpleDialog::Prompt`],
/// or `None` for Alert and Confirm dialogs.
///
/// The app should call [`servo::PromptDialog::set_current_value`] as the
/// user edits the field, then call [`servo::SimpleDialog::confirm`] or
/// [`servo::SimpleDialog::dismiss`] when the dialog closes.
pub fn prompt_value(dialog: &SimpleDialog) -> Option<&str> {
    match dialog {
        SimpleDialog::Prompt(p) => Some(p.current_value()),
        _ => None,
    }
}

/// Converts Servo [`FilterPattern`]s into glob strings suitable for a file
/// chooser dialog (e.g. `"*.pdf"`, `"*.png"`).
///
/// Each [`FilterPattern`] wraps a bare extension string without the leading
/// dot. This function prepends `"*."` to produce the standard glob form.
///
/// # Example
///
/// ```
/// # use servo::FilterPattern;
/// # use cosmic_webview::dialogs::filter_globs;
/// let patterns = vec![FilterPattern("png".to_owned()), FilterPattern("jpg".to_owned())];
/// assert_eq!(filter_globs(&patterns), vec!["*.png", "*.jpg"]);
/// ```
pub fn filter_globs(patterns: &[FilterPattern]) -> Vec<String> {
    patterns
        .iter()
        .map(|FilterPattern(ext)| format!("*.{ext}"))
        .collect()
}

/// Consume the file picker as a cancellation (no files selected).
///
/// Equivalent to calling [`FilePicker::dismiss`]; provided as an explicit
/// named function to make intent clear in app code.
pub fn cancel_file_picker(picker: FilePicker) {
    picker.dismiss();
}

// ── ColorPicker ──────────────────────────────────────────────────────────────

/// Formats a [`RgbColor`] as an HTML hex colour string (`#rrggbb`).
///
/// Use this when initialising a colour-picker widget with the current value
/// from [`servo::ColorPicker::current_color`].
///
/// # Example
///
/// ```
/// # use servo::RgbColor;
/// # use cosmic_webview::dialogs::rgb_to_hex;
/// let color = RgbColor { red: 255, green: 128, blue: 0 };
/// assert_eq!(rgb_to_hex(color), "#ff8000");
/// ```
pub fn rgb_to_hex(color: RgbColor) -> String {
    format!("#{:02x}{:02x}{:02x}", color.red, color.green, color.blue)
}

// ── SelectElement ─────────────────────────────────────────────────────────────

/// A flattened representation of a single `<option>` for display in a list widget.
///
/// Produced by [`flat_options`].
pub struct FlatSelectOption {
    /// The option identifier to pass to [`servo::SelectElement::select`].
    pub id: usize,
    /// Display label for the option.
    pub label: String,
    /// Whether the option is currently selected.
    pub is_selected: bool,
    /// Whether the option is disabled (should be shown greyed-out).
    pub is_disabled: bool,
    /// Label of the containing `<optgroup>`, if any.
    pub group_label: Option<String>,
}

/// Flattens a [`SelectElement`]'s option tree into a list suitable for a list widget.
///
/// Optgroups are inlined; each inner option carries the group label in
/// [`FlatSelectOption::group_label`]. The [`FlatSelectOption::id`] values match
/// the indices expected by [`servo::SelectElement::select`].
///
/// # Example
///
/// ```ignore
/// for opt in dialogs::flat_options(&select_el) {
///     if opt.is_selected { /* highlight */ }
///     println!("{}", opt.label);
/// }
/// ```
pub fn flat_options(el: &SelectElement) -> Vec<FlatSelectOption> {
    let selected = el.selected_options();
    let mut result = Vec::new();
    for item in el.options() {
        match item {
            SelectElementOptionOrOptgroup::Option(opt) => {
                result.push(FlatSelectOption {
                    id: opt.id,
                    label: opt.label.clone(),
                    is_selected: selected.contains(&opt.id),
                    is_disabled: opt.is_disabled,
                    group_label: None,
                });
            }
            SelectElementOptionOrOptgroup::Optgroup { label, options } => {
                for opt in options {
                    result.push(FlatSelectOption {
                        id: opt.id,
                        label: opt.label.clone(),
                        is_selected: selected.contains(&opt.id),
                        is_disabled: opt.is_disabled,
                        group_label: Some(label.clone()),
                    });
                }
            }
        }
    }
    result
}

// ── ContextMenu ───────────────────────────────────────────────────────────────

/// Returns a localizable display label for a [`ContextMenuAction`].
///
/// # Example
///
/// ```
/// # use servo::ContextMenuAction;
/// # use cosmic_webview::dialogs::action_label;
/// assert_eq!(action_label(ContextMenuAction::Copy), "Copy");
/// assert_eq!(action_label(ContextMenuAction::Reload), "Reload");
/// ```
pub fn action_label(action: ContextMenuAction) -> &'static str {
    match action {
        ContextMenuAction::GoBack => "Go Back",
        ContextMenuAction::GoForward => "Go Forward",
        ContextMenuAction::Reload => "Reload",
        ContextMenuAction::CopyLink => "Copy Link",
        ContextMenuAction::OpenLinkInNewWebView => "Open Link in New Tab",
        ContextMenuAction::CopyImageLink => "Copy Image Link",
        ContextMenuAction::OpenImageInNewView => "Open Image in New Tab",
        ContextMenuAction::Cut => "Cut",
        ContextMenuAction::Copy => "Copy",
        ContextMenuAction::Paste => "Paste",
        ContextMenuAction::SelectAll => "Select All",
    }
}

/// Returns the label of a context menu item, or `None` for separators.
///
/// Separators should be rendered as horizontal dividers in the menu.
pub fn menu_item_label(item: &ContextMenuItem) -> Option<&str> {
    match item {
        ContextMenuItem::Item { label, .. } => Some(label.as_str()),
        ContextMenuItem::Separator => None,
    }
}

/// Returns `true` if a context menu item is enabled and can be activated.
///
/// Disabled items should be shown greyed-out and not respond to clicks.
pub fn menu_item_enabled(item: &ContextMenuItem) -> bool {
    match item {
        ContextMenuItem::Item { enabled, .. } => *enabled,
        ContextMenuItem::Separator => false,
    }
}

/// Returns the action associated with a context menu item, or `None` for separators.
pub fn menu_item_action(item: &ContextMenuItem) -> Option<ContextMenuAction> {
    match item {
        ContextMenuItem::Item { action, .. } => Some(*action),
        ContextMenuItem::Separator => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use servo::{ContextMenuAction, ContextMenuItem, FilterPattern, RgbColor};

    #[test]
    fn filter_globs_prepends_star_dot() {
        let patterns = vec![
            FilterPattern("pdf".to_owned()),
            FilterPattern("docx".to_owned()),
        ];
        assert_eq!(filter_globs(&patterns), vec!["*.pdf", "*.docx"]);
    }

    #[test]
    fn filter_globs_empty() {
        assert!(filter_globs(&[]).is_empty());
    }

    #[test]
    fn rgb_to_hex_formats_correctly() {
        assert_eq!(
            rgb_to_hex(RgbColor {
                red: 255,
                green: 128,
                blue: 0
            }),
            "#ff8000"
        );
        assert_eq!(
            rgb_to_hex(RgbColor {
                red: 0,
                green: 0,
                blue: 0
            }),
            "#000000"
        );
        assert_eq!(
            rgb_to_hex(RgbColor {
                red: 255,
                green: 255,
                blue: 255
            }),
            "#ffffff"
        );
    }

    #[test]
    fn action_label_covers_all_variants() {
        let cases = [
            (ContextMenuAction::GoBack, "Go Back"),
            (ContextMenuAction::GoForward, "Go Forward"),
            (ContextMenuAction::Reload, "Reload"),
            (ContextMenuAction::CopyLink, "Copy Link"),
            (
                ContextMenuAction::OpenLinkInNewWebView,
                "Open Link in New Tab",
            ),
            (ContextMenuAction::CopyImageLink, "Copy Image Link"),
            (
                ContextMenuAction::OpenImageInNewView,
                "Open Image in New Tab",
            ),
            (ContextMenuAction::Cut, "Cut"),
            (ContextMenuAction::Copy, "Copy"),
            (ContextMenuAction::Paste, "Paste"),
            (ContextMenuAction::SelectAll, "Select All"),
        ];
        for (action, expected) in cases {
            assert_eq!(action_label(action), expected, "action {action:?}");
        }
    }

    #[test]
    fn menu_item_label_returns_none_for_separator() {
        assert!(menu_item_label(&ContextMenuItem::Separator).is_none());
    }

    #[test]
    fn menu_item_label_returns_label_for_item() {
        let item = ContextMenuItem::Item {
            label: "Copy".to_owned(),
            action: ContextMenuAction::Copy,
            enabled: true,
        };
        assert_eq!(menu_item_label(&item), Some("Copy"));
    }

    #[test]
    fn menu_item_enabled_false_for_separator() {
        assert!(!menu_item_enabled(&ContextMenuItem::Separator));
    }

    #[test]
    fn menu_item_enabled_respects_item_flag() {
        let enabled = ContextMenuItem::Item {
            label: "Cut".to_owned(),
            action: ContextMenuAction::Cut,
            enabled: true,
        };
        let disabled = ContextMenuItem::Item {
            label: "Paste".to_owned(),
            action: ContextMenuAction::Paste,
            enabled: false,
        };
        assert!(menu_item_enabled(&enabled));
        assert!(!menu_item_enabled(&disabled));
    }
}
