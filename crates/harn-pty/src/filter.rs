//! ConPTY asks its terminal for win32-input-mode (`CSI ? 9001 h`). Passed through, the request
//! switches the caller's own console to win32-encoded input, and conhost then delivers keys twice
//! (as VT and again win32-encoded); the agent sees an arrow key as Esc plus text. We answer like a
//! plain VT terminal, by not passing the request on: ConPTY accepts VT input regardless.

use std::borrow::Cow;

/// `CSI ? 9001`; followed by `h` (set) or `l` (reset).
const MODE_PREFIX: &[u8] = b"\x1b[?9001";
const MODE_FINALS: &[u8] = b"hl";

#[derive(Debug, Default)]
pub(crate) struct Win32InputModeFilter {
    /// A trailing partial request, completed (or not) by the next chunk.
    held: Vec<u8>,
}

impl Win32InputModeFilter {
    pub(crate) fn filter<'a>(&mut self, chunk: &'a [u8]) -> Cow<'a, [u8]> {
        if self.held.is_empty() && find(chunk, MODE_PREFIX).is_none() && partial_len(chunk) == 0 {
            return Cow::Borrowed(chunk);
        }
        let mut data = std::mem::take(&mut self.held);
        data.extend_from_slice(chunk);
        let keep = data.len() - partial_len(&data);
        self.held = data.split_off(keep);
        Cow::Owned(strip(&data))
    }

    /// Bytes still held back when the stream ends.
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }
}

fn strip(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut rest = data;
    while let Some(pos) = find(rest, MODE_PREFIX) {
        let after = pos + MODE_PREFIX.len();
        out.extend_from_slice(&rest[..pos]);
        if rest.get(after).is_some_and(|b| MODE_FINALS.contains(b)) {
            rest = &rest[after + 1..];
        } else {
            out.extend_from_slice(&rest[pos..after]);
            rest = &rest[after..];
        }
    }
    out.extend_from_slice(rest);
    out
}

/// Length of the longest suffix of `data` that could still grow into a full request.
fn partial_len(data: &[u8]) -> usize {
    (1..=MODE_PREFIX.len())
        .rev()
        .find(|&len| data.ends_with(&MODE_PREFIX[..len]))
        .unwrap_or(0)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut filter = Win32InputModeFilter::default();
        let mut out: Vec<u8> = (chunks.iter())
            .flat_map(|chunk| filter.filter(chunk).into_owned())
            .collect();
        out.extend(filter.finish());
        out
    }

    #[test]
    fn requests_are_dropped_also_when_split() {
        let out = run(&[b"\x1b[6n\x1b[?9001h\x1b[?1004hhi\x1b[?9001l"]);
        assert_eq!(out, b"\x1b[6n\x1b[?1004hhi");
        assert_eq!(run(&[b"a\x1b[?90", b"01hb"]), b"ab");
        assert_eq!(run(&[b"a\x1b", b"[?9001", b"lb"]), b"ab");
    }

    #[test]
    fn everything_else_passes_untouched() {
        let chunk = b"\x1b[?25hhello";
        let mut filter = Win32InputModeFilter::default();
        assert!(matches!(filter.filter(chunk), Cow::Borrowed(c) if c == chunk));
        for kept in [&b"\x1b[?90010h"[..], b"\x1b[?9001;1004h", b"tail\x1b[?9"] {
            assert_eq!(run(&[kept]), kept);
        }
        assert_eq!(run(&[b"x\x1b[?90", b"x"]), b"x\x1b[?90x");
    }
}
