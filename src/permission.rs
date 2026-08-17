//! Helpers for working with Servo's [`PermissionFeature`] from embedders.
//!
//! `PermissionFeature` is `Copy` but implements neither `PartialEq` nor a stable
//! string form, so any embedder that wants to compare, persist, or display
//! permission features ends up reimplementing the same matches. These helpers
//! provide them once.

use servo::PermissionFeature;

/// Returns `true` when two features are the same variant.
///
/// `PermissionFeature` does not implement `PartialEq`, so this compares the
/// discriminant.
///
/// ```
/// use cosmic_webview::{PermissionFeature, permission};
/// assert!(permission::features_match(
///     PermissionFeature::Camera,
///     PermissionFeature::Camera,
/// ));
/// assert!(!permission::features_match(
///     PermissionFeature::Camera,
///     PermissionFeature::Microphone,
/// ));
/// ```
pub fn features_match(a: PermissionFeature, b: PermissionFeature) -> bool {
    std::mem::discriminant(&a) == std::mem::discriminant(&b)
}

/// A stable string key identifying a permission feature.
///
/// Suitable as a database key for persisting per-site permission grants. The
/// keys are stable across releases, so stored grants stay valid.
///
/// ```
/// use cosmic_webview::{PermissionFeature, permission};
/// assert_eq!(permission::feature_key(PermissionFeature::Geolocation), "geolocation");
/// ```
pub fn feature_key(feature: PermissionFeature) -> &'static str {
    match feature {
        PermissionFeature::Geolocation => "geolocation",
        PermissionFeature::Notifications => "notifications",
        PermissionFeature::Push => "push",
        PermissionFeature::Midi => "midi",
        PermissionFeature::Camera => "camera",
        PermissionFeature::Microphone => "microphone",
        PermissionFeature::Speaker => "speaker",
        PermissionFeature::DeviceInfo => "device_info",
        PermissionFeature::BackgroundSync => "background_sync",
        PermissionFeature::Bluetooth => "bluetooth",
        PermissionFeature::PersistentStorage => "persistent_storage",
        // The payload names which resource is held awake; today the enum has a
        // single variant, and the key stays stable if more are added.
        PermissionFeature::ScreenWakeLock(_) => "screen_wake_lock",
        PermissionFeature::Gamepad => "gamepad",
    }
}

/// A human-readable label for a permission feature, e.g. for prompts and
/// settings UIs.
pub fn feature_label(feature: PermissionFeature) -> &'static str {
    feature_label_from_key(feature_key(feature)).unwrap_or("Permission")
}

/// The human-readable label for a key returned by [`feature_key`], or `None` if
/// the key is unknown.
pub fn feature_label_from_key(key: &str) -> Option<&'static str> {
    let label = match key {
        "geolocation" => "Location",
        "notifications" => "Notifications",
        "push" => "Push messages",
        "midi" => "MIDI devices",
        "camera" => "Camera",
        "microphone" => "Microphone",
        "speaker" => "Speaker selection",
        "device_info" => "Device information",
        "background_sync" => "Background sync",
        "bluetooth" => "Bluetooth",
        "persistent_storage" => "Persistent storage",
        "screen_wake_lock" => "Screen wake lock",
        "gamepad" => "Game controllers",
        _ => return None,
    };
    Some(label)
}
