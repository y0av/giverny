//! Input encoding: egui events → PTY bytes.
//!
//! Legacy xterm encoding for now; the kitty CSI-u path (Shift+Enter etc.)
//! extends `encode_key` by checking `TermMode` kitty flags.

use alacritty_terminal::term::TermMode;
use egui::{Key, Modifiers};

/// xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
fn mods_of(m: Modifiers) -> u8 {
    let mut v = 1;
    if m.shift {
        v += 1;
    }
    if m.alt {
        v += 2;
    }
    if m.ctrl || m.command {
        v += 4;
    }
    v
}

/// Whether the kitty "disambiguate escape codes" mode is active.
pub fn kitty_active(mode: TermMode) -> bool {
    mode.contains(TermMode::DISAMBIGUATE_ESC_CODES)
}

/// Encode a non-text key press. Plain printable keys return `None` (the
/// paired `Event::Text` carries them); Ctrl/Alt combos and specials encode.
pub fn encode_key(key: Key, mods: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    if kitty_active(mode)
        && let Some(seq) = encode_kitty(key, mods)
    {
        return Some(seq);
    }
    encode_legacy(key, mods, mode)
}

/// Kitty CSI-u encoding for the keys where it differs from legacy in ways
/// Claude Code relies on (Shift+Enter newline, disambiguated Esc/Ctrl keys).
fn encode_kitty(key: Key, mods: Modifiers) -> Option<Vec<u8>> {
    let m = mods_of(mods);
    let csi_u = |code: u32, m: u8| -> Vec<u8> {
        if m == 1 {
            format!("\x1b[{code}u").into_bytes()
        } else {
            format!("\x1b[{code};{m}u").into_bytes()
        }
    };
    match key {
        Key::Enter if m > 1 => Some(csi_u(13, m)),
        Key::Escape => Some(csi_u(27, m)),
        Key::Backspace if m > 1 => Some(csi_u(127, m)),
        Key::Tab if mods.alt || (mods.ctrl && mods.shift) => Some(csi_u(9, m)),
        _ => None,
    }
}

