# astrolabe-webview

> **Note:** This library has no official connection to [System76](https://system76.com/) or the
> [COSMIC Desktop Environment](https://github.com/pop-os/cosmic-epoch). It was originally called
> `cosmic-webview` out of lazy naming, nothing more, and was renamed to make that clear.
>
> This is a personal, experimental project.

A general-purpose WebView widget for the COSMIC desktop, built on [Servo](https://servo.org/)
and the [libcosmic](https://github.com/pop-os/libcosmic) UI toolkit (iced + wgpu). Any libcosmic
application can use it to show web content. The [Astrolabe](https://github.com/snaggen/astrolabe)
browser is built on it, but the widget is not tied to Astrolabe.

## Features

- `ServoState` — manages the Servo engine and a single WebView instance
- `WebViewWidget` — iced widget that renders the WebView via wgpu (GPU path or software fallback)
- Touchpad kinetic (momentum) scrolling, HiDPI, cursor and context-menu integration
- Cookie and session data persistence
- HTML5 media via GStreamer
- Permission request callbacks (storage, camera/mic)
- Site data management (list, clear cookies and storage per domain)
- Tracker/ad-blocking filter API
- Find in page (`find` / `find_next` / `find_prev` / `find_clear`)
- Remote developer tools (Firefox Remote Debugging Protocol)
- Built-in dialog handler for auth, file picker, select element, etc.

## Quick start

```rust
use astrolabe_webview::{ServoState, WebViewWidget, IcedEventLoopWaker};
use dpi::PhysicalSize;
use url::Url;

// Before the first ServoState, configure optional globals:
astrolabe_webview::set_profile_dir(data_dir);   // cookie persistence
astrolabe_webview::set_web_fonts(fonts);         // custom fonts
astrolabe_webview::set_devtools_enabled(6080);   // remote devtools (optional)

// Create a WebView (call once per tab):
let url = Url::parse("https://example.com").unwrap();
let size = PhysicalSize::new(1280_u32, 800);
let (servo, waker_rx) = ServoState::new(url, size, scale_factor);

// In your iced view():
WebViewWidget::new(&servo)

// Forward input events and drive the event loop:
servo.send_input(event);
servo.spin();  // call whenever the waker fires
```

## Build

```bash
cargo build

# Run the bundled single-tab browser example:
cargo run --example browser

# Software rendering:
LIBGL_ALWAYS_SOFTWARE=1 cargo run --example browser
```

Requires a wgpu-capable GPU driver and GStreamer development libraries for media playback.
