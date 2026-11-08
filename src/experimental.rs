//! Web-platform features that Servo ships but leaves off by default.
//!
//! Every preference Servo 0.8 marks as a feature and leaves off, except those
//! the crate decides itself: the permission-gated APIs [`crate::start_engine`]
//! always enables, WebRTC (see [`crate::set_camera_microphone_enabled`]),
//! geolocation (its permission prompt is unimplemented and hangs) and WebXR
//! layers (WebXR is not built in).

use std::cell::RefCell;

use servo::{PrefValue, Preferences};

/// An experimental web-platform feature, backed by one Servo preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExperimentalFeature {
    /// `document.adoptedStyleSheets` (constructable stylesheets).
    AdoptedStyleSheets,
    /// `navigator.clipboard.readText()` / `writeText()` (Servo lacks `read()` / `write()`).
    AsyncClipboard,
    /// `canvas.captureStream()` from the Media Capture and Streams API.
    CanvasCapture,
    /// CSS container queries.
    ContainerQueries,
    /// The Container Timing performance API.
    ContainerTiming,
    /// The asynchronous `cookieStore` API.
    CookieStore,
    /// The Credential Management API (`navigator.credentials`).
    CredentialManagement,
    /// CSS multi-column layout.
    CssColumns,
    /// CSS writing modes (vertical text).
    CssWritingModes,
    /// Elliptical `border-radius` corners.
    EllipseCorners,
    /// The File and Directory Entries API.
    EntriesApi,
    /// `document.execCommand()`, which many rich-text editors rely on.
    ExecCommand,
    /// The CSS Font Loading API (`FontFace`).
    FontFace,
    /// Variable fonts.
    FontVariations,
    /// IndexedDB client-side storage.
    IndexedDb,
    /// `IntersectionObserver`, used for lazy loading and scroll effects.
    IntersectionObserver,
    /// The Largest Contentful Paint performance API.
    LargestContentfulPaint,
    /// MathML Core.
    MathMl,
    /// `OffscreenCanvas`.
    OffscreenCanvas,
    /// `navigator.registerProtocolHandler()`.
    ProtocolHandlers,
    /// The HTML Sanitizer API.
    Sanitizer,
    /// Service workers.
    ServiceWorker,
    /// `window.visualViewport`.
    VisualViewport,
    /// The Screen Wake Lock API.
    WakeLock,
    /// The Web Animations API (`element.animate()`).
    WebAnimations,
    /// WebGL 2 contexts.
    WebGl2,
    /// WebGPU (`navigator.gpu`).
    WebGpu,
}

impl ExperimentalFeature {
    /// Every feature.
    pub const ALL: &'static [ExperimentalFeature] = &[
        ExperimentalFeature::AdoptedStyleSheets,
        ExperimentalFeature::AsyncClipboard,
        ExperimentalFeature::CanvasCapture,
        ExperimentalFeature::ContainerQueries,
        ExperimentalFeature::ContainerTiming,
        ExperimentalFeature::CookieStore,
        ExperimentalFeature::CredentialManagement,
        ExperimentalFeature::CssColumns,
        ExperimentalFeature::CssWritingModes,
        ExperimentalFeature::EllipseCorners,
        ExperimentalFeature::EntriesApi,
        ExperimentalFeature::ExecCommand,
        ExperimentalFeature::FontFace,
        ExperimentalFeature::FontVariations,
        ExperimentalFeature::IndexedDb,
        ExperimentalFeature::IntersectionObserver,
        ExperimentalFeature::LargestContentfulPaint,
        ExperimentalFeature::MathMl,
        ExperimentalFeature::OffscreenCanvas,
        ExperimentalFeature::ProtocolHandlers,
        ExperimentalFeature::Sanitizer,
        ExperimentalFeature::ServiceWorker,
        ExperimentalFeature::VisualViewport,
        ExperimentalFeature::WakeLock,
        ExperimentalFeature::WebAnimations,
        ExperimentalFeature::WebGl2,
        ExperimentalFeature::WebGpu,
    ];

    /// The Servo preference that switches this feature on.
    ///
    /// Also a stable identifier for storing the user's choice.
    ///
    /// ```
    /// use astrolabe_webview::ExperimentalFeature;
    ///
    /// let name = ExperimentalFeature::IndexedDb.pref_name();
    /// assert_eq!(name, "dom_indexeddb_enabled");
    /// assert_eq!(ExperimentalFeature::from_pref_name(name), Some(ExperimentalFeature::IndexedDb));
    /// ```
    pub fn pref_name(self) -> &'static str {
        match self {
            ExperimentalFeature::AdoptedStyleSheets => "dom_adoptedstylesheet_enabled",
            ExperimentalFeature::AsyncClipboard => "dom_async_clipboard_enabled",
            ExperimentalFeature::CanvasCapture => "dom_canvas_capture_enabled",
            ExperimentalFeature::ContainerQueries => "layout_css_container_queries_enabled",
            ExperimentalFeature::ContainerTiming => "container_timing_enabled",
            ExperimentalFeature::CookieStore => "dom_cookiestore_enabled",
            ExperimentalFeature::CredentialManagement => "dom_credential_management_enabled",
            ExperimentalFeature::CssColumns => "layout_css_columns_enabled",
            ExperimentalFeature::CssWritingModes => "layout_css_writing_mode_enabled",
            ExperimentalFeature::EllipseCorners => "layout_css_ellipse_corners_enabled",
            ExperimentalFeature::EntriesApi => "dom_entries_api_enabled",
            ExperimentalFeature::ExecCommand => "dom_exec_command_enabled",
            ExperimentalFeature::FontFace => "dom_fontface_enabled",
            ExperimentalFeature::FontVariations => "layout_css_font_variations_enabled",
            ExperimentalFeature::IndexedDb => "dom_indexeddb_enabled",
            ExperimentalFeature::IntersectionObserver => "dom_intersection_observer_enabled",
            ExperimentalFeature::LargestContentfulPaint => "largest_contentful_paint_enabled",
            ExperimentalFeature::MathMl => "mathml_core_enabled",
            ExperimentalFeature::OffscreenCanvas => "dom_offscreen_canvas_enabled",
            ExperimentalFeature::ProtocolHandlers => "dom_navigator_protocol_handlers_enabled",
            ExperimentalFeature::Sanitizer => "dom_sanitizer_enabled",
            ExperimentalFeature::ServiceWorker => "dom_serviceworker_enabled",
            ExperimentalFeature::VisualViewport => "dom_visual_viewport_enabled",
            ExperimentalFeature::WakeLock => "dom_wakelock_enabled",
            ExperimentalFeature::WebAnimations => "dom_web_animations_enabled",
            ExperimentalFeature::WebGl2 => "dom_webgl2_enabled",
            ExperimentalFeature::WebGpu => "dom_webgpu_enabled",
        }
    }

    /// The feature whose [`pref_name`](Self::pref_name) is `name`, if any.
    pub fn from_pref_name(name: &str) -> Option<ExperimentalFeature> {
        Self::ALL.iter().copied().find(|f| f.pref_name() == name)
    }
}

