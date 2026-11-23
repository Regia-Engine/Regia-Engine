// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/window_backend.rs,
// commit d6ed37677, regia-absorb-1), ADR-8012; adapted under ADR-8156.
//! Bare-Alt menu activation suppression for menu-free Windows hosts (ADR-8156).
//! Adapted from DO27 winplat.rs:340-367 and window_backend.rs:22-32,188-203,
//! d6ed37677 (regia-absorb-1), owner-authorized P-08 copy-and-adapt, class P3.

/// Whether a native Windows message would activate the menu on a bare Alt tap.
/// Keyboard events, Alt+Space, Alt+F4, minimize/maximize and all other messages pass.
/// Available on every target for host policy tests; it makes no OS calls.
pub fn swallows_message(message: u32, wparam: usize, lparam: isize) -> bool {
    message == 0x0112 && (wparam & 0xfff0) == 0xf100 && lparam == 0
}

/// Installs bare-Alt suppression on a Windows window's owning thread.
/// Idempotent; the subclass removes itself on window destruction. On other OSes
/// this is a successful no-op. The platform runner calls it on its primary window;
/// custom menu-free hosts call it after creating their own window.
///
/// # Errors
/// Returns an error for an invalid handle, the wrong thread or refused subclass.
#[cfg(has_window)]
pub fn install(window: &winit::window::Window) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        crate::platform::install_alt_menu(window)
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_a_bare_alt_tap_is_swallowed() {
        assert!(swallows_message(0x0112, 0xf100, 0));
        assert!(swallows_message(0x0112, 0xf10f, 0));
        assert!(!swallows_message(0x0112, 0xf100, 0x20));
        for command in [0xf060, 0xf020, 0xf030, 0xf040, 0xf090] {
            assert!(!swallows_message(0x0112, command, 0));
        }
        // Actual Alt key press/release must still reach winit/input.
        assert!(!swallows_message(0x0104, 0xf100, 0));
        assert!(!swallows_message(0x0105, 0xf100, 0));
    }
}
