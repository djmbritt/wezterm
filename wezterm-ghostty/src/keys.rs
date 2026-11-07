//! Translation of wezterm key events into libghostty-vt key events.
use ghostty_vt_sys::*;
use wezterm_term::{KeyCode, KeyModifiers};

/// The libghostty-vt description of a key press
pub struct GhosttyKeyDesc {
    pub key: GhosttyKey,
    /// The text the key produces (already shifted), if any
    pub utf8: Option<String>,
    /// The codepoint the key produces with no modifiers applied
    pub unshifted: u32,
    /// Modifiers that were consumed to produce `utf8`
    pub consumed: GhosttyMods,
}

pub fn mods_to_ghostty(mods: KeyModifiers) -> GhosttyMods {
    let mut m: u32 = 0;
    if mods.contains(KeyModifiers::SHIFT) {
        m |= GHOSTTY_MODS_SHIFT;
        if mods.contains(KeyModifiers::RIGHT_SHIFT) {
            m |= GHOSTTY_MODS_SHIFT_SIDE;
        }
    }
    if mods.contains(KeyModifiers::CTRL) {
        m |= GHOSTTY_MODS_CTRL;
        if mods.contains(KeyModifiers::RIGHT_CTRL) {
            m |= GHOSTTY_MODS_CTRL_SIDE;
        }
    }
    if mods.contains(KeyModifiers::ALT) {
        m |= GHOSTTY_MODS_ALT;
        if mods.contains(KeyModifiers::RIGHT_ALT) {
            m |= GHOSTTY_MODS_ALT_SIDE;
        }
    }
    if mods.contains(KeyModifiers::SUPER) {
        m |= GHOSTTY_MODS_SUPER;
    }
    m as GhosttyMods
}

/// Map a character to the US-layout physical key that produces it,
/// along with the unshifted codepoint for that key and whether shift
/// is required to produce the character.
fn char_to_key(c: char) -> Option<(GhosttyKey, char, bool)> {
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        let key = GHOSTTY_KEY_A + (lower as u32 - 'a' as u32);
        return Some((key, lower, c.is_ascii_uppercase()));
    }
    if c.is_ascii_digit() {
        let key = GHOSTTY_KEY_DIGIT_0 + (c as u32 - '0' as u32);
        return Some((key, c, false));
    }
    let (key, base, shifted) = match c {
        '`' => (GHOSTTY_KEY_BACKQUOTE, '`', false),
        '~' => (GHOSTTY_KEY_BACKQUOTE, '`', true),
        '\\' => (GHOSTTY_KEY_BACKSLASH, '\\', false),
        '|' => (GHOSTTY_KEY_BACKSLASH, '\\', true),
        '[' => (GHOSTTY_KEY_BRACKET_LEFT, '[', false),
        '{' => (GHOSTTY_KEY_BRACKET_LEFT, '[', true),
        ']' => (GHOSTTY_KEY_BRACKET_RIGHT, ']', false),
        '}' => (GHOSTTY_KEY_BRACKET_RIGHT, ']', true),
        ',' => (GHOSTTY_KEY_COMMA, ',', false),
        '<' => (GHOSTTY_KEY_COMMA, ',', true),
        '.' => (GHOSTTY_KEY_PERIOD, '.', false),
        '>' => (GHOSTTY_KEY_PERIOD, '.', true),
        '=' => (GHOSTTY_KEY_EQUAL, '=', false),
        '+' => (GHOSTTY_KEY_EQUAL, '=', true),
        '-' => (GHOSTTY_KEY_MINUS, '-', false),
        '_' => (GHOSTTY_KEY_MINUS, '-', true),
        '\'' => (GHOSTTY_KEY_QUOTE, '\'', false),
        '"' => (GHOSTTY_KEY_QUOTE, '\'', true),
        ';' => (GHOSTTY_KEY_SEMICOLON, ';', false),
        ':' => (GHOSTTY_KEY_SEMICOLON, ';', true),
        '/' => (GHOSTTY_KEY_SLASH, '/', false),
        '?' => (GHOSTTY_KEY_SLASH, '/', true),
        ' ' => (GHOSTTY_KEY_SPACE, ' ', false),
        '!' => (GHOSTTY_KEY_DIGIT_1, '1', true),
        '@' => (GHOSTTY_KEY_DIGIT_2, '2', true),
        '#' => (GHOSTTY_KEY_DIGIT_3, '3', true),
        '$' => (GHOSTTY_KEY_DIGIT_4, '4', true),
        '%' => (GHOSTTY_KEY_DIGIT_5, '5', true),
        '^' => (GHOSTTY_KEY_DIGIT_6, '6', true),
        '&' => (GHOSTTY_KEY_DIGIT_7, '7', true),
        '*' => (GHOSTTY_KEY_DIGIT_8, '8', true),
        '(' => (GHOSTTY_KEY_DIGIT_9, '9', true),
        ')' => (GHOSTTY_KEY_DIGIT_0, '0', true),
        _ => return None,
    };
    Some((key, base, shifted))
}