thread_local! {
    /// Features switched on. Read when the engine starts and updatable at
    /// runtime via [`set_experimental_feature`].
    static ENABLED: RefCell<Vec<ExperimentalFeature>> = const { RefCell::new(Vec::new()) };
}

/// Switch an experimental feature on or off. All are off by default.
///
/// Call before [`crate::start_engine`] to apply from the first page load;
/// calling afterwards updates the running engine and takes effect on the next
/// page load.
///
/// ```no_run
/// use astrolabe_webview::{ExperimentalFeature, set_experimental_feature};
///
/// for &feature in ExperimentalFeature::ALL {
///     set_experimental_feature(feature, true);
/// }
/// astrolabe_webview::start_engine();
/// ```
pub fn set_experimental_feature(feature: ExperimentalFeature, enabled: bool) {
    ENABLED.with(|list| {
        let mut list = list.borrow_mut();
        list.retain(|&f| f != feature);
        if enabled {
            list.push(feature);
        }
    });
    crate::state::with_engine(|servo| {
        servo.set_preference(feature.pref_name(), PrefValue::Bool(enabled));
    });
}

/// Turns on, in `preferences`, every feature switched on so far.
pub(crate) fn apply(preferences: &mut Preferences) {
    ENABLED.with(|list| {
        for feature in list.borrow().iter() {
            preferences.set_value(feature.pref_name(), PrefValue::Bool(true));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // `Preferences::set_value` panics on an unknown name, so every name must
    // exist in the pinned Servo version. A Servo upgrade that drops one fails
    // here instead of at browser start.
    #[test]
    fn every_pref_name_is_a_servo_bool_preference() {
        for &feature in ExperimentalFeature::ALL {
            assert_eq!(
                Preferences::type_of(feature.pref_name()),
                "bool",
                "{feature:?}"
            );
        }
    }

    // A feature Servo turns on by default has graduated and belongs off this list.
    #[test]
    fn features_are_off_in_servo_by_default() {
        let defaults = Preferences::default();
        for &feature in ExperimentalFeature::ALL {
            let value = defaults.get_value(feature.pref_name());
            assert!(
                matches!(value, PrefValue::Bool(false)),
                "{feature:?} is {value:?}"
            );
        }
    }

    #[test]
    fn pref_names_round_trip_and_are_unique() {
        for &feature in ExperimentalFeature::ALL {
            assert_eq!(
                ExperimentalFeature::from_pref_name(feature.pref_name()),
                Some(feature)
            );
        }
        assert_eq!(
            ExperimentalFeature::from_pref_name("dom_no_such_pref"),
            None
        );
    }

    #[test]
    fn apply_enables_only_switched_on_features() {
        set_experimental_feature(ExperimentalFeature::WebGpu, true);
        set_experimental_feature(ExperimentalFeature::Sanitizer, true);
        set_experimental_feature(ExperimentalFeature::Sanitizer, false);

        let mut preferences = Preferences::default();
        apply(&mut preferences);

        assert!(preferences.dom_webgpu_enabled);
        assert!(!preferences.dom_sanitizer_enabled);
        assert!(!preferences.dom_indexeddb_enabled);
    }
}
