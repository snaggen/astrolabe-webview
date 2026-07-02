//! Conversion from iced input events to Servo [`InputEvent`]s.
//!
//! Coordinates from iced are in *logical* pixels. Servo expects *physical*
//! device pixels, so every position is multiplied by the HiDPI scale factor
//! before being forwarded.

use std::str::FromStr as _;

use libcosmic::iced::{Point, keyboard, mouse};
use servo::{
    Code, DevicePoint, InputEvent, Key as KtKey, KeyState, KeyboardEvent as ServoKeyboardEvent,
    Location as KtLocation, Modifiers as KtModifiers, MouseButton as ServoMouseButton,
    MouseButtonAction, MouseButtonEvent, MouseLeftViewportEvent, MouseMoveEvent, NamedKey,
    WebViewPoint, WheelDelta, WheelEvent, WheelMode,
};

// ── Mouse ─────────────────────────────────────────────────────────────────────

/// Converts an iced mouse button to a Servo [`ServoMouseButton`].
pub(crate) fn servo_mouse_button(button: mouse::Button) -> ServoMouseButton {
    match button {
        mouse::Button::Left => ServoMouseButton::Left,
        mouse::Button::Middle => ServoMouseButton::Middle,
        mouse::Button::Right => ServoMouseButton::Right,
        mouse::Button::Back => ServoMouseButton::Back,
        mouse::Button::Forward => ServoMouseButton::Forward,
        mouse::Button::Other(n) => ServoMouseButton::Other(n),
    }
}

/// Converts a logical-pixel position to a `WebViewPoint` in device pixels.
pub(crate) fn servo_point(pos: Point, scale: f32) -> WebViewPoint {
    WebViewPoint::from(DevicePoint::new(pos.x * scale, pos.y * scale))
}

/// Maps an iced mouse event to a Servo [`InputEvent`], returning `None` for
/// events that require the cursor to be inside the widget when it is not.
///
/// `pos_in_bounds` is the cursor position relative to the widget top-left
/// in logical pixels, or `None` if the cursor is outside the widget.
pub(crate) fn mouse_event_to_servo(
    event: &mouse::Event,
    pos_in_bounds: Option<Point>,
    scale: f32,
) -> Option<InputEvent> {
    match event {
        mouse::Event::CursorMoved { .. } => {
            let point = servo_point(pos_in_bounds?, scale);
            Some(InputEvent::MouseMove(MouseMoveEvent::new(point)))
        }
        mouse::Event::ButtonPressed(button) => {
            let point = servo_point(pos_in_bounds?, scale);
            Some(InputEvent::MouseButton(MouseButtonEvent::new(
                MouseButtonAction::Down,
                servo_mouse_button(*button),
                point,
            )))
        }
        mouse::Event::ButtonReleased(button) => {
            let point = servo_point(pos_in_bounds?, scale);
            Some(InputEvent::MouseButton(MouseButtonEvent::new(
                MouseButtonAction::Up,
                servo_mouse_button(*button),
                point,
            )))
        }
        mouse::Event::WheelScrolled { delta } => {
            let point = servo_point(pos_in_bounds?, scale);
            Some(scroll_to_wheel(delta, point))
        }
        mouse::Event::CursorLeft => Some(InputEvent::MouseLeftViewport(
            MouseLeftViewportEvent::default(),
        )),
        // CursorEntered has no useful action in Servo.
        mouse::Event::CursorEntered => None,
    }
}

/// Standard CSS line height in pixels, used when the platform reports scroll in lines.
pub(crate) const LINE_PX: f64 = 38.0;

/// Multiplier applied to touchpad pixel-delta scroll events.
///
/// libinput reports raw physical displacement in screen pixels with no
/// acceleration; applying this factor brings the effective scroll speed in
/// line with typical browser behaviour (Firefox/Chrome use a similar factor).
pub(crate) const TOUCHPAD_SCROLL_SCALE: f64 = 3.0;