fn functional(key: GhosttyKey) -> Option<GhosttyKeyDesc> {
    Some(GhosttyKeyDesc {
        key,
        utf8: None,
        unshifted: 0,
        consumed: 0,
    })
}

pub fn describe_key(key: &KeyCode, mods: KeyModifiers) -> Option<GhosttyKeyDesc> {
    use KeyCode::*;
    let k = match key {
        Char('\r') | Char('\n') | Enter => functional(GHOSTTY_KEY_ENTER),
        Char('\t') | Tab => functional(GHOSTTY_KEY_TAB),
        Char('\u{8}') | Backspace => functional(GHOSTTY_KEY_BACKSPACE),
        Char('\u{1b}') | Escape => functional(GHOSTTY_KEY_ESCAPE),
        Char('\u{7f}') | Delete => functional(GHOSTTY_KEY_DELETE),
        Char(c) => {
            let mut c = *c;
            // When ctrl is held, wezterm may hand us an already
            // control-mapped character; recover the printable key
            if (c as u32) < 0x20 {
                c = ((c as u8) | 0x40).to_ascii_lowercase() as char;
            }
            match char_to_key(c) {
                Some((key, base, shifted)) => {
                    let consumed = if shifted && mods.contains(KeyModifiers::SHIFT) {
                        GHOSTTY_MODS_SHIFT as u16
                    } else {
                        0
                    };
                    Some(GhosttyKeyDesc {
                        key,
                        utf8: Some(c.to_string()),
                        unshifted: base as u32,
                        consumed,
                    })
                }
                None => Some(GhosttyKeyDesc {
                    key: GHOSTTY_KEY_UNIDENTIFIED,
                    utf8: Some(c.to_string()),
                    unshifted: c as u32,
                    consumed: if mods.contains(KeyModifiers::SHIFT) {
                        GHOSTTY_MODS_SHIFT as u16
                    } else {
                        0
                    },
                }),
            }
        }
        Function(n) if (1..=25).contains(n) => functional(GHOSTTY_KEY_F1 + (*n as u32 - 1)),
        Numpad0 => functional(GHOSTTY_KEY_NUMPAD_0),
        Numpad1 => functional(GHOSTTY_KEY_NUMPAD_1),
        Numpad2 => functional(GHOSTTY_KEY_NUMPAD_2),
        Numpad3 => functional(GHOSTTY_KEY_NUMPAD_3),
        Numpad4 => functional(GHOSTTY_KEY_NUMPAD_4),
        Numpad5 => functional(GHOSTTY_KEY_NUMPAD_5),
        Numpad6 => functional(GHOSTTY_KEY_NUMPAD_6),
        Numpad7 => functional(GHOSTTY_KEY_NUMPAD_7),
        Numpad8 => functional(GHOSTTY_KEY_NUMPAD_8),
        Numpad9 => functional(GHOSTTY_KEY_NUMPAD_9),
        Multiply => functional(GHOSTTY_KEY_NUMPAD_MULTIPLY),
        Add => functional(GHOSTTY_KEY_NUMPAD_ADD),
        Separator => functional(GHOSTTY_KEY_NUMPAD_SEPARATOR),
        Subtract => functional(GHOSTTY_KEY_NUMPAD_SUBTRACT),
        Decimal => functional(GHOSTTY_KEY_NUMPAD_DECIMAL),
        Divide => functional(GHOSTTY_KEY_NUMPAD_DIVIDE),
        PageUp => functional(GHOSTTY_KEY_PAGE_UP),
        PageDown => functional(GHOSTTY_KEY_PAGE_DOWN),
        End => functional(GHOSTTY_KEY_END),
        Home => functional(GHOSTTY_KEY_HOME),
        LeftArrow | ApplicationLeftArrow => functional(GHOSTTY_KEY_ARROW_LEFT),
        RightArrow | ApplicationRightArrow => functional(GHOSTTY_KEY_ARROW_RIGHT),
        UpArrow | ApplicationUpArrow => functional(GHOSTTY_KEY_ARROW_UP),
        DownArrow | ApplicationDownArrow => functional(GHOSTTY_KEY_ARROW_DOWN),
        KeyPadHome => functional(GHOSTTY_KEY_NUMPAD_HOME),
        KeyPadEnd => functional(GHOSTTY_KEY_NUMPAD_END),
        KeyPadPageUp => functional(GHOSTTY_KEY_NUMPAD_PAGE_UP),
        KeyPadPageDown => functional(GHOSTTY_KEY_NUMPAD_PAGE_DOWN),
        KeyPadBegin => functional(GHOSTTY_KEY_NUMPAD_BEGIN),
        Insert => functional(GHOSTTY_KEY_INSERT),
        Help => functional(GHOSTTY_KEY_HELP),
        PrintScreen | Print => functional(GHOSTTY_KEY_PRINT_SCREEN),
        Pause => functional(GHOSTTY_KEY_PAUSE),
        CapsLock => functional(GHOSTTY_KEY_CAPS_LOCK),
        NumLock => functional(GHOSTTY_KEY_NUM_LOCK),
        ScrollLock => functional(GHOSTTY_KEY_SCROLL_LOCK),
        Applications | Menu | LeftMenu | RightMenu => functional(GHOSTTY_KEY_CONTEXT_MENU),
        Shift | LeftShift => functional(GHOSTTY_KEY_SHIFT_LEFT),
        RightShift => functional(GHOSTTY_KEY_SHIFT_RIGHT),
        Control | LeftControl => functional(GHOSTTY_KEY_CONTROL_LEFT),
        RightControl => functional(GHOSTTY_KEY_CONTROL_RIGHT),
        Alt | LeftAlt => functional(GHOSTTY_KEY_ALT_LEFT),
        RightAlt => functional(GHOSTTY_KEY_ALT_RIGHT),
        Super | LeftWindows => functional(GHOSTTY_KEY_META_LEFT),
        RightWindows => functional(GHOSTTY_KEY_META_RIGHT),
        Copy => functional(GHOSTTY_KEY_COPY),
        Cut => functional(GHOSTTY_KEY_CUT),
        Paste => functional(GHOSTTY_KEY_PASTE),
        BrowserBack => functional(GHOSTTY_KEY_BROWSER_BACK),
        BrowserForward => functional(GHOSTTY_KEY_BROWSER_FORWARD),
        BrowserRefresh => functional(GHOSTTY_KEY_BROWSER_REFRESH),
        BrowserStop => functional(GHOSTTY_KEY_BROWSER_STOP),
        BrowserSearch => functional(GHOSTTY_KEY_BROWSER_SEARCH),
        BrowserFavorites => functional(GHOSTTY_KEY_BROWSER_FAVORITES),
        BrowserHome => functional(GHOSTTY_KEY_BROWSER_HOME),
        VolumeMute => functional(GHOSTTY_KEY_AUDIO_VOLUME_MUTE),
        VolumeDown => functional(GHOSTTY_KEY_AUDIO_VOLUME_DOWN),
        VolumeUp => functional(GHOSTTY_KEY_AUDIO_VOLUME_UP),
        MediaNextTrack => functional(GHOSTTY_KEY_MEDIA_TRACK_NEXT),
        MediaPrevTrack => functional(GHOSTTY_KEY_MEDIA_TRACK_PREVIOUS),
        MediaStop => functional(GHOSTTY_KEY_MEDIA_STOP),
        MediaPlayPause => functional(GHOSTTY_KEY_MEDIA_PLAY_PAUSE),
        Sleep => functional(GHOSTTY_KEY_SLEEP),
        _ => None,
    };
    k
}
