//! Ready-to-use handler for all interactive Servo dialog controls.
//!
//! Drop a [`DialogHandler`] into your application model and wire it up with
//! three lines of code — the library takes care of all dialog UI and Servo
//! response calls, including the OS file-picker (when the `xdg-portal` feature
//! is enabled).
//!
//! # Example
//!
//! ```ignore
//! // 1. Store in your app:
//! struct App { dialogs: DialogHandler, servo: ServoState, ... }
//!
//! // 2. In update(), after servo.spin():
//! if let Some(control) = self.servo.take_dialog() {
//!     if let Some(task) = self.dialogs.accept(control) {
//!         return task.map(|msg| libcosmic::action::app(Message::Dialog(msg)));
//!     }
//! }
//!
//! // 3. Forward dialog messages:
//! Message::Dialog(msg) => self.dialogs.update(msg),
//!
//! // 4. In Application::dialog():
//! fn dialog(&self) -> Option<Element<'_, Message>> {
//!     self.dialogs.view().map(|e| e.map(Message::Dialog))
//! }
//! ```

use std::path::PathBuf;

use libcosmic::Element;
use libcosmic::iced::Length;
use libcosmic::widget;
use libcosmic::widget::color_picker::{ColorPickerModel, ColorPickerUpdate};
use servo::{ColorPicker, EmbedderControl, FilePicker, SelectElement, SimpleDialog};

use crate::dialogs;

#[cfg(feature = "xdg-portal")]
use libcosmic::dialog::file_chooser::open as file_dialog;

/// Manages all interactive dialog controls requested by Servo.
///
/// This type is an implementation detail of [`crate::ServoState`]; use
/// [`ServoState::handle_dialog_control`], [`ServoState::update_dialog`], and
/// [`ServoState::dialog_view`] instead.
pub(crate) struct DialogHandler {
    pending_simple: Option<SimpleDialog>,
    simple_input: String,
    pending_color: Option<ColorPicker>,
    color_model: ColorPickerModel,
    pending_select: Option<SelectElement>,
    select_ids: Vec<usize>,
    pending_file_picker: Option<FilePicker>,
}

/// Messages produced by the dialog UI.
///
/// Wrap this in your own `Message` enum and forward to [`DialogHandler::update`].
///
/// ```ignore
/// enum Message {
///     Dialog(DialogMsg),
///     ...
/// }
/// ```
#[derive(Clone, Debug)]
pub enum DialogMsg {
    /// User pressed OK in an alert / confirm / prompt dialog.
    SimpleOk,
    /// User pressed Cancel in a confirm / prompt dialog.
    SimpleCancel,
    /// User typed in the prompt dialog's text field.
    SimpleInput(String),
    /// Internal update from the libcosmic color-picker widget.
    ColorPicker(ColorPickerUpdate),
    /// User clicked an option in the `<select>` list.
    SelectToggle(usize),
    /// User confirmed the multi-select dialog.
    SelectSubmit,
    /// User cancelled the select dialog.
    SelectCancel,
    /// The OS file-picker dialog completed. Internal — produced by the async task
    /// returned from [`DialogHandler::accept`]; forward to [`DialogHandler::update`].
    FilePickerDone(Option<Vec<PathBuf>>),
}

impl Default for DialogHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl DialogHandler {
    pub(crate) fn new() -> Self {
        Self {
            pending_simple: None,
            simple_input: String::new(),
            pending_color: None,
            color_model: ColorPickerModel::new("Hex", "RGB", None, None),
            pending_select: None,
            select_ids: Vec::new(),
            pending_file_picker: None,
        }
    }

