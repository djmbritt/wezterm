use super::*;
use std::sync::Mutex;
use termwiz::color::ColorAttribute;
use wezterm_term::color::ColorPalette;
use wezterm_term::{Intensity, SemanticType};

#[derive(Debug)]
struct TestConfig {
    scrollback: usize,
}

impl TerminalConfiguration for TestConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Sink {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

#[derive(Clone, Default)]
struct Alerts(Arc<Mutex<Vec<Alert>>>);
impl AlertHandler for Alerts {
    fn alert(&mut self, alert: Alert) {
        self.0.lock().unwrap().push(alert);
    }
}

fn term_with(rows: usize, cols: usize, scrollback: usize) -> (Terminal, Sink) {
    let sink = Sink::default();
    let term = Terminal::new(
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 10,
            pixel_height: rows * 20,
            dpi: 96,
        },
        Arc::new(TestConfig { scrollback }),
        "WezTerm",
        "test",
        Box::new(sink.clone()),
    );
    (term, sink)
}

fn screen_text(term: &mut Terminal) -> Vec<String> {
    let dims = term.dimensions();
    let (_, lines) = term.get_lines(dims.physical_top..dims.physical_top + dims.viewport_rows as isize);
    lines
        .iter()
        .map(|l| l.as_str().trim_end().to_string())
        .collect()
}

#[test]
fn basic_text_and_attrs() {
    let (mut term, _) = term_with(4, 20, 100);
    term.advance_bytes("hello \x1b[1;31mworld\x1b[0m\r\nline two");
    assert_eq!(screen_text(&mut term), vec!["hello world", "line two", "", ""]);

    let (_, lines) = term.get_lines(0..1);
    let cell = lines[0].get_cell(6).unwrap();
    assert_eq!(cell.str(), "w");
    assert_eq!(cell.attrs().intensity(), Intensity::Bold);
    assert_eq!(cell.attrs().foreground(), ColorAttribute::PaletteIndex(1));
    let plain = lines[0].get_cell(0).unwrap();
    assert_eq!(plain.attrs().foreground(), ColorAttribute::Default);

    let cursor = term.cursor_pos();
    assert_eq!((cursor.x, cursor.y), (8, 1));
}

#[test]
fn truecolor_and_wide() {
    let (mut term, _) = term_with(2, 10, 100);
    term.advance_bytes("\x1b[38;2;1;2;3m好x\x1b[0m");
    let (_, lines) = term.get_lines(0..1);
    let line = &lines[0];
    assert_eq!(line.as_str().trim_end(), "好x");
    let cell = line.get_cell(0).unwrap();
    assert_eq!(cell.width(), 2);
    assert!(matches!(
        cell.attrs().foreground(),
        ColorAttribute::TrueColorWithDefaultFallback(_)
    ));
    assert_eq!(line.get_cell(2).unwrap().str(), "x");
}

#[test]
fn scrolling_keeps_stable_indices() {
    let (mut term, _) = term_with(3, 10, 1000);
    for i in 0..10 {
        term.advance_bytes(format!("line{i}\r\n"));
    }
    let dims = term.dimensions();
    assert_eq!(dims.viewport_rows, 3);
    assert_eq!(dims.scrollback_top, 0);
    assert_eq!(dims.scrollback_rows, 11 + 2 - 2 + 0 + dims.viewport_rows - 3 + 0 + 0);
    // line0 is always at stable row 0
    let (first, lines) = term.get_lines(0..2);
    assert_eq!(first, 0);
    assert_eq!(lines[0].as_str().trim_end(), "line0");
    assert_eq!(lines[1].as_str().trim_end(), "line1");
    assert_eq!(screen_text(&mut term), vec!["line8", "line9", ""]);
}

#[test]
fn dirty_tracking() {
    let (mut term, _) = term_with(3, 10, 100);
    term.advance_bytes("aaa\r\nbbb\r\nccc");
    let seq = term.current_seqno();
    let _ = term.get_lines(0..3);
    // Only modify the middle row
    term.advance_bytes("\x1b[2;1HBBB");
    let changed = term.get_changed_stable_rows(0..3, seq);
    // libghostty may also flag the row the cursor left; row 0 is untouched
    assert!(changed.contains(&(1..2)), "{changed:?}");
    assert!(!changed.iter().any(|r| r.contains(&0)), "{changed:?}");
    assert_eq!(screen_text(&mut term), vec!["aaa", "BBB", "ccc"]);
    // Nothing changed since now
    let seq = term.current_seqno();
    assert!(term.get_changed_stable_rows(0..3, seq).is_empty());
}

