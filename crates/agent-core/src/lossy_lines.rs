//! The lines of a command's output, whatever bytes it prints.
//!
//! A program may print bytes that are not UTF-8: a Latin-1 fixture, a
//! Windows-1252 log, a binary file. [`tokio::io::AsyncBufReadExt::lines`]
//! fails at the first such byte, and a reader that stops there loses the rest
//! of the output and leaves the pipe undrained, so a command that goes on
//! writing blocks once the pipe is full. [`LossyLines`] reads the bytes
//! themselves and replaces what is not UTF-8 with U+FFFD, so every line
//! arrives and the pipe is read to its end.
//!
//! A line is held in memory only up to [`MAX_LINE_BYTES`]: a longer one — a
//! command printing megabytes without a newline — arrives in pieces of at
//! most that size, each split between characters, so the output caps of the
//! callers bound memory as they count.

use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// The longest piece of a line [`LossyLines`] holds before handing it on.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// The lines of `reader`, each without its `\n` or `\r\n`, with what is not
/// UTF-8 replaced by U+FFFD; a line longer than [`MAX_LINE_BYTES`] in pieces.
pub struct LossyLines<R> {
    reader: R,
    /// The line being read. Bytes a cancelled read took stay here for the next
    /// call, so [`next_line`](Self::next_line) is cancel safe.
    line: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> LossyLines<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            line: Vec::new(),
        }
    }

    /// The next line, or `None` at the end of the stream. The last line may
    /// end without a newline; a line longer than [`MAX_LINE_BYTES`] comes as
    /// several, split between characters.
    ///
    /// Cancel safe, as `Lines::next_line` is: a call dropped inside
    /// `tokio::select!` loses nothing, and the next call carries on with the
    /// same line. Bytes move from the reader to the line in one step, with no
    /// await between taking them and consuming them.
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                if self.line.is_empty() {
                    return Ok(None);
                }
                let line = std::mem::take(&mut self.line);
                return Ok(Some(lossy(line)));
            }
            let room = MAX_LINE_BYTES.saturating_sub(self.line.len());
            let window = &available[..available.len().min(room)];
            if let Some(end) = window.iter().position(|&b| b == b'\n') {
                self.line.extend_from_slice(&window[..end]);
                self.reader.consume(end + 1);
                let mut line = std::mem::take(&mut self.line);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(lossy(line)));
            }
            let taken = window.len();
            self.line.extend_from_slice(window);
            self.reader.consume(taken);
            if self.line.len() >= MAX_LINE_BYTES {
                // A piece of a long line, cut between characters: the start
                // of a character split by the cut waits for the next piece.
                let cut = char_boundary(&self.line);
                let rest = self.line.split_off(cut);
                let piece = std::mem::replace(&mut self.line, rest);
                return Ok(Some(lossy(piece)));
            }
        }
    }
}

/// `bytes` as text, with what is not UTF-8 replaced by U+FFFD.
fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|invalid| String::from_utf8_lossy(invalid.as_bytes()).into_owned())
}

/// Where `bytes` can be cut without splitting the UTF-8 character at its
/// end: before an incomplete last character, else at the end.
fn char_boundary(bytes: &[u8]) -> usize {
    // A character is at most four bytes: look back over at most three.
    for back in 1..=3.min(bytes.len()) {
        let byte = bytes[bytes.len() - back];
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue; // a continuation byte
        }
        let width = match byte {
            b if b < 0x80 => 1,
            b if b >= 0xF0 => 4,
            b if b >= 0xE0 => 3,
            b if b >= 0xC0 => 2,
            _ => 1,
        };
        return if width > back {
            bytes.len() - back
        } else {
            bytes.len()
        };
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncWriteExt, BufReader};

    use super::*;

    async fn all_lines(bytes: &[u8]) -> Vec<String> {
        let mut lines = LossyLines::new(BufReader::new(bytes));
        let mut out = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            out.push(line);
        }
        out
    }

    /// A line longer than the limit, without a newline, arrives in pieces of
    /// at most the limit, cut between characters, and nothing is lost.
    #[tokio::test]
    async fn a_long_line_arrives_in_bounded_pieces() {
        let mut bytes = "é".repeat(MAX_LINE_BYTES).into_bytes(); // two bytes each
        bytes.insert(0, b'x'); // so a cut at the limit falls inside a character
        bytes.extend_from_slice(b"\nnext");
        let lines = all_lines(&bytes).await;
        assert!(lines.len() >= 3, "{}", lines.len());
        assert!(lines.iter().all(|line| line.len() <= MAX_LINE_BYTES));
        assert!(lines.iter().all(|line| !line.contains('\u{fffd}')));
        let whole: String = lines[..lines.len() - 1].concat();
        assert_eq!(whole.len(), 1 + 2 * MAX_LINE_BYTES);
        assert_eq!(lines.last().map(String::as_str), Some("next"));
    }

    /// Lines split as `lines()` splits them: on `\n` and `\r\n`, an empty line
    /// kept, a last line without a newline kept, nothing after the end.
    #[tokio::test]
    async fn lines_end_where_lines_end() {
        assert_eq!(
            all_lines(b"one\r\ntwo\n\nthree").await,
            ["one", "two", "", "three"]
        );
        assert_eq!(all_lines(b"one\n").await, ["one"]);
        assert!(all_lines(b"").await.is_empty());
    }

    /// A byte that is not UTF-8 becomes U+FFFD, and the lines after it are
    /// read as before.
    #[tokio::test]
    async fn a_byte_that_is_not_utf8_does_not_end_the_output() {
        assert_eq!(
            all_lines(b"before\ncaf\xe9\n\xff\xfe\nafter\n").await,
            ["before", "caf\u{fffd}", "\u{fffd}\u{fffd}", "after"]
        );
    }

    /// A read dropped halfway through a line, as `tokio::select!` drops the
    /// branch that loses, keeps what it took for the next call.
    #[tokio::test]
    async fn a_cancelled_read_loses_nothing() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let mut lines = LossyLines::new(BufReader::new(reader));

        writer.write_all(b"first ha").await.unwrap();
        let cancelled = tokio::time::timeout(Duration::from_millis(50), lines.next_line()).await;
        assert!(cancelled.is_err(), "a line without its end is not a line");

        writer.write_all(b"lf\nsecond").await.unwrap();
        drop(writer);
        assert_eq!(
            lines.next_line().await.unwrap().as_deref(),
            Some("first half")
        );
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("second"));
        assert_eq!(lines.next_line().await.unwrap(), None);
    }
}
