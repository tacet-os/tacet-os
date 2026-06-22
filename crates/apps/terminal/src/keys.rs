//! winit key → PTY byte translation.
//!
//! Pure, no winit/PTY dependencies in the function body — the caller
//! pulls apart the [`KeyEvent`] and passes us a `KeyAction`. That
//! keeps this module unit-testable without spinning up a window.

use winit::event::{ElementState, KeyEvent, Modifiers};
use winit::keyboard::{Key, NamedKey};

/// Encode a winit key event into PTY bytes. Returns `None` for events
/// we don't translate (e.g. modifier-only presses, key releases). The
/// terminal MVP only sends bytes on press — repeat is handled by the
/// OS via repeated `Pressed` events.
pub fn encode(event: &KeyEvent, mods: &Modifiers) -> Option<Vec<u8>> {
    if event.state != ElementState::Pressed {
        return None;
    }

    let mods = mods.state();
    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    // Shift is implicit in winit's `logical_key` (the produced
    // character is already case-shifted by the keymap); we only
    // need to inspect it for keys that have shift-modified VT
    // sequences (none in MVP).

    let key = event.logical_key.as_ref();

    // 1. NamedKey — function keys, arrows, editing keys, etc.
    if let Key::Named(named) = key {
        return Some(encode_named(named)?);
    }

    // 2. Character keys.
    if let Key::Character(text) = key {
        return Some(encode_char(text, ctrl, alt));
    }

    None
}

fn encode_named(named: NamedKey) -> Option<Vec<u8>> {
    let bytes: &[u8] = match named {
        // Editing.
        NamedKey::Enter => b"\r",
        NamedKey::Backspace => b"\x7f",
        NamedKey::Tab => b"\t",
        NamedKey::Escape => b"\x1b",
        NamedKey::Space => b" ",
        // Arrows (cursor mode application-cursor disabled — emit CSI).
        // Most TUIs accept both \x1b[A and \x1bOA; CSI form is the
        // mode-independent default.
        NamedKey::ArrowUp => b"\x1b[A",
        NamedKey::ArrowDown => b"\x1b[B",
        NamedKey::ArrowRight => b"\x1b[C",
        NamedKey::ArrowLeft => b"\x1b[D",
        // Editing/nav.
        NamedKey::Home => b"\x1b[H",
        NamedKey::End => b"\x1b[F",
        NamedKey::PageUp => b"\x1b[5~",
        NamedKey::PageDown => b"\x1b[6~",
        NamedKey::Insert => b"\x1b[2~",
        NamedKey::Delete => b"\x1b[3~",
        // Function keys. F1-F4 are SS3-style (\x1bO[PQRS]) on xterm
        // by tradition; F5+ are CSI ~ form.
        NamedKey::F1 => b"\x1bOP",
        NamedKey::F2 => b"\x1bOQ",
        NamedKey::F3 => b"\x1bOR",
        NamedKey::F4 => b"\x1bOS",
        NamedKey::F5 => b"\x1b[15~",
        NamedKey::F6 => b"\x1b[17~",
        NamedKey::F7 => b"\x1b[18~",
        NamedKey::F8 => b"\x1b[19~",
        NamedKey::F9 => b"\x1b[20~",
        NamedKey::F10 => b"\x1b[21~",
        NamedKey::F11 => b"\x1b[23~",
        NamedKey::F12 => b"\x1b[24~",
        _ => return None,
    };
    Some(bytes.to_vec())
}

fn encode_char(text: &str, ctrl: bool, alt: bool) -> Vec<u8> {
    if ctrl {
        // Ctrl+letter → 0x01..=0x1A. We only consider the first ASCII
        // char of the keymap-translated text and lowercase it before
        // mapping. Non-letter ctrl combos pass through unmodified —
        // shells handle them via their own rebindings.
        if let Some(c) = text.chars().next() {
            let lc = c.to_ascii_lowercase();
            if lc.is_ascii_alphabetic() {
                let byte = (lc as u8) - b'a' + 1;
                return if alt {
                    vec![0x1b, byte]
                } else {
                    vec![byte]
                };
            }
            // Common non-letter ctrl bindings.
            let special: Option<u8> = match c {
                ' ' => Some(0x00),       // Ctrl+Space → NUL
                '[' => Some(0x1b),       // Ctrl+[ → ESC
                '\\' => Some(0x1c),
                ']' => Some(0x1d),
                '/' | '_' => Some(0x1f),
                _ => None,
            };
            if let Some(b) = special {
                return if alt { vec![0x1b, b] } else { vec![b] };
            }
        }
    }

    // Alt + plain char → ESC prefix.
    if alt {
        let mut out = Vec::with_capacity(text.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(text.as_bytes());
        return out;
    }

    text.as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    //! Direct tests against `encode_named` / `encode_char` keep us
    //! independent of winit's `KeyEvent` struct (which is non-trivial
    //! to construct in a unit test). The `encode()` wrapper is just
    //! state-gating + dispatch, exercised in integration.

    use super::*;

    #[test]
    fn key_arrow_up_emits_csi_a() {
        assert_eq!(encode_named(NamedKey::ArrowUp).unwrap(), b"\x1b[A");
    }

    #[test]
    fn key_arrow_down_emits_csi_b() {
        assert_eq!(encode_named(NamedKey::ArrowDown).unwrap(), b"\x1b[B");
    }

    #[test]
    fn key_enter_emits_cr() {
        assert_eq!(encode_named(NamedKey::Enter).unwrap(), b"\r");
    }

    #[test]
    fn key_backspace_emits_del() {
        assert_eq!(encode_named(NamedKey::Backspace).unwrap(), b"\x7f");
    }

    #[test]
    fn key_f1_emits_ss3_p() {
        assert_eq!(encode_named(NamedKey::F1).unwrap(), b"\x1bOP");
    }

    #[test]
    fn key_f5_emits_csi_15_tilde() {
        assert_eq!(encode_named(NamedKey::F5).unwrap(), b"\x1b[15~");
    }

    #[test]
    fn key_ctrl_c_emits_etx() {
        assert_eq!(encode_char("c", true, false), vec![0x03]);
    }

    #[test]
    fn key_ctrl_a_emits_soh() {
        assert_eq!(encode_char("a", true, false), vec![0x01]);
    }

    #[test]
    fn key_ctrl_uppercase_letter_still_maps() {
        // The keymap may give us "C" if shift is held; we should still
        // map ctrl+C → ETX (0x03) rather than something else.
        assert_eq!(encode_char("C", true, false), vec![0x03]);
    }

    #[test]
    fn key_alt_x_prefixes_esc() {
        assert_eq!(encode_char("x", false, true), vec![0x1b, b'x']);
    }

    #[test]
    fn key_ctrl_alt_d_is_esc_eot() {
        assert_eq!(encode_char("d", true, true), vec![0x1b, 0x04]);
    }

    #[test]
    fn key_plain_letter_passes_through() {
        assert_eq!(encode_char("Z", false, false), b"Z".to_vec());
    }

    #[test]
    fn key_ctrl_left_bracket_is_esc() {
        assert_eq!(encode_char("[", true, false), vec![0x1b]);
    }
}