#[test]
fn pruning_advances_offset() {
    let (mut term, _) = term_with(2, 10, 10);
    for i in 0..5000 {
        term.advance_bytes(format!("{i}\r\n"));
    }
    let dims = term.dimensions();
    assert!(dims.scrollback_top > 0, "{dims:?}");
    // The stable rows of the most recent output are exact
    let (_, lines) = term.get_lines(dims.physical_top - 1..dims.physical_top);
    assert_eq!(lines[0].as_str().trim_end(), "4998");
    assert_eq!(dims.physical_top, 4999);
}

#[test]
fn alt_screen() {
    let (mut term, _) = term_with(3, 10, 100);
    term.advance_bytes("primary");
    term.advance_bytes("\x1b[?1049h");
    assert!(term.is_alt_screen_active());
    term.advance_bytes("alt");
    // 1049 preserves the cursor position
    assert_eq!(screen_text(&mut term), vec!["       alt", "", ""]);
    term.advance_bytes("\x1b[?1049l");
    assert!(!term.is_alt_screen_active());
    assert_eq!(screen_text(&mut term), vec!["primary", "", ""]);
}

#[test]
fn title_and_user_vars() {
    let (mut term, _) = term_with(3, 10, 100);
    let alerts = Alerts::default();
    term.set_notification_handler(Box::new(alerts.clone()));
    term.advance_bytes("\x1b]2;my title\x07");
    assert_eq!(term.get_title(), "my title");
    term.advance_bytes("\x1b]1337;SetUserVar=foo=YmFy\x07");
    assert_eq!(term.user_vars().get("foo").map(|s| s.as_str()), Some("bar"));
    term.advance_bytes("\x07");
    let got = alerts.0.lock().unwrap().clone();
    assert!(got.contains(&Alert::WindowTitleChanged("my title".to_string())));
    assert!(got.contains(&Alert::SetUserVar {
        name: "foo".to_string(),
        value: "bar".to_string()
    }));
    assert!(got.contains(&Alert::Bell));
}

#[test]
fn cwd() {
    let (mut term, _) = term_with(3, 10, 100);
    term.advance_bytes("\x1b]7;file://localhost/tmp/foo\x07");
    assert_eq!(
        term.get_current_dir().map(|u| u.path().to_string()),
        Some("/tmp/foo".to_string())
    );
}

#[test]
fn device_status_reply() {
    let (mut term, sink) = term_with(3, 10, 100);
    term.advance_bytes("ab\x1b[6n");
    assert_eq!(sink.take(), b"\x1b[1;3R");
}

#[test]
fn keys() {
    let (mut term, sink) = term_with(3, 10, 100);
    term.key_down(KeyCode::Char('a'), KeyModifiers::NONE).unwrap();
    assert_eq!(sink.take(), b"a");
    term.key_down(KeyCode::Char('A'), KeyModifiers::SHIFT).unwrap();
    assert_eq!(sink.take(), b"A");
    term.key_down(KeyCode::Char('c'), KeyModifiers::CTRL).unwrap();
    assert_eq!(sink.take(), b"\x03");
    term.key_down(KeyCode::Char('a'), KeyModifiers::ALT).unwrap();
    assert_eq!(sink.take(), b"\x1ba");
    term.key_down(KeyCode::Char('\r'), KeyModifiers::NONE).unwrap();
    assert_eq!(sink.take(), b"\r");
    term.key_down(KeyCode::Char('\u{8}'), KeyModifiers::NONE).unwrap();
    assert_eq!(sink.take(), b"\x7f");
    term.key_down(KeyCode::UpArrow, KeyModifiers::NONE).unwrap();
    assert_eq!(sink.take(), b"\x1b[A");
    term.key_down(KeyCode::Function(1), KeyModifiers::NONE).unwrap();
    assert_eq!(sink.take(), b"\x1bOP");
    term.key_down(KeyCode::Char('é'), KeyModifiers::NONE).unwrap();
    assert_eq!(sink.take(), "é".as_bytes());

    // application cursor keys
    term.advance_bytes("\x1b[?1h");
    term.key_down(KeyCode::UpArrow, KeyModifiers::NONE).unwrap();
    assert_eq!(sink.take(), b"\x1bOA");

    // kitty keyboard protocol
    term.advance_bytes("\x1b[>1u");
    assert!(matches!(
        term.get_keyboard_encoding(),
        KeyboardEncoding::Kitty(_)
    ));
    term.key_down(KeyCode::Char('c'), KeyModifiers::CTRL).unwrap();
    assert_eq!(sink.take(), b"\x1b[99;5u");
}

