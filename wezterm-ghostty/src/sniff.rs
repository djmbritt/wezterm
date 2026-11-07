//! A tiny streaming scanner that picks out OSC sequences that
//! libghostty-vt deliberately ignores but wezterm relies upon.
//!
//! Currently that is iTerm2's `OSC 1337 ; SetUserVar=NAME=BASE64 ST`,
//! which wezterm's shell integration (and many user configs) use to pass
//! information from the shell to the GUI.  The bytes are still passed
//! through to libghostty unchanged; this only observes them.

const PREFIX: &[u8] = b"1337;SetUserVar=";
const MAX_LEN: usize = 64 * 1024;

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Esc,
    /// Inside an OSC; the buffer holds the payload so far.  `None` means
    /// we've determined that this OSC isn't one we care about.
    Osc(Option<Vec<u8>>),
    /// Saw ESC inside an OSC (possible ST)
    OscEsc(Option<Vec<u8>>),
}

#[derive(Default)]
pub struct OscSniffer {
    state: State,
}

impl OscSniffer {
    /// Scan `bytes`, returning any `(name, value)` user var assignments
    /// that completed within them.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<(String, String)> {
        let mut found = vec![];
        let mut i = 0;
        while i < bytes.len() {
            match std::mem::take(&mut self.state) {
                State::Ground => match bytes[i..].iter().position(|&b| b == 0x1b) {
                    Some(pos) => {
                        i += pos + 1;
                        self.state = State::Esc;
                    }
                    None => return found,
                },
                State::Esc => {
                    self.state = if bytes[i] == b']' {
                        State::Osc(Some(Vec::new()))
                    } else if bytes[i] == 0x1b {
                        State::Esc
                    } else {
                        State::Ground
                    };
                    i += 1;
                }
                State::Osc(mut buf) => {
                    let b = bytes[i];
                    i += 1;
                    match b {
                        0x07 => {
                            Self::finish(buf, &mut found);
                            self.state = State::Ground;
                        }
                        0x1b => self.state = State::OscEsc(buf),
                        // CAN/SUB abort the sequence
                        0x18 | 0x1a => self.state = State::Ground,
                        _ => {
                            if let Some(v) = buf.as_mut() {
                                v.push(b);
                                let n = v.len();
                                let relevant = if n <= PREFIX.len() {
                                    PREFIX.starts_with(v)
                                } else {
                                    n <= MAX_LEN
                                };
                                if !relevant {
                                    buf = None;
                                }
                            }
                            self.state = State::Osc(buf);
                        }
                    }
                }
                State::OscEsc(buf) => {
                    if bytes[i] == b'\\' {
                        i += 1;
                        Self::finish(buf, &mut found);
                        self.state = State::Ground;
                    } else {
                        // ESC aborts the OSC and begins a new sequence;
                        // reprocess this byte in the Esc state.
                        self.state = State::Esc;
                    }
                }
            }
        }
        found
    }

    fn finish(buf: Option<Vec<u8>>, found: &mut Vec<(String, String)>) {
        let Some(buf) = buf else { return };
        let Some(rest) = buf.strip_prefix(PREFIX) else {
            return;
        };
        let rest = String::from_utf8_lossy(rest);
        if let Some((name, value)) = rest.split_once('=') {
            use base64::Engine;
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(value) {
                found.push((
                    name.to_string(),
                    String::from_utf8_lossy(&decoded).into_owned(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn split_across_chunks() {
        let mut s = OscSniffer::default();
        let seq = b"x\x1b]1337;SetUserVar=foo=YmFy\x1b\\y\x1b]0;title\x07";
        let mut found = vec![];
        for chunk in seq.chunks(3) {
            found.extend(s.scan(chunk));
        }
        assert_eq!(found, vec![("foo".to_string(), "bar".to_string())]);
    }

    #[test]
    fn bel_terminated() {
        let mut s = OscSniffer::default();
        assert_eq!(
            s.scan(b"\x1b]1337;SetUserVar=a=Yg==\x07"),
            vec![("a".to_string(), "b".to_string())]
        );
        assert!(s.scan(b"\x1b]1337;File=foo\x07").is_empty());
    }
}
