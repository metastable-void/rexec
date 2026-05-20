#[derive(Default)]
pub struct OutputFilter {
    state: State,
}

#[derive(Default, Clone, Copy)]
enum State {
    #[default]
    Normal,
    Esc,
    Csi,
    StringSeq,
    StringSeqSawEsc,
}

impl OutputFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            self.state = match self.state {
                State::Normal => match b {
                    0x1b => State::Esc,
                    b'\r' => {
                        out.push(b'\n');
                        State::Normal
                    }
                    _ => {
                        out.push(b);
                        State::Normal
                    }
                },
                State::Esc => match b {
                    b'[' => State::Csi,
                    b']' | b'P' | b'X' | b'^' | b'_' => State::StringSeq,
                    0x1b => State::Esc,
                    _ => State::Normal,
                },
                State::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        State::Normal
                    } else {
                        State::Csi
                    }
                }
                State::StringSeq => match b {
                    0x07 => State::Normal,
                    0x1b => State::StringSeqSawEsc,
                    _ => State::StringSeq,
                },
                State::StringSeqSawEsc => match b {
                    b'\\' => State::Normal,
                    0x1b => State::StringSeqSawEsc,
                    _ => State::StringSeq,
                },
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter_all(input: &[u8]) -> Vec<u8> {
        let mut f = OutputFilter::new();
        let mut out = Vec::new();
        f.push(input, &mut out);
        out
    }

    #[test]
    fn passes_through_plain_ascii() {
        assert_eq!(filter_all(b"hello world\n"), b"hello world\n");
    }

    #[test]
    fn strips_sgr_color_sequence() {
        assert_eq!(
            filter_all(b"\x1b[31mred\x1b[0m text"),
            b"red text"
        );
    }

    #[test]
    fn strips_cursor_movement_csi() {
        assert_eq!(filter_all(b"a\x1b[2Jb\x1b[10;20Hc"), b"abc");
    }

    #[test]
    fn strips_osc_terminated_by_bel() {
        assert_eq!(filter_all(b"x\x1b]0;title\x07y"), b"xy");
    }

    #[test]
    fn strips_osc_terminated_by_st() {
        assert_eq!(filter_all(b"x\x1b]0;title\x1b\\y"), b"xy");
    }

    #[test]
    fn replaces_carriage_return_with_newline() {
        assert_eq!(filter_all(b"line\rover"), b"line\nover");
    }

    #[test]
    fn handles_split_csi_across_chunks() {
        let mut f = OutputFilter::new();
        let mut out = Vec::new();
        f.push(b"hi\x1b", &mut out);
        f.push(b"[31mred\x1b[0m!", &mut out);
        assert_eq!(out, b"hired!");
    }

    #[test]
    fn handles_single_char_esc_sequence() {
        // ESC 7 (save cursor) — single char after ESC, no terminator.
        assert_eq!(filter_all(b"a\x1b7b"), b"ab");
    }
}
