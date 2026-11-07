//! wezterm input → Rex input payloads.
//!
//! Rex encodes input on the server, where the authoritative terminal lives,
//! so the client sends key *events* (Ghostty key names, modifiers, text)
//! rather than escape sequences. See rex-linux's `src/app/keys.rs`.
//!
//! wezterm hands panes logical keys, not physical positions, so keys are
//! named by what they produce on a US layout. Text is always sent as typed,
//! so other layouts still type the right characters.
use serde_json::{json, Value};
use termwiz::input::{KeyCode, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Press,
    Release,
}

/// `shift+ctrl+alt+super`, as Rex expects.
pub fn mods(m: Modifiers) -> String {
    [
        (m.contains(Modifiers::SHIFT), "shift"),
        (m.contains(Modifiers::CTRL), "ctrl"),
        (m.contains(Modifiers::ALT), "alt"),
        (m.contains(Modifiers::SUPER), "super"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect::<Vec<_>>()
    .join("+")
}

fn char_name(c: char) -> Option<String> {
    let c = unshifted(c.to_ascii_lowercase());
    Some(match c {
        'a'..='z' => format!("key_{c}"),
        '0'..='9' => format!("digit_{c}"),
        '-' => "minus".into(),
        '=' => "equal".into(),
        '[' => "bracket_left".into(),
        ']' => "bracket_right".into(),
        '\\' => "backslash".into(),
        ';' => "semicolon".into(),
        '\'' => "quote".into(),
        ',' => "comma".into(),
        '.' => "period".into(),
        '/' => "slash".into(),
        '`' => "backquote".into(),
        ' ' => "space".into(),
        _ => return None,
    })
}

/// The unshifted character on a US layout.
fn unshifted(c: char) -> char {
    match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        c => c,
    }
}

/// Ghostty's name for a non-text key.
fn named(code: KeyCode) -> Option<String> {
    use KeyCode as K;
    Some(
        match code {
            K::Enter | K::Char('\r') | K::Char('\n') => "enter",
            K::Tab | K::Char('\t') => "tab",
            K::Escape | K::Char('\x1b') => "escape",
            K::Backspace | K::Char('\x08') | K::Char('\x7f') => "backspace",
            K::Delete => "delete",
            K::Insert => "insert",
            K::LeftArrow | K::ApplicationLeftArrow => "arrow_left",
            K::RightArrow | K::ApplicationRightArrow => "arrow_right",
            K::UpArrow | K::ApplicationUpArrow => "arrow_up",
            K::DownArrow | K::ApplicationDownArrow => "arrow_down",
            K::Home => "home",
            K::End => "end",
            K::PageUp => "page_up",
            K::PageDown => "page_down",
            K::Function(n) if (1..=25).contains(&n) => return Some(format!("f{n}")),
            K::Numpad0 => "numpad_0",
            K::Numpad1 => "numpad_1",
            K::Numpad2 => "numpad_2",
            K::Numpad3 => "numpad_3",
            K::Numpad4 => "numpad_4",
            K::Numpad5 => "numpad_5",
            K::Numpad6 => "numpad_6",
            K::Numpad7 => "numpad_7",
            K::Numpad8 => "numpad_8",
            K::Numpad9 => "numpad_9",
            K::Multiply => "numpad_multiply",
            K::Add => "numpad_add",
            K::Subtract => "numpad_subtract",
            K::Decimal => "numpad_decimal",
            K::Divide => "numpad_divide",
            K::Separator => "numpad_separator",
            K::KeyPadHome => "numpad_home",
            K::KeyPadEnd => "numpad_end",
            K::KeyPadPageUp => "numpad_page_up",
            K::KeyPadPageDown => "numpad_page_down",
            K::KeyPadBegin => "numpad_begin",
            K::Pause => "pause",
            K::CapsLock => "caps_lock",
            K::NumLock => "num_lock",
            K::ScrollLock => "scroll_lock",
            K::PrintScreen | K::Print => "print_screen",
            K::Menu | K::Applications => "context_menu",
            K::Help => "help",
            K::Shift | K::LeftShift => "shift_left",
            K::RightShift => "shift_right",
            K::Control | K::LeftControl => "control_left",
            K::RightControl => "control_right",
            K::Alt | K::LeftAlt => "alt_left",
            K::RightAlt => "alt_right",
            K::Super | K::LeftWindows => "meta_left",
            K::RightWindows => "meta_right",
            _ => return None,
        }
        .to_owned(),
    )
}

/// A `client.key` payload, or `None` for keys Rex can't take.
pub fn key_payload(code: KeyCode, m: Modifiers, action: KeyAction) -> Option<Value> {
    let plain = !(m.contains(Modifiers::CTRL) || m.contains(Modifiers::SUPER));
    let alt = m.contains(Modifiers::ALT);
    let (name, text, unshifted_cp) = match code {
        KeyCode::Char(c) if !c.is_control() => {
            let lower = c.to_lowercase().next().unwrap_or(c);
            let base = unshifted(lower);
            let name = char_name(c).unwrap_or_else(|| "unidentified".into());
            // Text goes along unless a command modifier turns the key into a
            // shortcut; Alt keeps it (the server adds the ESC prefix).
            let text = if plain || alt {
                c.to_string()
            } else {
                String::new()
            };
            (name, text, base as u32)
        }
        other => {
            let name = named(other)?;
            let text = match other {
                KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n') if plain && !alt => {
                    "\n".to_string()
                }
                KeyCode::Tab | KeyCode::Char('\t') if plain && !alt => "\t".to_string(),
                _ => String::new(),
            };
            (name, text, 0)
        }
    };
    let (action, text) = match action {
        KeyAction::Press => ("press", text),
        KeyAction::Release => ("release", String::new()),
    };
    // Shift is consumed when it picked the text ("A"); Enter and Tab type
    // the same either way, so their shift stays visible to the encoder.
    let consumed = m.contains(Modifiers::SHIFT)
        && !text.is_empty()
        && matches!(code, KeyCode::Char(c) if !c.is_control());
    Some(json!({
        "key": name,
        "mods": mods(m),
        "consumed_mods": if consumed { "shift" } else { "" },
        "text": text,
        "unshifted_codepoint": unshifted_cp,
        "composing": false,
        "action": action,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

/// A `block.event` mouse payload. `x`/`y` are cells in the viewport.
/// Shift is never sent: it is the local-selection override.
pub fn mouse_payload(
    action: MouseAction,
    button: Option<&str>,
    x: usize,
    y: usize,
    m: Modifiers,
) -> Value {
    let mut payload = json!({
        "action": match action {
            MouseAction::Press => "press",
            MouseAction::Release => "release",
            MouseAction::Motion => "motion",
        },
        "x": x,
        "y": y,
        "mods": mods(m - Modifiers::SHIFT),
    });
    if let Some(button) = button {
        payload["button"] = json!(button);
    }
    payload
}

/// A `block.event` wheel payload: negative `delta_y` scrolls up.
pub fn wheel_payload(x: usize, y: usize, delta_x: i32, delta_y: i32) -> Value {
    json!({"x": x, "y": y, "delta_x": delta_x, "delta_y": delta_y})
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn typed_capital_consumes_shift() {
        let p = key_payload(KeyCode::Char('A'), Modifiers::SHIFT, KeyAction::Press).unwrap();
        assert_eq!(p["key"], "key_a");
        assert_eq!(p["text"], "A");
        assert_eq!(p["mods"], "shift");
        assert_eq!(p["consumed_mods"], "shift");
        assert_eq!(p["unshifted_codepoint"], 'a' as u32);
        assert_eq!(p["action"], "press");
    }

    #[test]
    fn ctrl_keys_send_no_text() {
        let p = key_payload(KeyCode::Char('c'), Modifiers::CTRL, KeyAction::Press).unwrap();
        assert_eq!(p["key"], "key_c");
        assert_eq!(p["text"], "");
        assert_eq!(p["mods"], "ctrl");
    }

    #[test]
    fn alt_keeps_text() {
        let p = key_payload(KeyCode::Char('x'), Modifiers::ALT, KeyAction::Press).unwrap();
        assert_eq!(p["text"], "x");
        assert_eq!(p["mods"], "alt");
    }

    #[test]
    fn shifted_symbols_and_named_keys() {
        let p = key_payload(KeyCode::Char('!'), Modifiers::SHIFT, KeyAction::Press).unwrap();
        assert_eq!(p["key"], "digit_1");
        assert_eq!(p["unshifted_codepoint"], '1' as u32);

        let p = key_payload(KeyCode::Enter, Modifiers::NONE, KeyAction::Press).unwrap();
        assert_eq!(p["key"], "enter");
        assert_eq!(p["text"], "\n");

        let p = key_payload(KeyCode::Enter, Modifiers::SHIFT, KeyAction::Press).unwrap();
        assert_eq!(p["consumed_mods"], "", "shift stays visible on Enter");

        let p = key_payload(KeyCode::UpArrow, Modifiers::NONE, KeyAction::Press).unwrap();
        assert_eq!(p["key"], "arrow_up");
        assert_eq!(p["text"], "");

        assert_eq!(
            key_payload(KeyCode::Function(12), Modifiers::NONE, KeyAction::Press).unwrap()["key"],
            "f12"
        );
        assert!(key_payload(KeyCode::Function(40), Modifiers::NONE, KeyAction::Press).is_none());
    }

    #[test]
    fn releases_carry_no_text() {
        let p = key_payload(KeyCode::Char('a'), Modifiers::NONE, KeyAction::Release).unwrap();
        assert_eq!(p["action"], "release");
        assert_eq!(p["text"], "");
    }

    #[test]
    fn non_ascii_text_is_unidentified_but_typed() {
        let p = key_payload(KeyCode::Char('é'), Modifiers::NONE, KeyAction::Press).unwrap();
        assert_eq!(p["key"], "unidentified");
        assert_eq!(p["text"], "é");
    }

    #[test]
    fn mouse_drops_shift() {
        let p = mouse_payload(
            MouseAction::Press,
            Some("left"),
            3,
            4,
            Modifiers::SHIFT | Modifiers::CTRL,
        );
        assert_eq!(p["mods"], "ctrl");
        assert_eq!(p["button"], "left");
        assert_eq!(p["x"], 3);
    }
}
