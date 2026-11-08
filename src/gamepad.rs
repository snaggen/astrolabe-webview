//! Gamepad support, behind the `gamepad` feature.
//!
//! Servo does not read input devices itself: the embedder polls them and feeds
//! `InputEvent::Gamepad` into a WebView, and answers rumble requests through
//! [`GamepadDelegate`]. This module does both with gilrs, mirroring servoshell's
//! `desktop/gamepad.rs` — the button and axis numbering below is what the
//! Gamepad specification calls the *standard mapping*, and pages rely on it.
//!
//! The gate is Servo's own: our `gamepad` feature turns on `servo/gamepad`,
//! without which the delegate type does not exist.

use std::cell::RefCell;
use std::collections::HashMap;

use gilrs::ff::{BaseEffect, BaseEffectType, Effect, EffectBuilder, Repeat, Replay, Ticks};
use gilrs::{EventType, Gilrs};
use servo::{
    GamepadDelegate, GamepadEvent, GamepadHapticEffectRequest, GamepadHapticEffectRequestType,
    GamepadHapticEffectType, GamepadIndex, GamepadInputBounds, GamepadSupportedHapticEffects,
    GamepadUpdateType, InputEvent, WebView,
};

/// A rumble effect playing on a pad, held until it finishes so the page's
/// promise can be resolved.
struct HapticEffect {
    effect: Effect,
    request: GamepadHapticEffectRequest,
}

/// Reads gamepads with gilrs and plays rumble effects on them.
///
/// One instance serves the whole engine: gilrs owns the device connections, and
/// a second handle would compete for the same events.
pub(crate) struct GamepadDelegateImpl {
    handle: RefCell<Gilrs>,
    /// Rumble effects by gamepad index, awaiting completion.
    haptic_effects: RefCell<HashMap<usize, HapticEffect>>,
}

impl GamepadDelegateImpl {
    /// Opens the gamepad subsystem, or returns `None` when it is unavailable —
    /// a machine with no input device access is not an error worth failing over.
    pub(crate) fn new() -> Option<Self> {
        match Gilrs::new() {
            Ok(handle) => Some(Self {
                handle: RefCell::new(handle),
                haptic_effects: RefCell::new(HashMap::new()),
            }),
            Err(e) => {
                tracing::warn!("Gamepad support unavailable: {e}");
                None
            }
        }
    }

    /// Drains pending gamepad events into `webview`.
    ///
    /// Returns `true` while any gamepad is connected, which lets the caller poll
    /// often only when there is something to read.
    pub(crate) fn poll(&self, webview: &WebView) -> bool {
        let mut handle = self.handle.borrow_mut();
        while let Some(event) = handle.next_event() {
            let index = GamepadIndex(event.id.into());
            let servo_event = match event.event {
                // Digital buttons report 1.0 pressed, 0.0 released. The triggers
                // (6, 7) are analogue and arrive as ButtonChanged instead, and 17
                // is the catch-all for buttons outside the standard mapping.
                EventType::ButtonPressed(button, _) => {
                    let button = map_button(button);
                    (!matches!(button, 6 | 7 | 17)).then_some(GamepadEvent::Updated(
                        index,
                        GamepadUpdateType::Button(button, 1.0),
                    ))
                }
                EventType::ButtonReleased(button, _) => {
                    let button = map_button(button);
                    (!matches!(button, 6 | 7 | 17)).then_some(GamepadEvent::Updated(
                        index,
                        GamepadUpdateType::Button(button, 0.0),
                    ))
                }
                EventType::ButtonChanged(button, value, _) => {
                    let button = map_button(button);
                    matches!(button, 6 | 7).then(|| {
                        GamepadEvent::Updated(
                            index,
                            GamepadUpdateType::Button(button, f64::from(value)),
                        )
                    })
                }
                EventType::AxisChanged(axis, value, _) => {
                    map_axis(axis).map(|axis| {
                        // The Gamepad specification points the Y axes down;
                        // gilrs points them up.
                        let value = if matches!(axis, 1 | 3) { -value } else { value };
                        GamepadEvent::Updated(
                            index,
                            GamepadUpdateType::Axis(axis, f64::from(value)),
                        )
                    })
                }
                EventType::Connected => {
                    let name = handle.gamepad(event.id).name().to_owned();
                    Some(GamepadEvent::Connected(
                        index,
                        name,
                        GamepadInputBounds {
                            axis_bounds: (-1.0, 1.0),
                            button_bounds: (0.0, 1.0),
                        },
                        GamepadSupportedHapticEffects {
                            supports_dual_rumble: true,
                            // gilrs has no trigger rumble.
                            supports_trigger_rumble: false,
                        },
                    ))
                }
                EventType::Disconnected => Some(GamepadEvent::Disconnected(index)),
                EventType::ForceFeedbackEffectCompleted => {
                    match self.haptic_effects.borrow_mut().remove(&event.id.into()) {
                        Some(haptic) => haptic.request.succeeded(),
                        None => tracing::warn!("No haptic effect for gamepad {}", event.id),
                    }
                    None
                }
                _ => None,
            };

            if let Some(servo_event) = servo_event {
                webview.notify_input_event(InputEvent::Gamepad(servo_event));
            }
        }

        handle.gamepads().next().is_some()
    }

