use ghostty_vt_sys::*;

#[test]
fn hello() {
    unsafe {
        let mut term: GhosttyTerminal = core::ptr::null_mut();
        assert_eq!(ghostty_terminal_new(core::ptr::null(), &mut term, 80, 24), GHOSTTY_SUCCESS);
        let text = b"Hello, \x1b[1;32mworld\x1b[0m!\r\n";
        ghostty_terminal_vt_write(term, text.as_ptr(), text.len());
        let mut x: u16 = 99;
        ghostty_terminal_get(term, GHOSTTY_TERMINAL_DATA_CURSOR_Y, &mut x as *mut u16 as *mut _);
        assert_eq!(x, 1);
        ghostty_terminal_free(term);
    }
}
