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
//! let (state, waker_rx) = ServoState::new(url, size);
//! ```

mod delegate;
pub mod dialogs;
pub(crate) mod input;
pub mod primitive;
mod rendering;
mod state;
mod waker;
mod widget;

// Public API
pub use primitive::WebViewFrame;
pub use rendering::{ActiveRenderingContext, GpuRenderingContext};
pub use state::ServoState;
pub use waker::IcedEventLoopWaker;
pub use widget::WebViewWidget;
