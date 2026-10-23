//! The tap binding: one side-specific modifier standing alone, judged on release.
//!
//! `RegisterHotKey` accepts a bare modifier and then never delivers it (measured on Win11 24H2), so the
//! tap has to be watched by the program itself. Raw input was measured out first: with `INPUTSINK`,
//! `WM_INPUT` reaches only windows the compositor is actually presenting — a hidden tray window, a
//! screen-off-screen window and an alpha-zero window all got nothing — and a hook is a resident
//! callback the system may silently drop. What is left is polling the asynchronous key state on a
//! fine timer while a tap binding exists: no hook, no extra window, no key is ever swallowed, and the
//! left/right sides come apart in the virtual keys themselves. A press only fires if no other key or
//! mouse button was down at any poll in between, so Right Ctrl keeps working as a modifier.

use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

use crate::hotkey::TapKey;

/// The pure part: what a sequence of polls means. Everything here is unit-testable without a device.
#[derive(Default)]
pub struct Machine {
    key: Option<TapKey>,
    /// A fresh binding first waits to see the key up, so a hold that predates the binding is not a tap.
    primed: bool,
    /// The bound key is currently held.
    down: bool,
    /// The held press has seen no other input, so releasing it fires.
    armed: bool,
}

impl Machine {
    pub fn new(key: Option<TapKey>) -> Machine {
        Machine {
            key,
            primed: key.is_none(),
            down: false,
            armed: false,
        }
    }

    /// Swapping bindings resets any hold in progress; a fresh press starts a fresh judgement.
    pub fn set_key(&mut self, key: Option<TapKey>) {
        *self = Machine::new(key);
    }

    pub fn key(&self) -> Option<TapKey> {
        self.key
    }

    /// The bound key is currently held, so the caller knows when other input has to be watched.
    pub fn holding(&self) -> bool {
        self.down
    }

    /// One poll of the bound key's state. Returns true exactly once, when a clean tap completes.
    pub fn poll(&mut self, down: bool, other: bool) -> bool {
        if self.key.is_none() {
            return false;
        }
        if !self.primed {
            if !down {
                self.primed = true;
            }
            return false;
        }
        match (self.down, down) {
            (false, true) => {
                self.down = true;
                self.armed = !other;
            }
            (true, false) => {
                self.down = false;
                if self.armed {
                    self.armed = false;
                    return true;
                }
            }
            // A chord partner seen while the tap is held ends the tap, even if it goes back up.
            (true, true) => {
                if other {
                    self.armed = false;
                }
            }
            (false, false) => {}
        }
        false
    }
}

/// True while the tap key itself is down, by the virtual key that names its side only.
pub fn key_down(key: TapKey) -> bool {
    (unsafe { GetAsyncKeyState(key.held_by()[0]) }) as u16 & 0x8000 != 0
}

/// True when any other key, mouse button or already-held modifier is down. The tap key's own virtual
/// keys (its side key and the generic one it feeds) are excluded, so holding it is not its own chord.
pub fn other_down(key: TapKey) -> bool {
    let own = key.held_by();
    for vk in 1..=254i32 {
        if vk == own[0] || (own[1] != 0 && vk == own[1]) {
            continue;
        }
        if unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000 != 0 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_tap_fires_once_on_release() {
        let mut machine = Machine::new(Some(TapKey::RCtrl));
        assert!(!machine.poll(false, false));
        assert!(!machine.poll(true, false));
        assert!(!machine.poll(true, false));
        assert!(machine.poll(false, false));
        // The machine keeps working for the next tap.
        assert!(!machine.poll(true, false));
        assert!(machine.poll(false, false));
    }

    #[test]
    fn a_chord_press_never_fires_the_tap() {
        let mut machine = Machine::new(Some(TapKey::RCtrl));
        assert!(!machine.poll(false, false));
        assert!(!machine.poll(true, false));
        // C goes down and comes back up while Right Ctrl is still held.
        assert!(!machine.poll(true, true));
        assert!(!machine.poll(true, false));
        assert!(!machine.poll(false, false));
    }

    #[test]
    fn a_chord_already_in_progress_is_not_a_tap() {
        let mut machine = Machine::new(Some(TapKey::RCtrl));
        assert!(!machine.poll(false, false));
        // C was already down when Right Ctrl arrived.
        assert!(!machine.poll(true, true));
        assert!(!machine.poll(true, false));
        assert!(!machine.poll(false, false));
    }

    #[test]
    fn a_mouse_click_while_held_is_a_chord_too() {
        let mut machine = Machine::new(Some(TapKey::RCtrl));
        assert!(!machine.poll(false, false));
        assert!(!machine.poll(true, false));
        assert!(!machine.poll(true, true));
        assert!(!machine.poll(false, false));
    }

    #[test]
    fn a_key_down_before_a_poll_gap_is_caught_while_holding() {
        let mut machine = Machine::new(Some(TapKey::RCtrl));
        assert!(!machine.poll(false, false));
        assert!(!machine.poll(true, false));
        // The chord partner went down and up entirely between two polls: the state poll still sees it.
        assert!(!machine.poll(true, true));
        assert!(!machine.poll(false, false));
    }

    #[test]
    fn a_hold_that_predates_the_binding_is_not_a_tap() {
        let mut machine = Machine::new(Some(TapKey::RCtrl));
        // The binding was set while the key was already down.
        assert!(!machine.poll(true, false));
        assert!(!machine.poll(false, false));
        // From the first clean press on, taps work.
        assert!(!machine.poll(true, false));
        assert!(machine.poll(false, false));
    }

    #[test]
    fn holding_through_many_polls_is_one_tap() {
        let mut machine = Machine::new(Some(TapKey::RCtrl));
        assert!(!machine.poll(false, false));
        assert!(!machine.poll(true, false));
        for _ in 0..50 {
            assert!(!machine.poll(true, false));
        }
        assert!(machine.poll(false, false));
    }

    #[test]
    fn swapping_the_binding_resets_the_hold() {
        let mut machine = Machine::new(Some(TapKey::RCtrl));
        assert!(!machine.poll(false, false));
        assert!(!machine.poll(true, false));
        machine.set_key(Some(TapKey::LCtrl));
        assert!(!machine.poll(false, false));
        assert!(!machine.poll(true, false));
        assert!(machine.poll(false, false));
    }

    #[test]
    fn without_a_binding_the_machine_is_inert() {
        let mut machine = Machine::new(None);
        assert!(!machine.poll(true, false));
        assert!(!machine.poll(false, false));
    }
}