    /// Accept an [`EmbedderControl`] from [`crate::ServoState::take_dialog`].
    ///
    /// Returns `Some(task)` only for `FilePicker`: the task opens the OS file
    /// chooser asynchronously and resolves to [`DialogMsg::FilePickerDone`].
    /// Map it into your message type and return it from `update()`:
    ///
    /// ```ignore
    /// if let Some(task) = self.dialogs.accept(control) {
    ///     return task.map(|msg| libcosmic::action::app(Message::Dialog(msg)));
    /// }
    /// ```
    ///
    /// All other controls (`SimpleDialog`, `ColorPicker`, `SelectElement`) are
    /// stored internally. Call [`view`](Self::view) to render them.
    /// `ContextMenu` and remaining variants are dropped (auto-dismissed).
    pub(crate) fn accept(
        &mut self,
        control: EmbedderControl,
    ) -> Option<libcosmic::iced::Task<DialogMsg>> {
        match control {
            EmbedderControl::SimpleDialog(d) => {
                self.simple_input = dialogs::prompt_value(&d).unwrap_or("").to_owned();
                self.pending_simple = Some(d);
                None
            }
            EmbedderControl::ColorPicker(picker) => {
                let initial = picker.current_color().map(|c| libcosmic::iced::Color {
                    r: c.red as f32 / 255.0,
                    g: c.green as f32 / 255.0,
                    b: c.blue as f32 / 255.0,
                    a: 1.0,
                });
                self.color_model = ColorPickerModel::new("Hex", "RGB", initial, initial);
                self.pending_color = Some(picker);
                None
            }
            EmbedderControl::SelectElement(el) => {
                self.select_ids = el.selected_options();
                self.pending_select = Some(el);
                None
            }
            EmbedderControl::FilePicker(picker) => {
                #[cfg(feature = "xdg-portal")]
                let task = {
                    let allow_multiple = picker.allow_select_multiple();
                    self.pending_file_picker = Some(picker);
                    Some(libcosmic::iced::Task::perform(
                        pick_files(allow_multiple),
                        DialogMsg::FilePickerDone,
                    ))
                };
                // Without xdg-portal the picker is dropped here, auto-dismissing it.
                #[cfg(not(feature = "xdg-portal"))]
                let task = {
                    drop(picker);
                    None
                };
                task
            }
            // ContextMenu and remaining variants: caller handles or drops them.
            _ => None,
        }
    }

    /// Handle a [`DialogMsg`], updating internal dialog state and responding to
    /// Servo when the user has made a choice.
    pub(crate) fn update(&mut self, msg: DialogMsg) {
        match msg {
            DialogMsg::SimpleOk => {
                if let Some(dialog) = self.pending_simple.take() {
                    match dialog {
                        SimpleDialog::Prompt(mut p) => {
                            p.set_current_value(&self.simple_input);
                            p.confirm();
                        }
                        other => other.confirm(),
                    }
                    self.simple_input.clear();
                }
            }
            DialogMsg::SimpleCancel => {
                if let Some(dialog) = self.pending_simple.take() {
                    dialog.dismiss();
                    self.simple_input.clear();
                }
            }
            DialogMsg::SimpleInput(text) => {
                self.simple_input = text;
            }
            DialogMsg::ColorPicker(upd) => {
                let apply = matches!(
                    upd,
                    ColorPickerUpdate::AppliedColor | ColorPickerUpdate::ActionFinished
                );
                let cancel = matches!(upd, ColorPickerUpdate::Cancel);
                // Ignore the returned Task<()> — clipboard-copy requires app-level
                // task integration which is out of scope for this handler.
                let _task = self.color_model.update::<()>(upd);

                if apply {
                    if let Some(mut picker) = self.pending_color.take() {
                        if let Some(c) = self.color_model.get_applied_color() {
                            picker.select(Some(servo::RgbColor {
                                red: (c.r * 255.0).round() as u8,
                                green: (c.g * 255.0).round() as u8,
                                blue: (c.b * 255.0).round() as u8,
                            }));
                        }
                        picker.submit();
                    }
                } else if cancel {
                    // Drop sends the original color back to Servo.
                    self.pending_color = None;
                }
            }
            DialogMsg::SelectToggle(id) => {
                if let Some(el) = &self.pending_select {
                    if el.allow_select_multiple() {
                        if let Some(pos) = self.select_ids.iter().position(|&x| x == id) {
                            self.select_ids.remove(pos);
                        } else {
                            self.select_ids.push(id);
                        }
                    } else {
                        if let Some(mut el) = self.pending_select.take() {
                            el.select(vec![id]);
                            el.submit();
                        }
                        self.select_ids.clear();
                    }
                }
            }
            DialogMsg::SelectSubmit => {
                if let Some(mut el) = self.pending_select.take() {
                    el.select(self.select_ids.clone());
                    el.submit();
                }
                self.select_ids.clear();
            }
            DialogMsg::SelectCancel => {
                // Drop submits the original selection back to Servo.
                self.pending_select = None;
                self.select_ids.clear();
            }
            DialogMsg::FilePickerDone(paths) => {
                if let Some(mut picker) = self.pending_file_picker.take() {
                    match paths.as_deref() {
                        Some(p) if !p.is_empty() => {
                            picker.select(p);
                            picker.submit();
                        }
                        _ => picker.dismiss(),
                    }
                }
            }
        }
    }

