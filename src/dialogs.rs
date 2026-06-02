//! Helper utilities for presenting Servo dialog controls in a libcosmic application.
//!
//! Servo delivers interactive browser dialogs via
//! [`EmbedderControl`](servo::EmbedderControl). Phase 5 handles the two
//! **Must** variants:
//!
//! - [`servo::SimpleDialog`] — `alert()`, `confirm()`, `prompt()`
//! - [`servo::FilePicker`] — `<input type=file>`
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

use servo::{FilePicker, FilterPattern, SimpleDialog};

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

#[cfg(test)]
mod tests {
    use super::*;
    use servo::FilterPattern;

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
}