    /// Starts a dual-rumble effect, answering the page's promise when it ends.
    fn play_haptic_effect(
        &self,
        effect_type: &GamepadHapticEffectType,
        request: GamepadHapticEffectRequest,
    ) {
        let index = request.gamepad_index();
        let GamepadHapticEffectType::DualRumble(params) = effect_type;

        let mut handle = self.handle.borrow_mut();
        let Some((_, gamepad)) = handle.gamepads().find(|(id, _)| usize::from(*id) == index) else {
            tracing::debug!("No connected gamepad {index} to rumble");
            request.failed();
            return;
        };

        let scheduling = Replay {
            after: Ticks::from_ms(params.start_delay as u32),
            play_for: Ticks::from_ms(params.duration as u32),
            with_delay: Ticks::from_ms(0),
        };
        let magnitude = |value: f64| (value * f64::from(u16::MAX)).round() as u16;
        let effect = EffectBuilder::new()
            .add_effect(BaseEffect {
                kind: BaseEffectType::Strong {
                    magnitude: magnitude(params.strong_magnitude),
                },
                scheduling,
                envelope: Default::default(),
            })
            .add_effect(BaseEffect {
                kind: BaseEffectType::Weak {
                    magnitude: magnitude(params.weak_magnitude),
                },
                scheduling,
                envelope: Default::default(),
            })
            .repeat(Repeat::For(scheduling.after + scheduling.play_for))
            .add_gamepad(&gamepad)
            .finish(&mut handle);

        // servoshell panics here; a page asking a pad without force feedback to
        // rumble is not a reason to take the browser down.
        let effect = match effect {
            Ok(effect) => effect,
            Err(e) => {
                tracing::debug!("Cannot build haptic effect: {e:?}");
                request.failed();
                return;
            }
        };
        if let Err(e) = effect.play() {
            tracing::debug!("Cannot play haptic effect: {e:?}");
            request.failed();
            return;
        }

        self.haptic_effects
            .borrow_mut()
            .insert(index, HapticEffect { effect, request });
    }

    /// Stops a running effect, answering the page's promise either way.
    fn stop_haptic_effect(&self, request: GamepadHapticEffectRequest) {
        let index = request.gamepad_index();
        let Some(haptic) = self.haptic_effects.borrow_mut().remove(&index) else {
            request.failed();
            return;
        };
        match haptic.effect.stop() {
            Ok(()) => request.succeeded(),
            Err(e) => {
                tracing::debug!("Cannot stop haptic effect: {e:?}");
                request.failed();
            }
        }
    }
}

impl GamepadDelegate for GamepadDelegateImpl {
    fn handle_haptic_effect_request(&self, request: GamepadHapticEffectRequest) {
        match request.request_type() {
            GamepadHapticEffectRequestType::Play(effect_type) => {
                self.play_haptic_effect(&effect_type.clone(), request);
            }
            GamepadHapticEffectRequestType::Stop => self.stop_haptic_effect(request),
        }
    }
}

/// Maps a gilrs button to its index in the Gamepad specification's standard
/// mapping. 17 means "outside the standard mapping", and is dropped.
///
/// <https://www.w3.org/TR/gamepad/#dfn-represents-a-standard-gamepad-button>
fn map_button(button: gilrs::Button) -> usize {
    match button {
        gilrs::Button::South => 0,
        gilrs::Button::East => 1,
        gilrs::Button::West => 2,
        gilrs::Button::North => 3,
        gilrs::Button::LeftTrigger => 4,
        gilrs::Button::RightTrigger => 5,
        gilrs::Button::LeftTrigger2 => 6,
        gilrs::Button::RightTrigger2 => 7,
        gilrs::Button::Select => 8,
        gilrs::Button::Start => 9,
        gilrs::Button::LeftThumb => 10,
        gilrs::Button::RightThumb => 11,
        gilrs::Button::DPadUp => 12,
        gilrs::Button::DPadDown => 13,
        gilrs::Button::DPadLeft => 14,
        gilrs::Button::DPadRight => 15,
        gilrs::Button::Mode => 16,
        _ => 17,
    }
}

/// Maps a gilrs axis to its standard-mapping index, or `None` for axes the
/// specification does not describe.
///
/// <https://www.w3.org/TR/gamepad/#dfn-represents-a-standard-gamepad-axis>
fn map_axis(axis: gilrs::Axis) -> Option<usize> {
    match axis {
        gilrs::Axis::LeftStickX => Some(0),
        gilrs::Axis::LeftStickY => Some(1),
        gilrs::Axis::RightStickX => Some(2),
        gilrs::Axis::RightStickY => Some(3),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The standard mapping is what pages index into, so the well-known
    /// positions must not drift.
    #[test]
    fn buttons_follow_the_standard_mapping() {
        assert_eq!(map_button(gilrs::Button::South), 0);
        assert_eq!(map_button(gilrs::Button::North), 3);
        assert_eq!(map_button(gilrs::Button::LeftTrigger2), 6);
        assert_eq!(map_button(gilrs::Button::DPadRight), 15);
        assert_eq!(map_button(gilrs::Button::Mode), 16);
        // Anything outside the mapping collapses onto the ignored index.
        assert_eq!(map_button(gilrs::Button::Unknown), 17);
    }

    #[test]
    fn only_the_four_stick_axes_are_mapped() {
        assert_eq!(map_axis(gilrs::Axis::LeftStickX), Some(0));
        assert_eq!(map_axis(gilrs::Axis::RightStickY), Some(3));
        assert_eq!(map_axis(gilrs::Axis::DPadX), None);
    }
}
