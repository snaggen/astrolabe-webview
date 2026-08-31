#![deny(warnings)]
//! Servo-based WebView widget for the COSMIC Desktop environment.
//!
//! # Example
//!
//! ```no_run
//! use cosmic_webview::ServoState;
//! use url::Url;
//! use dpi::PhysicalSize;
//!
//! let url = Url::parse("https://example.com").unwrap();
//! let size = PhysicalSize::new(1024_u32, 768);
//! let (state, waker_rx) = ServoState::new(url, size, 1.0);
//! ```

pub(crate) mod delegate;
pub(crate) mod dialog_handler;
pub mod dialogs;
pub mod find_bar;
#[cfg(feature = "gamepad")]
mod gamepad;
#[cfg(all(target_os = "linux", feature = "dmabuf"))]
pub(crate) mod gpu_share;
pub(crate) mod input;
pub mod permission;
pub mod primitive;
mod rendering;
mod state;
mod waker;
#[cfg(feature = "webdriver")]
pub mod webdriver;
mod widget;

// Public API
pub use delegate::ContentFilterFn;
pub use dialog_handler::DialogMsg;
pub use find_bar::{FIND_BAR_ID, FindBar, FindBarMsg, FindBarState, FindResult};
pub use primitive::WebViewFrame;
pub use rendering::{ActiveRenderingContext, GpuRenderingContext};
pub use state::{
    DevtoolsRequest, ScreenGeometry, ServoState, ThemePreference, UserContent, UserContentItem,
    WebFonts, clear_all_cookies, clear_site_data, list_site_data, set_camera_microphone_enabled,
    set_devtools_enabled, set_profile_dir, set_screen_geometry, set_user_content, set_web_fonts,
    shutdown_engine, start_engine, take_devtools_request,
};
pub use waker::IcedEventLoopWaker;
pub use widget::{WebViewWidget, cursor_interaction};

// Re-export servo types that embedding applications need directly.
// This removes the need for embedders to add servo as a direct dependency.
pub use servo::{
    AllowOrDenyRequest, BluetoothDeviceDescription, BluetoothDeviceSelectionRequest, ContextMenu,
    ContextMenuAction, ContextMenuItem, EditingActionEvent, EmbedderControl, InputEvent,
    LoadStatus, MediaMetadata, MediaPositionState, MediaSessionActionType,
    MediaSessionPlaybackState, Notification, PermissionFeature, PermissionRequest, SiteData, Theme,
};
#[cfg(feature = "webdriver")]
pub use servo::{
    DeviceIndependentIntRect, GenericSender, NewWindowTypeHint, ScreenshotCaptureError,
    WebDriverCommandMsg, WebDriverLoadStatus, WebDriverUserPrompt, WebDriverUserPromptAction,
    WebViewId,
};
