//! Translate crossterm key events into the byte sequences a PTY child
//! expects on its stdin.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encode a key event into the bytes a typical xterm-style terminal would
/// transmit. Returns an empty slice for keys we don't translate (e.g. modifier-only).
pub fn encode_key(ev: KeyEvent) -> Vec<u8> {
    let alt = ev.modifiers.contains(KeyModifiers::ALT);
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);

    let mut bytes = match ev.code {
        KeyCode::Char(c) => {
            if ctrl {
                ctrl_byte(c).map(|b| vec![b]).unwrap_or_else(|| {
                    let mut s = String::new();
                    s.push(c);
                    s.into_bytes()
                })
            } else {
                let mut s = String::new();
                s.push(c);
                s.into_bytes()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => Vec::new(),
        },
        KeyCode::Null => vec![0],
        _ => Vec::new(),
    };

    if alt && !bytes.is_empty() {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// Map an ASCII char + Ctrl into its control byte, where defined.
fn ctrl_byte(c: char) -> Option<u8> {
    let upper = c.to_ascii_uppercase();
    match upper {
        '@' => Some(0x00),
        'A'..='Z' => Some((upper as u8) - b'A' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        ' ' => Some(0x00),
        '?' => Some(0x7f),
        _ => None,
    }
}
