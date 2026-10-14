//! Keyboard and mouse forwarding from GTK to the session.

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use rdesk_core::input::{MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use rdesk_input::{Key, ScrollUnit};
use tracing::debug;

use super::State;

pub(super) fn attach(state: &State) {
    // Capture phase on the window, so window shortcuts (F10, Tab focus moves…) go to the remote PC.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = std::rc::Rc::downgrade(state);
    keys.connect_key_pressed(move |_, keyval, keycode, modifiers| {
        let Some(state) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if !state.connected.get() {
            return glib::Propagation::Proceed;
        }
        let ctrl_alt = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK;
        if modifiers.contains(ctrl_alt) && matches!(keyval, gdk::Key::Return | gdk::Key::KP_Enter) {
            if state.window.is_fullscreen() {
                state.window.unfullscreen();
            } else {
                state.window.fullscreen();
            }
            return glib::Propagation::Stop;
        }
        send_key(&state, keycode, true);
        glib::Propagation::Stop
    });
    let weak = std::rc::Rc::downgrade(state);
    keys.connect_key_released(move |_, _, keycode, _| {
        if let Some(state) = weak.upgrade()
            && state.connected.get()
        {
            send_key(&state, keycode, false);
        }
    });
    state.window.add_controller(keys);

    // Keys held while focus leaves would otherwise stay pressed on the remote side.
    let weak = std::rc::Rc::downgrade(state);
    state.window.connect_is_active_notify(move |window| {
        let Some(state) = weak.upgrade() else { return };
        if !state.connected.get() {
            return;
        }
        if window.is_active() {
            synchronize_locks(&state);
        } else if let Some(session) = state.session.borrow().as_ref() {
            session.release_all();
        }
    });

    let motion = gtk::EventControllerMotion::new();
    let weak = std::rc::Rc::downgrade(state);
    motion.connect_motion(move |_, x, y| {
        if let Some(state) = weak.upgrade()
            && let Some(position) = to_remote(&state, x, y)
        {
            send(&state, [Operation::MouseMove(position)]);
        }
    });
    state.picture.add_controller(motion);

    // Raw button events: GestureClick tracks one button at a time, but chords (e.g. left+right) must work.
    let buttons = gtk::EventControllerLegacy::new();
    let weak = std::rc::Rc::downgrade(state);
    buttons.connect_event(move |controller, event| {
        let Some(state) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let pressed = match event.event_type() {
            gdk::EventType::ButtonPress => true,
            gdk::EventType::ButtonRelease => false,
            _ => return glib::Propagation::Proceed,
        };
        let Some(button) = event.downcast_ref::<gdk::ButtonEvent>().map(|e| e.button()) else {
            return glib::Propagation::Proceed;
        };
        if pressed && let Some(widget) = controller.widget() {
            widget.grab_focus();
        }
        let Some(button) = u16::try_from(button).ok().and_then(MouseButton::from_native_button) else {
            debug!(button, "Unmapped mouse button");
            return glib::Propagation::Stop;
        };
        send(
            &state,
            [if pressed {
                Operation::MouseButtonPressed(button)
            } else {
                Operation::MouseButtonReleased(button)
            }],
        );
        glib::Propagation::Stop
    });
    state.picture.add_controller(buttons);

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let weak = std::rc::Rc::downgrade(state);
    scroll.connect_scroll(move |controller, dx, dy| {
        let Some(state) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let unit = match controller.unit() {
            gdk::ScrollUnit::Wheel => ScrollUnit::Notches,
            _ => ScrollUnit::Pixels,
        };
        let vertical = state.scroll_vertical.borrow_mut().add(dy, unit);
        // RDP's horizontal wheel is positive to the right, like GTK; the accumulator flips the sign
        // for the vertical axis, so flip it back here.
        let horizontal = state.scroll_horizontal.borrow_mut().add(-dx, unit);
        let operations = vertical
            .into_iter()
            .map(|units| (true, units))
            .chain(horizontal.into_iter().map(|units| (false, units)))
            .map(|(is_vertical, rotation_units)| {
                Operation::WheelRotations(WheelRotations {
                    is_vertical,
                    rotation_units,
                })
            });
        send(&state, operations);
        glib::Propagation::Stop
    });
    let weak = std::rc::Rc::downgrade(state);
    scroll.connect_scroll_end(move |_| {
        if let Some(state) = weak.upgrade() {
            state.scroll_vertical.borrow_mut().reset();
            state.scroll_horizontal.borrow_mut().reset();
        }
    });
    state.picture.add_controller(scroll);
}

pub(super) fn synchronize_locks(state: &State) {
    let Some(keyboard) = gdk::Display::default()
        .and_then(|d| d.default_seat())
        .and_then(|seat| seat.keyboard())
    else {
        return;
    };
    if let Some(session) = state.session.borrow().as_ref() {
        session.synchronize_locks(
            keyboard.is_caps_locked(),
            keyboard.is_num_locked(),
            keyboard.is_scroll_locked(),
        );
    }
}

fn send(state: &State, operations: impl IntoIterator<Item = Operation>) {
    if let Some(session) = state.session.borrow().as_ref() {
        session.send_input(operations);
    }
}

fn send_key(state: &State, keycode: u32, pressed: bool) {
    match rdesk_input::from_xkb_keycode(keycode) {
        Some(Key::Scancode { code, extended }) => {
            let scancode = Scancode::from_u8(extended, code);
            send(
                state,
                [if pressed {
                    Operation::KeyPressed(scancode)
                } else {
                    Operation::KeyReleased(scancode)
                }],
            );
        }
        Some(Key::Pause) => {
            if pressed && let Some(session) = state.session.borrow().as_ref() {
                session.send_pause();
            }
        }
        None => debug!(keycode, "Unmapped key"),
    }
}

/// Maps a point in the picture to remote desktop pixels, accounting for the letterboxed scaling.
fn to_remote(state: &State, x: f64, y: f64) -> Option<MousePosition> {
    let texture = state.texture.borrow();
    let texture = texture.as_ref()?;
    let (tw, th) = (f64::from(texture.width()), f64::from(texture.height()));
    let (ww, wh) = (f64::from(state.picture.width()), f64::from(state.picture.height()));
    map_point(x, y, (ww, wh), (tw, th))
}

fn map_point(x: f64, y: f64, widget: (f64, f64), remote: (f64, f64)) -> Option<MousePosition> {
    let scale = (widget.0 / remote.0).min(widget.1 / remote.1);
    if !(scale.is_finite() && scale > 0.0) {
        return None;
    }
    let offset_x = (widget.0 - remote.0 * scale) / 2.0;
    let offset_y = (widget.1 - remote.1 * scale) / 2.0;
    let rx = ((x - offset_x) / scale).floor().clamp(0.0, remote.0 - 1.0);
    let ry = ((y - offset_y) / scale).floor().clamp(0.0, remote.1 - 1.0);
    Some(MousePosition {
        x: rx as u16,
        y: ry as u16,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(x: u16, y: u16) -> Option<MousePosition> {
        Some(MousePosition { x, y })
    }

    #[test]
    fn maps_scaled_and_letterboxed_points() {
        // 1920x1080 shown in a 960x640 widget: scale 0.5, 50 px bars at top and bottom.
        let (w, r) = ((960.0, 640.0), (1920.0, 1080.0));
        assert_eq!(map_point(0.0, 50.0, w, r), pos(0, 0));
        assert_eq!(map_point(480.0, 320.0, w, r), pos(960, 540));
        assert_eq!(map_point(959.9, 589.9, w, r), pos(1919, 1079));
        // Points on the bars clamp to the edge.
        assert_eq!(map_point(10.0, 5.0, w, r), pos(20, 0));
        assert_eq!(map_point(10.0, 639.0, w, r), pos(20, 1079));
        assert_eq!(map_point(1.0, 1.0, (0.0, 0.0), r), None);
    }
}
