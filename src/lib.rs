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

mod delegate;
pub mod dialog_handler;
pub mod dialogs;
#[cfg(all(target_os = "linux", feature = "dmabuf"))]
pub(crate) mod gpu_share;
pub(crate) mod input;
pub mod primitive;
mod rendering;
mod state;
mod waker;
mod widget;

// Public API
pub use dialog_handler::DialogMsg;
pub use primitive::WebViewFrame;
pub use rendering::{ActiveRenderingContext, GpuRenderingContext};
pub use state::{ServoState, set_profile_dir, shutdown_engine};
pub use waker::IcedEventLoopWaker;
pub use widget::{WebViewWidget, cursor_interaction};

// Re-export servo types that embedding applications need directly.
// This removes the need for embedders to add servo as a direct dependency.
pub use servo::{
    ContextMenu, ContextMenuAction, ContextMenuItem, EditingActionEvent, EmbedderControl,
    InputEvent, LoadStatus, PermissionFeature, PermissionRequest,
};