fn scroll_to_wheel(delta: &mouse::ScrollDelta, point: WebViewPoint) -> InputEvent {
    let wheel_delta = match delta {
        mouse::ScrollDelta::Lines { x, y } => WheelDelta {
            x: *x as f64 * LINE_PX,
            y: *y as f64 * LINE_PX,
            z: 0.0,
            mode: WheelMode::DeltaPixel,
        },
        mouse::ScrollDelta::Pixels { x, y } => WheelDelta {
            x: *x as f64 * TOUCHPAD_SCROLL_SCALE,
            y: *y as f64 * TOUCHPAD_SCROLL_SCALE,
            z: 0.0,
            mode: WheelMode::DeltaPixel,
        },
    };
    InputEvent::Wheel(WheelEvent::new(wheel_delta, point))
}

/// Creates a [`InputEvent::Wheel`] for kinetic (momentum) scroll continuation.
///
/// `dx`/`dy` are logical pixels (same sign convention as Servo's WheelEvent:
/// positive y = scroll down). `pos` is the cursor position in Servo device pixels.
pub(crate) fn kinetic_scroll_event(dx: f64, dy: f64, pos: WebViewPoint) -> InputEvent {
    let delta = WheelDelta {
        x: dx,
        y: dy,
        z: 0.0,
        mode: WheelMode::DeltaPixel,
    };
    InputEvent::Wheel(WheelEvent::new(delta, pos))
}

// ── Keyboard ──────────────────────────────────────────────────────────────────

/// Converts an iced keyboard event to a Servo [`InputEvent`].
///
/// Returns `None` for `ModifiersChanged`, which Servo does not handle directly.
pub(crate) fn keyboard_event_to_servo(event: &keyboard::Event) -> Option<InputEvent> {
    match event {
        keyboard::Event::KeyPressed {
            key,
            physical_key,
            location,
            modifiers,
            repeat,
            ..
        } => Some(InputEvent::Keyboard(ServoKeyboardEvent::new_without_event(
            KeyState::Down,
            iced_key_to_kt(key),
            iced_physical_to_code(physical_key),
            iced_location_to_kt(*location),
            iced_modifiers_to_kt(*modifiers),
            *repeat,
            false,
        ))),
        keyboard::Event::KeyReleased {
            key,
            physical_key,
            location,
            modifiers,
            ..
        } => Some(InputEvent::Keyboard(ServoKeyboardEvent::new_without_event(
            KeyState::Up,
            iced_key_to_kt(key),
            iced_physical_to_code(physical_key),
            iced_location_to_kt(*location),
            iced_modifiers_to_kt(*modifiers),
            false,
            false,
        ))),
        keyboard::Event::ModifiersChanged(_) => None,
    }
}

/// Whether `event` is a press or release of a bare modifier key
/// (Control/Shift/Alt/Super), as opposed to a character or other key.
///
/// Used to forward *only* modifier state — never typed content — when keeping
/// the engine's global keyboard-modifier state in sync from a window-level
/// listener, so that a modifier release is not missed when a non-webview page is
/// shown.
pub(crate) fn is_bare_modifier_key(event: &keyboard::Event) -> bool {
    let key = match event {
        keyboard::Event::KeyPressed { key, .. } | keyboard::Event::KeyReleased { key, .. } => key,
        keyboard::Event::ModifiersChanged(_) => return false,
    };
    matches!(
        key,
        keyboard::Key::Named(
            keyboard::key::Named::Control
                | keyboard::key::Named::Shift
                | keyboard::key::Named::Alt
                | keyboard::key::Named::Super
        )
    )
}

