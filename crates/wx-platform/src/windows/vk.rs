//! Virtual-key mapping between Windows and [`SpecialKey`].
//!
//! One table used in both directions, so capture and injection cannot drift apart
//! — a mapping that is asymmetric would make a key work when sent from Windows
//! but not when sent to it, and that asymmetry is invisible until someone tests
//! both directions of the same key.

use windows::Win32::UI::Input::KeyboardAndMouse::*;
use wx_proto::SpecialKey;

/// Windows virtual keys paired with their semantic equivalents.
///
/// Left-hand side is the *sided* virtual key where Windows has one, because the
/// low-level keyboard hook always reports the sided form and the protocol keeps
/// left and right distinct.
const TABLE: &[(VIRTUAL_KEY, SpecialKey)] = &[
    (VK_ESCAPE, SpecialKey::Escape),
    (VK_BACK, SpecialKey::Backspace),
    (VK_TAB, SpecialKey::Tab),
    (VK_RETURN, SpecialKey::Enter),
    (VK_DELETE, SpecialKey::Delete),
    (VK_INSERT, SpecialKey::Insert),
    (VK_HOME, SpecialKey::Home),
    (VK_END, SpecialKey::End),
    (VK_PRIOR, SpecialKey::PageUp),
    (VK_NEXT, SpecialKey::PageDown),
    (VK_UP, SpecialKey::Up),
    (VK_DOWN, SpecialKey::Down),
    (VK_LEFT, SpecialKey::Left),
    (VK_RIGHT, SpecialKey::Right),
    (VK_F1, SpecialKey::F1),
    (VK_F2, SpecialKey::F2),
    (VK_F3, SpecialKey::F3),
    (VK_F4, SpecialKey::F4),
    (VK_F5, SpecialKey::F5),
    (VK_F6, SpecialKey::F6),
    (VK_F7, SpecialKey::F7),
    (VK_F8, SpecialKey::F8),
    (VK_F9, SpecialKey::F9),
    (VK_F10, SpecialKey::F10),
    (VK_F11, SpecialKey::F11),
    (VK_F12, SpecialKey::F12),
    (VK_LSHIFT, SpecialKey::ShiftLeft),
    (VK_RSHIFT, SpecialKey::ShiftRight),
    (VK_LCONTROL, SpecialKey::CtrlLeft),
    (VK_RCONTROL, SpecialKey::CtrlRight),
    (VK_LMENU, SpecialKey::AltLeft),
    (VK_RMENU, SpecialKey::AltRight),
    (VK_LWIN, SpecialKey::SuperLeft),
    (VK_RWIN, SpecialKey::SuperRight),
    (VK_CAPITAL, SpecialKey::CapsLock),
    (VK_NUMLOCK, SpecialKey::NumLock),
    (VK_SCROLL, SpecialKey::ScrollLock),
    (VK_SNAPSHOT, SpecialKey::PrintScreen),
    (VK_PAUSE, SpecialKey::Pause),
    (VK_APPS, SpecialKey::Menu),
    (VK_VOLUME_UP, SpecialKey::VolumeUp),
    (VK_VOLUME_DOWN, SpecialKey::VolumeDown),
    (VK_VOLUME_MUTE, SpecialKey::VolumeMute),
    (VK_MEDIA_PLAY_PAUSE, SpecialKey::MediaPlayPause),
    (VK_MEDIA_NEXT_TRACK, SpecialKey::MediaNext),
    (VK_MEDIA_PREV_TRACK, SpecialKey::MediaPrev),
    (VK_MEDIA_STOP, SpecialKey::MediaStop),
];

/// Semantic key for a virtual key reported by the keyboard hook.
///
/// Unsided virtual keys are folded onto the left-hand key. They should not appear
/// from `WH_KEYBOARD_LL`, but `SendInput`-injected events and some vendor drivers
/// do produce them, and treating them as unknown would send the modifier as a
/// meaningless raw keycode.
pub fn special_from_vk(vk: u16) -> Option<SpecialKey> {
    let vk = VIRTUAL_KEY(vk);
    if vk == VK_SHIFT {
        return Some(SpecialKey::ShiftLeft);
    }
    if vk == VK_CONTROL {
        return Some(SpecialKey::CtrlLeft);
    }
    if vk == VK_MENU {
        return Some(SpecialKey::AltLeft);
    }
    TABLE.iter().find(|(v, _)| *v == vk).map(|(_, s)| *s)
}

/// Virtual key to inject for a semantic key.
///
/// `None` for keys Windows has no virtual key for. Brightness is the notable
/// case: it is handled by firmware and the WM_APPCOMMAND path, not by a VK, so
/// those events cannot be injected and the caller reports them unsupported rather
/// than pressing something arbitrary.
pub fn vk_from_special(key: SpecialKey) -> Option<VIRTUAL_KEY> {
    TABLE.iter().find(|(_, s)| *s == key).map(|(v, _)| *v)
}