fn encode_legacy(key: Key, mods: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let m = mods_of(mods);
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    // Cursor keys: SS3 in app mode (unmodified), CSI otherwise.
    let cursor = |ch: char| -> Vec<u8> {
        if m == 1 {
            if app_cursor {
                format!("\x1bO{ch}").into_bytes()
            } else {
                format!("\x1b[{ch}").into_bytes()
            }
        } else {
            format!("\x1b[1;{m}{ch}").into_bytes()
        }
    };
    let tilde = |code: u8| -> Vec<u8> {
        if m == 1 {
            format!("\x1b[{code}~").into_bytes()
        } else {
            format!("\x1b[{code};{m}~").into_bytes()
        }
    };
    let ss3_or_csi = |ch: char| -> Vec<u8> {
        if m == 1 {
            format!("\x1bO{ch}").into_bytes()
        } else {
            format!("\x1b[1;{m}{ch}").into_bytes()
        }
    };

    let seq = match key {
        Key::Enter => {
            if mods.alt {
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        }
        Key::Backspace => {
            if mods.alt {
                b"\x1b\x7f".to_vec()
            } else {
                b"\x7f".to_vec()
            }
        }
        Key::Tab => {
            if mods.shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        Key::Escape => {
            if mods.alt {
                b"\x1b\x1b".to_vec()
            } else {
                b"\x1b".to_vec()
            }
        }
        Key::ArrowUp => cursor('A'),
        Key::ArrowDown => cursor('B'),
        Key::ArrowRight => cursor('C'),
        Key::ArrowLeft => cursor('D'),
        Key::Home => cursor('H'),
        Key::End => cursor('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => ss3_or_csi('P'),
        Key::F2 => ss3_or_csi('Q'),
        Key::F3 => ss3_or_csi('R'),
        Key::F4 => ss3_or_csi('S'),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        _ => {
            // Ctrl+letter / Ctrl+space etc. (no Text event fires for these).
            if mods.ctrl && !mods.alt {
                let byte = ctrl_byte(key)?;
                return Some(vec![byte]);
            }
            if mods.ctrl && mods.alt {
                let byte = ctrl_byte(key)?;
                return Some(vec![0x1b, byte]);
            }
            // Alt+letter: ESC prefix + lowercase letter.
            if mods.alt && !mods.ctrl {
                let ch = letter_of(key)?;
                let mut v = vec![0x1b];
                v.extend(ch.to_string().into_bytes());
                return Some(v);
            }
            return None;
        }
    };
    Some(seq)
}

fn letter_of(key: Key) -> Option<char> {
    let name = key.name();
    let mut chars = name.chars();
    let c = chars.next()?;
    if chars.next().is_none() && c.is_ascii_alphabetic() {
        Some(c.to_ascii_lowercase())
    } else {
        None
    }
}

fn ctrl_byte(key: Key) -> Option<u8> {
    if let Some(c) = letter_of(key) {
        return Some(c as u8 & 0x1f);
    }
    match key {
        Key::Space => Some(0),
        Key::OpenBracket => Some(0x1b),
        Key::CloseBracket => Some(0x1d),
        Key::Backslash => Some(0x1c),
        Key::Slash => Some(0x1f),
        Key::Minus => Some(0x1f),
        _ => None,
    }
}

/// Mouse buttons/wheel in xterm code space (before modifier/motion offsets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseCode {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    /// Motion with no button held (MOUSE_MOTION mode).
    NoButton,
}

impl MouseCode {
    fn base(self) -> u8 {
        match self {
            MouseCode::Left => 0,
            MouseCode::Middle => 1,
            MouseCode::Right => 2,
            MouseCode::WheelUp => 64,
            MouseCode::WheelDown => 65,
            MouseCode::NoButton => 3,
        }
    }
}

/// Encode a mouse report. `col`/`line` are 0-based viewport cell coords.
/// Wheel events are always "pressed"; motion adds 32.
pub fn encode_mouse(
    code: MouseCode,
    col: u16,
    line: u16,
    pressed: bool,
    motion: bool,
    mods: Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let mut cb = code.base();
    if mods.shift {
        cb += 4;
    }
    if mods.alt {
        cb += 8;
    }
    if mods.ctrl || mods.command {
        cb += 16;
    }
    if motion {
        cb += 32;
    }
    let (x, y) = (col as u32 + 1, line as u32 + 1);
    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if pressed { 'M' } else { 'm' };
        Some(format!("\x1b[<{cb};{x};{y}{suffix}").into_bytes())
    } else {
        // Legacy X10 encoding: release reports button 3; coords cap at 223.
        if x > 223 || y > 223 {
            return None;
        }
        let cb = if pressed { cb } else { (cb & !0b11) | 3 };
        Some(vec![0x1b, b'[', b'M', 32 + cb, 32 + x as u8, 32 + y as u8])
    }
}

/// Sanitize text (typed or pasted) so it cannot inject escape sequences:
/// strips ESC and C1 controls; converts `\r\n`/`\n` to `\r`.
pub fn sanitize_text(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {}
            '\u{80}'..='\u{9f}' => {}
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(b'\r');
            }
            '\n' => out.push(b'\r'),
            _ => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out
}

