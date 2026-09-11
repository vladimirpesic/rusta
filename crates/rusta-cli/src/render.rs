//! Output rendering — the single surface the REPL writes to.
//!
//! Production wires `std::io::stdout()`; tests capture a shared buffer, so
//! every assertion runs against exactly what a user would see. Streaming
//! completions print incrementally (`print` without newline, flushed); all
//! other output is line-oriented.

use std::io::Write;

/// Where session output goes.
pub struct Reporter {
    out: Box<dyn Write + Send>,
}

impl Reporter {
    /// A reporter writing to `out`.
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self { out }
    }

    /// The production reporter: standard output.
    pub fn stdout() -> Self {
        Self::new(Box::new(std::io::stdout()))
    }

    /// Writes one line (newline appended), flushed immediately.
    pub fn line(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
        let _ = self.out.flush();
    }

    /// Writes raw text with no newline and flushes — streaming deltas.
    pub fn raw(&mut self, text: &str) {
        let _ = self.out.write_all(text.as_bytes());
        let _ = self.out.flush();
    }

    /// A blank line.
    pub fn blank(&mut self) {
        self.line("");
    }
}
