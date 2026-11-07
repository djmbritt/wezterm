use ghostty_vt_sys::*;

unsafe fn get16(t: GhosttyTerminal, d: GhosttyTerminalData) -> u16 {
    let mut x: u16 = 0;
    ghostty_terminal_get(t, d, &mut x as *mut u16 as *mut _);
    x
}

#[test]
fn shrink_rows() {
    unsafe {
        let mut t: GhosttyTerminal = core::ptr::null_mut();
        ghostty_terminal_new(core::ptr::null(), &mut t, 20, 10);
        let s = b"first\r\nsecond\r\nthird\r\nfourth";
        ghostty_terminal_vt_write(t, s.as_ptr(), s.len());
        ghostty_terminal_resize(t, 20, 2, 10, 20);
        let (x, y) = (get16(t, GHOSTTY_TERMINAL_DATA_CURSOR_X), get16(t, GHOSTTY_TERMINAL_DATA_CURSOR_Y));
        let mut h: usize = 0;
        ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS, &mut h as *mut usize as *mut _);
        eprintln!("raw: cursor=({x},{y}) history={h}");
        ghostty_terminal_free(t);
    }
}