#[test]
fn mouse() {
    let (mut term, sink) = term_with(3, 10, 100);
    assert!(!term.is_mouse_grabbed());
    term.advance_bytes("\x1b[?1000h\x1b[?1006h");
    assert!(term.is_mouse_grabbed());
    term.mouse_event(MouseEvent {
        kind: MouseEventKind::Press,
        x: 2,
        y: 1,
        x_pixel_offset: 1,
        y_pixel_offset: 1,
        button: MouseButton::Left,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    assert_eq!(sink.take(), b"\x1b[<0;3;2M");
}

#[test]
fn paste_and_focus() {
    let (mut term, sink) = term_with(3, 10, 100);
    term.send_paste("hi").unwrap();
    assert_eq!(sink.take(), b"hi");
    term.advance_bytes("\x1b[?2004h\x1b[?1004h");
    term.send_paste("hi").unwrap();
    assert_eq!(sink.take(), b"\x1b[200~hi\x1b[201~");
    term.focus_changed(false);
    assert_eq!(sink.take(), b"\x1b[O");
    term.focus_changed(true);
    assert_eq!(sink.take(), b"\x1b[I");
}

#[test]
fn palette_override() {
    let (mut term, _) = term_with(3, 10, 100);
    let alerts = Alerts::default();
    term.set_notification_handler(Box::new(alerts.clone()));
    assert_eq!(term.palette(), ColorPalette::default());
    term.advance_bytes("\x1b]4;1;rgb:12/34/56\x07");
    let p = term.palette();
    assert_eq!(p.colors.0[1].to_srgb_u8(), (0x12, 0x34, 0x56, 0xff));
    assert!(alerts.0.lock().unwrap().contains(&Alert::PaletteChanged));
}

#[test]
fn semantic_zones() {
    let (mut term, _) = term_with(5, 20, 100);
    term.advance_bytes("\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out\r\n");
    let zones = term.get_semantic_zones().unwrap();
    let types: Vec<SemanticType> = zones.iter().map(|z| z.semantic_type).collect();
    assert!(types.contains(&SemanticType::Prompt), "{zones:?}");
    assert!(types.contains(&SemanticType::Input), "{zones:?}");
}

#[test]
fn erase_scrollback_keeps_viewport() {
    let (mut term, _) = term_with(3, 10, 1000);
    for i in 0..10 {
        term.advance_bytes(format!("l{i}\r\n"));
    }
    term.advance_bytes("x");
    let before = term.dimensions();
    term.erase_scrollback();
    let after = term.dimensions();
    assert_eq!(after.scrollback_rows, 3);
    assert_eq!(after.physical_top, before.physical_top);
    assert_eq!(screen_text(&mut term), vec!["l8", "l9", "x"]);
}

#[test]
fn logical_lines() {
    let (mut term, _) = term_with(4, 5, 100);
    term.advance_bytes("abcdefgh\r\nxy");
    let mut groups = vec![];
    term.for_each_logical_line_in_stable_range(0..4, |range, lines| {
        let s: String = lines.iter().map(|l| l.as_str().to_string()).collect();
        groups.push((range, s.trim_end().to_string()));
        true
    });
    assert_eq!(groups[0], (0..2, "abcdefgh".to_string()));
    assert_eq!(groups[1], (2..3, "xy".to_string()));
}

#[test]
fn resize_keeps_content_reachable() {
    let (mut term, _) = term_with(10, 20, 100);
    term.advance_bytes("first\r\nsecond\r\nthird\r\nfourth");
    let size = |rows| TerminalSize {
        rows,
        cols: 20,
        pixel_width: 200,
        pixel_height: rows * 20,
        dpi: 96,
    };
    term.resize(size(2));
    assert_eq!(screen_text(&mut term), vec!["third", "fourth"]);
    assert_eq!(term.cursor_pos().y, 1);
    term.resize(size(10));
    let dims = term.dimensions();
    let (_, lines) = term.get_lines(dims.scrollback_top..dims.physical_top + 10);
    let text: Vec<String> = lines
        .iter()
        .map(|l| l.as_str().trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(text, vec!["first", "second", "third", "fourth"]);
}