/// Converts an iced logical [`Key`] to the `keyboard_types` equivalent.
///
/// Both iced and `keyboard_types` follow the W3C UI Events spec, so named key
/// variants share the same PascalCase names (e.g. `Named::ArrowLeft` →
/// `NamedKey::ArrowLeft`). The string round-trip covers all current and future
/// spec keys without an exhaustive match.
fn iced_key_to_kt(key: &keyboard::Key) -> KtKey {
    match key {
        keyboard::Key::Character(s) => KtKey::Character(s.to_string()),
        keyboard::Key::Named(named) => {
            // Derive Debug prints the variant name, which is the W3C key name.
            let s = format!("{named:?}");
            NamedKey::from_str(&s)
                .map(KtKey::Named)
                .unwrap_or(KtKey::Named(NamedKey::Unidentified))
        }
        keyboard::Key::Unidentified => KtKey::Named(NamedKey::Unidentified),
    }
}

/// Converts an iced physical key to a `keyboard_types` [`Code`].
fn iced_physical_to_code(physical: &keyboard::key::Physical) -> Code {
    match physical {
        keyboard::key::Physical::Code(code) => {
            let s = format!("{code:?}");
            Code::from_str(&s).unwrap_or(Code::Unidentified)
        }
        keyboard::key::Physical::Unidentified(_) => Code::Unidentified,
    }
}

/// Converts an iced [`Location`] to the `keyboard_types` equivalent.
fn iced_location_to_kt(loc: keyboard::Location) -> KtLocation {
    match loc {
        keyboard::Location::Standard => KtLocation::Standard,
        keyboard::Location::Left => KtLocation::Left,
        keyboard::Location::Right => KtLocation::Right,
        keyboard::Location::Numpad => KtLocation::Numpad,
    }
}