/// Whether a virtual key needs `KEYEVENTF_EXTENDEDKEY`.
///
/// The extended flag is what distinguishes the navigation cluster from the numpad
/// keys that share its scancodes. Omit it and arrow keys move the caret only when
/// Num Lock happens to be off, which presents as "the arrow keys work on my
/// machine but not on my colleague's".
pub fn is_extended(vk: VIRTUAL_KEY) -> bool {
    matches!(
        vk,
        VK_RCONTROL
            | VK_RMENU
            | VK_INSERT
            | VK_DELETE
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_UP
            | VK_DOWN
            | VK_LEFT
            | VK_RIGHT
            | VK_NUMLOCK
            | VK_SNAPSHOT
            | VK_DIVIDE
            | VK_LWIN
            | VK_RWIN
            | VK_APPS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_is_symmetric_in_both_directions() {
        // Asymmetry here means a key that can be sent from Windows but not to it.
        for (vk, special) in TABLE {
            assert_eq!(
                special_from_vk(vk.0),
                Some(*special),
                "{special:?} does not map back from {vk:?}"
            );
            assert_eq!(
                vk_from_special(*special),
                Some(*vk),
                "{special:?} does not map to a virtual key"
            );
        }
    }

    #[test]
    fn no_virtual_key_is_listed_twice() {
        // A duplicate would make the reverse lookup silently pick the first entry.
        for (i, (vk, _)) in TABLE.iter().enumerate() {
            for (vk_other, _) in &TABLE[i + 1..] {
                assert_ne!(vk.0, vk_other.0, "virtual key {vk:?} appears twice");
            }
        }
    }

    #[test]
    fn no_semantic_key_is_listed_twice() {
        for (i, (_, s)) in TABLE.iter().enumerate() {
            for (_, s_other) in &TABLE[i + 1..] {
                assert_ne!(s, s_other, "special key {s:?} appears twice");
            }
        }
    }

    #[test]
    fn left_and_right_modifiers_stay_distinct() {
        assert_ne!(
            vk_from_special(SpecialKey::CtrlLeft),
            vk_from_special(SpecialKey::CtrlRight)
        );
        assert_ne!(
            vk_from_special(SpecialKey::ShiftLeft),
            vk_from_special(SpecialKey::ShiftRight)
        );
    }

    #[test]
    fn unsided_modifiers_fold_onto_the_left_key() {
        assert_eq!(special_from_vk(VK_SHIFT.0), Some(SpecialKey::ShiftLeft));
        assert_eq!(special_from_vk(VK_CONTROL.0), Some(SpecialKey::CtrlLeft));
        assert_eq!(special_from_vk(VK_MENU.0), Some(SpecialKey::AltLeft));
    }

    #[test]
    fn printable_keys_are_not_in_the_special_table() {
        // Letters and digits must go down the text path, or the cross-layout
        // guarantee is lost for exactly the keys that need it.
        for vk in [VK_A.0, VK_Z.0, VK_0.0, VK_9.0, VK_SPACE.0, VK_OEM_1.0] {
            assert_eq!(
                special_from_vk(vk),
                None,
                "vk {vk} leaked into the special table"
            );
        }
    }

    #[test]
    fn navigation_keys_are_flagged_extended() {
        for key in [
            SpecialKey::Up,
            SpecialKey::Down,
            SpecialKey::Left,
            SpecialKey::Right,
            SpecialKey::Home,
            SpecialKey::End,
            SpecialKey::PageUp,
            SpecialKey::PageDown,
            SpecialKey::Insert,
            SpecialKey::Delete,
        ] {
            let vk = vk_from_special(key).unwrap();
            assert!(
                is_extended(vk),
                "{key:?} must be injected as an extended key"
            );
        }
    }

    #[test]
    fn ordinary_keys_are_not_flagged_extended() {
        for key in [
            SpecialKey::Enter,
            SpecialKey::Tab,
            SpecialKey::F1,
            SpecialKey::Escape,
        ] {
            assert!(!is_extended(vk_from_special(key).unwrap()), "{key:?}");
        }
    }

    #[test]
    fn keys_windows_cannot_inject_report_no_virtual_key() {
        // Honest failure beats pressing an arbitrary key: brightness is a firmware
        // path with no virtual key.
        assert_eq!(vk_from_special(SpecialKey::BrightnessUp), None);
        assert_eq!(vk_from_special(SpecialKey::BrightnessDown), None);
    }

    #[test]
    fn media_keys_map_so_they_reach_whichever_machine_has_the_cursor() {
        for key in [
            SpecialKey::VolumeUp,
            SpecialKey::VolumeMute,
            SpecialKey::MediaPlayPause,
            SpecialKey::MediaNext,
        ] {
            assert!(vk_from_special(key).is_some(), "{key:?}");
        }
    }
}