/// Wrap pasted text for the terminal: bracketed when the mode is on
/// (sanitized so the payload cannot fake the closing bracket), plain
/// sanitized bytes otherwise.
pub fn encode_paste(text: &str, mode: TermMode) -> Vec<u8> {
    let body = sanitize_text(text);
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = Vec::with_capacity(body.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend(body);
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Modifiers {
        Modifiers::NONE
    }

    #[test]
    fn basic_keys() {
        let m = TermMode::empty();
        assert_eq!(encode_key(Key::Enter, none(), m).unwrap(), b"\r");
        assert_eq!(encode_key(Key::Backspace, none(), m).unwrap(), b"\x7f");
        assert_eq!(encode_key(Key::ArrowUp, none(), m).unwrap(), b"\x1b[A");
        assert_eq!(
            encode_key(Key::Tab, Modifiers::SHIFT, m).unwrap(),
            b"\x1b[Z"
        );
        assert_eq!(encode_key(Key::PageDown, none(), m).unwrap(), b"\x1b[6~");
    }

    #[test]
    fn app_cursor_mode_uses_ss3() {
        let m = TermMode::APP_CURSOR;
        assert_eq!(encode_key(Key::ArrowUp, none(), m).unwrap(), b"\x1bOA");
        assert_eq!(
            encode_key(Key::ArrowUp, Modifiers::SHIFT, m).unwrap(),
            b"\x1b[1;2A",
            "modified arrows always CSI"
        );
    }

    #[test]
    fn ctrl_combos() {
        let m = TermMode::empty();
        assert_eq!(encode_key(Key::C, Modifiers::CTRL, m).unwrap(), vec![0x03]);
        assert_eq!(encode_key(Key::A, Modifiers::CTRL, m).unwrap(), vec![0x01]);
        assert_eq!(
            encode_key(Key::Space, Modifiers::CTRL, m).unwrap(),
            vec![0x00]
        );
        assert_eq!(
            encode_key(Key::A, none(), m),
            None,
            "plain letters come via Text"
        );
    }

    #[test]
    fn shift_enter_is_csi_u_under_kitty() {
        let kitty = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(
            encode_key(Key::Enter, Modifiers::SHIFT, kitty).unwrap(),
            b"\x1b[13;2u",
            "the Claude Code newline"
        );
        assert_eq!(encode_key(Key::Enter, none(), kitty).unwrap(), b"\r");
        assert_eq!(encode_key(Key::Escape, none(), kitty).unwrap(), b"\x1b[27u");
    }

    #[test]
    fn paste_is_bracketed_and_sanitized() {
        let out = encode_paste("a\x1b[31mb\r\nc", TermMode::BRACKETED_PASTE);
        assert_eq!(out, b"\x1b[200~a[31mb\rc\x1b[201~");
        let plain = encode_paste("x\ny", TermMode::empty());
        assert_eq!(plain, b"x\ry");
    }

    #[test]
    fn alt_letter_prefixes_escape() {
        let m = TermMode::empty();
        assert_eq!(encode_key(Key::B, Modifiers::ALT, m).unwrap(), b"\x1bb");
    }

    #[test]
    fn sgr_mouse_reports() {
        let m = TermMode::SGR_MOUSE | TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            encode_mouse(MouseCode::Left, 0, 0, true, false, none(), m).unwrap(),
            b"\x1b[<0;1;1M"
        );
        assert_eq!(
            encode_mouse(MouseCode::Left, 10, 4, false, false, none(), m).unwrap(),
            b"\x1b[<0;11;5m"
        );
        assert_eq!(
            encode_mouse(MouseCode::WheelUp, 2, 2, true, false, none(), m).unwrap(),
            b"\x1b[<64;3;3M"
        );
        assert_eq!(
            encode_mouse(MouseCode::Left, 5, 5, true, true, none(), m).unwrap(),
            b"\x1b[<32;6;6M",
            "drag motion adds 32"
        );
    }

    #[test]
    fn legacy_mouse_reports() {
        let m = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            encode_mouse(MouseCode::Left, 0, 0, true, false, none(), m).unwrap(),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        assert_eq!(
            encode_mouse(MouseCode::Left, 0, 0, false, false, none(), m).unwrap(),
            vec![0x1b, b'[', b'M', 35, 33, 33],
            "legacy release is button 3"
        );
        assert!(encode_mouse(MouseCode::Left, 250, 0, true, false, none(), m).is_none());
    }
}
