//! The lines of a command's output, whatever bytes it prints.
//!
//! A program may print bytes that are not UTF-8: a Latin-1 fixture, a
//! Windows-1252 log, a binary file. [`tokio::io::AsyncBufReadExt::lines`]
//! fails at the first such byte, and a reader that stops there loses the rest
//! of the output and leaves the pipe undrained, so a command that goes on
//! writing blocks once the pipe is full. [`LossyLines`] reads the bytes
//! themselves and replaces what is not UTF-8 with U+FFFD, so every line
//! arrives and the pipe is read to its end.

use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// The lines of `reader`, each without its `\n` or `\r\n`, with what is not
/// UTF-8 replaced by U+FFFD.
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
    /// end without a newline.
    ///
    /// Cancel safe, as `Lines::next_line` is: a call dropped inside
    /// `tokio::select!` loses nothing, and the next call carries on with the
    /// same line.
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        let read = self.reader.read_until(b'\n', &mut self.line).await?;
        if read == 0 && self.line.is_empty() {
            return Ok(None);
        }
        let mut line = std::mem::take(&mut self.line);
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        }
        Ok(Some(String::from_utf8(line).unwrap_or_else(|invalid| {
            String::from_utf8_lossy(invalid.as_bytes()).into_owned()
        })))
    }
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