/// Converts iced modifier flags to `keyboard_types` modifier flags.
fn iced_modifiers_to_kt(mods: keyboard::Modifiers) -> KtModifiers {
    let mut result = KtModifiers::empty();
    if mods.alt() {
        result |= KtModifiers::ALT;
    }
    if mods.control() {
        result |= KtModifiers::CONTROL;
    }
    if mods.shift() {
        result |= KtModifiers::SHIFT;
    }
    if mods.logo() {
        result |= KtModifiers::META;
    }
    result
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use libcosmic::iced::{Point, keyboard, mouse};
    use servo::{
        InputEvent, KeyState, MouseButton as ServoMouseButton, MouseButtonAction, WheelMode,
    };

    use super::*;

    fn pos(x: f32, y: f32) -> Option<Point> {
        Some(Point::new(x, y))
    }

    // ── mouse_event_to_servo ───────────────────────────────────────────────

    #[test]
    fn left_button_down_maps_coords_with_scale() {
        let event = mouse::Event::ButtonPressed(mouse::Button::Left);
        let servo = mouse_event_to_servo(&event, pos(100.0, 50.0), 2.0).unwrap();
        let InputEvent::MouseButton(e) = servo else {
            panic!("wrong variant")
        };
        assert_eq!(e.action, MouseButtonAction::Down);
        assert_eq!(e.button, ServoMouseButton::Left);
        // servo_point multiplies by scale, so (100*2, 50*2) = (200, 100)
        if let servo::WebViewPoint::Device(pt) = e.point {
            assert!((pt.x - 200.0).abs() < 0.01);
            assert!((pt.y - 100.0).abs() < 0.01);
        } else {
            panic!("expected Device point");
        }
    }

    #[test]
    fn right_button_up_outside_bounds_returns_none() {
        let event = mouse::Event::ButtonReleased(mouse::Button::Right);
        assert!(mouse_event_to_servo(&event, None, 1.0).is_none());
    }

    #[test]
    fn cursor_moved_scales_coords() {
        let event = mouse::Event::CursorMoved {
            position: Point::ORIGIN,
        };
        let servo = mouse_event_to_servo(&event, pos(10.0, 20.0), 1.5).unwrap();
        let InputEvent::MouseMove(e) = servo else {
            panic!("wrong variant")
        };
        if let servo::WebViewPoint::Device(pt) = e.point {
            assert!((pt.x - 15.0).abs() < 0.01);
            assert!((pt.y - 30.0).abs() < 0.01);
        } else {
            panic!("expected Device point");
        }
    }

    #[test]
    fn wheel_lines_uses_line_px_multiplier() {
        let event = mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
        };
        let servo = mouse_event_to_servo(&event, pos(0.0, 0.0), 1.0).unwrap();
        let InputEvent::Wheel(e) = servo else {
            panic!("wrong variant")
        };
        assert_eq!(e.delta.mode, WheelMode::DeltaPixel);
        assert!((e.delta.y - 38.0).abs() < 0.01);
    }

    #[test]
    fn wheel_pixels_passes_through() {
        let event = mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Pixels { x: 5.0, y: -10.0 },
        };
        let servo = mouse_event_to_servo(&event, pos(0.0, 0.0), 1.0).unwrap();
        let InputEvent::Wheel(e) = servo else {
            panic!("wrong variant")
        };
        assert_eq!(e.delta.mode, WheelMode::DeltaPixel);
        assert!((e.delta.x - 5.0 * TOUCHPAD_SCROLL_SCALE).abs() < 0.01);
        assert!((e.delta.y - -10.0 * TOUCHPAD_SCROLL_SCALE).abs() < 0.01);
    }

    #[test]
    fn kinetic_scroll_event_produces_wheel_with_pixel_delta() {
        let point = servo_point(libcosmic::iced::Point { x: 100.0, y: 200.0 }, 1.0);
        let event = kinetic_scroll_event(3.0, -7.5, point);
        let InputEvent::Wheel(e) = event else {
            panic!("wrong variant")
        };
        assert_eq!(e.delta.mode, WheelMode::DeltaPixel);
        assert!((e.delta.x - 3.0).abs() < 0.01);
        assert!((e.delta.y - -7.5).abs() < 0.01);
    }

    #[test]
    fn cursor_left_produces_mouse_left_viewport() {
        let event = mouse::Event::CursorLeft;
        let servo = mouse_event_to_servo(&event, None, 1.0).unwrap();
        assert!(matches!(servo, InputEvent::MouseLeftViewport(_)));
    }

    // ── keyboard_event_to_servo ────────────────────────────────────────────

    #[test]
    fn character_key_down_maps_correctly() {
        let event = keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("a".into()),
            modified_key: keyboard::Key::Character("a".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyA),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::default(),
            text: Some("a".into()),
            repeat: false,
        };
        let servo = keyboard_event_to_servo(&event).unwrap();
        let InputEvent::Keyboard(ke) = servo else {
            panic!("wrong variant")
        };
        assert_eq!(ke.event.state, KeyState::Down);
        assert_eq!(ke.event.key, servo::Key::Character("a".to_string()));
    }

    #[test]
    fn named_enter_key_maps_correctly() {
        let event = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::default(),
            text: None,
            repeat: false,
        };
        let servo = keyboard_event_to_servo(&event).unwrap();
        let InputEvent::Keyboard(ke) = servo else {
            panic!("wrong variant")
        };
        assert_eq!(ke.event.key, servo::Key::Named(servo::NamedKey::Enter));
    }

    #[test]
    fn key_released_has_up_state() {
        let event = keyboard::Event::KeyReleased {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::default(),
        };
        let servo = keyboard_event_to_servo(&event).unwrap();
        let InputEvent::Keyboard(ke) = servo else {
            panic!("wrong variant")
        };
        assert_eq!(ke.event.state, KeyState::Up);
    }

    #[test]
    fn modifiers_changed_returns_none() {
        let event = keyboard::Event::ModifiersChanged(keyboard::Modifiers::default());
        assert!(keyboard_event_to_servo(&event).is_none());
    }

    #[test]
    fn shift_modifier_maps_correctly() {
        let event = keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("A".into()),
            modified_key: keyboard::Key::Character("A".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyA),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::SHIFT,
            text: Some("A".into()),
            repeat: false,
        };
        let servo = keyboard_event_to_servo(&event).unwrap();
        let InputEvent::Keyboard(ke) = servo else {
            panic!("wrong variant")
        };
        assert!(ke.event.modifiers.contains(servo::Modifiers::SHIFT));
    }
}