    /// Build the dialog element for the current pending control.
    ///
    /// Returns `None` when no dialog is active. Map the result into your own
    /// message type and return it from `Application::dialog`:
    ///
    /// ```ignore
    /// fn dialog(&self) -> Option<Element<'_, Message>> {
    ///     self.dialogs.view().map(|e| e.map(Message::Dialog))
    /// }
    /// ```
    pub(crate) fn view(&self) -> Option<Element<'_, DialogMsg>> {
        if let Some(el) = &self.pending_select {
            return Some(self.select_view(el));
        }
        if self.pending_color.is_some() {
            return Some(self.color_view());
        }
        if let Some(pending) = &self.pending_simple {
            return Some(self.simple_view(pending));
        }
        None
    }

    fn select_view(&self, el: &SelectElement) -> Element<'_, DialogMsg> {
        let options = dialogs::flat_options(el);
        let allow_multiple = el.allow_select_multiple();

        let list = options.iter().fold(
            widget::column::with_capacity(options.len()).spacing(4),
            |col, opt| {
                let selected = self.select_ids.contains(&opt.id);
                let label = match &opt.group_label {
                    Some(g) => format!("{g} › {}", opt.label),
                    None => opt.label.clone(),
                };
                let btn = if selected {
                    widget::button::suggested(label)
                } else {
                    widget::button::standard(label)
                };
                let btn = if opt.is_disabled {
                    btn
                } else {
                    btn.on_press(DialogMsg::SelectToggle(opt.id))
                };
                col.push(btn)
            },
        );

        let mut d = widget::dialog()
            .title("Select an option")
            .control(widget::scrollable(list).height(Length::Fixed(240.0)))
            .secondary_action(widget::button::standard("Cancel").on_press(DialogMsg::SelectCancel));

        if allow_multiple {
            d = d.primary_action(widget::button::suggested("OK").on_press(DialogMsg::SelectSubmit));
        }

        d.into()
    }

    fn color_view(&self) -> Element<'_, DialogMsg> {
        self.color_model
            .builder(DialogMsg::ColorPicker)
            .save_label("OK")
            .cancel_label("Cancel")
            .build("Recent colors", "Copy to clipboard", "Copied!")
            .into()
    }

    fn simple_view(&self, dialog: &SimpleDialog) -> Element<'_, DialogMsg> {
        let mut d = widget::dialog()
            .title(dialogs::title(dialog))
            .body(dialog.message().to_owned());

        if dialogs::prompt_value(dialog).is_some() {
            d = d.control(
                widget::text_input("", &self.simple_input).on_input(DialogMsg::SimpleInput),
            );
        }

        d = d.primary_action(widget::button::suggested("OK").on_press(DialogMsg::SimpleOk));

        if dialogs::has_cancel(dialog) {
            d = d.secondary_action(
                widget::button::standard("Cancel").on_press(DialogMsg::SimpleCancel),
            );
        }

        d.into()
    }
}

/// Open the OS file-picker asynchronously via the XDG desktop portal.
///
/// Returns `None` on cancel, or the selected paths on success.
#[cfg(feature = "xdg-portal")]
async fn pick_files(allow_multiple: bool) -> Option<Vec<PathBuf>> {
    tracing::debug!("opening file picker (allow_multiple={allow_multiple})");
    if allow_multiple {
        match file_dialog::Dialog::new()
            .title("Open Files")
            .open_files()
            .await
        {
            Ok(r) => {
                let paths: Vec<PathBuf> = r
                    .urls()
                    .iter()
                    .filter_map(|u| u.to_file_path().ok())
                    .collect();
                tracing::debug!("file picker returned {} path(s)", paths.len());
                Some(paths)
            }
            Err(e) => {
                tracing::error!("file picker error: {e}");
                None
            }
        }
    } else {
        match file_dialog::Dialog::new()
            .title("Open File")
            .open_file()
            .await
        {
            Ok(r) => match r.url().to_file_path() {
                Ok(p) => {
                    tracing::debug!("file picker returned: {}", p.display());
                    Some(vec![p])
                }
                Err(()) => {
                    tracing::error!("file picker returned a non-file URL: {}", r.url());
                    None
                }
            },
            Err(e) => {
                tracing::error!("file picker error: {e}");
                None
            }
        }
    }
}
