//! Strips terminal control sequences from text before the REPL and headless
//! printers write it to a terminal.
//!
//! Captured command output can contain terminal control sequences — colours,
//! cursor moves, alternate-screen switches, hyperlinks, `\r` spinners, bells,
//! … — and any sequence left intact is executed by the user's terminal. This
//! strips everything a terminal would interpret as a command rather than
//! printable text, keeping only `\n` and `\t`.
//!
//! An incomplete trailing sequence is held until the next chunk, so a
//! sequence split across chunks cannot slip through. The hold is bounded, 4
//! KiB for an OSC and 64 bytes for a CSI: past the bound, or when the stream
//! ends ([`TerminalSanitizer::finish`]), only the sequence's introducer is
//! dropped and the rest is printed as text. Each output stream has a
//! sanitizer of its own
//! ([`TerminalStreams`]), so a sequence one stream leaves open never holds
//! back another's output.

use std::collections::BTreeMap;
use std::io::IsTerminal;

/// ESC — the start of every escape sequence.
const ESC: u8 = 0x1B;

/// The longest incomplete OSC (`ESC ]` …) held while it waits for its
/// terminator. A longer one is not a sequence still arriving but output that
/// happens to contain `ESC ]`, such as a binary file printed by `cat`.
const MAX_HELD_OSC: usize = 4096;

/// The longest incomplete CSI (`ESC [` …) held while it waits for its final
/// byte.
const MAX_HELD_CSI: usize = 64;

/// A sanitizer holding the incomplete tail of a sequence between chunks.
#[derive(Debug, Default)]
pub struct TerminalSanitizer {
    /// An incomplete trailing escape sequence, waiting for the next chunk.
    pending: String,
}

impl TerminalSanitizer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sanitize one chunk, returning the printable text it contains.
    ///
    /// An incomplete escape sequence at the end of the chunk is held, within
    /// its bound, and prepended to the next call.
    #[must_use]
    pub fn sanitize(&mut self, chunk: &str) -> String {
        let mut text = std::mem::take(&mut self.pending);
        text.push_str(chunk);
        self.scan(&text, false)
    }

    /// End the stream, returning what was still held: an unfinished sequence
    /// is no command, so its introducer is dropped and the rest is printed
    /// as text, filtered like any other.
    #[must_use]
    pub fn finish(&mut self) -> String {
        let text = std::mem::take(&mut self.pending);
        self.scan(&text, true)
    }

    /// The printable text in `text`. An incomplete trailing sequence is held
    /// unless the stream has `ended` or the sequence has outgrown its bound;
    /// then its introducer goes and the scan goes on through the rest.
    fn scan(&mut self, text: &str, ended: bool) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        // Start of the current run of printable text.
        let mut plain = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            let b = bytes[i];
            if b == ESC {
                out.push_str(&text[plain..i]);
                match escape_len(bytes, i) {
                    Some(len) => i += len,
                    None if !ended && bytes.len() - i <= hold_limit(bytes, i) => {
                        self.pending.push_str(&text[i..]);
                        return out;
                    }
                    None => i += introducer_len(bytes, i),
                }
                plain = i;
            } else if b == b'\n' || b == b'\t' || (0x20..=0x7F).contains(&b) {
                i += 1;
            } else if b < 0x20 {
                // C0 control characters other than \n and \t.
                out.push_str(&text[plain..i]);
                i += 1;
                plain = i;
            } else {
                // Continuation bytes of a multi-byte character can fall in
                // 0x80..=0x9F, so strip C1 controls at the character level.
                let c = text[i..].chars().next().expect("valid utf-8");
                if ('\u{80}'..='\u{9F}').contains(&c) {
                    out.push_str(&text[plain..i]);
                    i += c.len_utf8();
                    plain = i;
                } else {
                    i += c.len_utf8();
                }
            }
        }
        out.push_str(&text[plain..]);
        out
    }
}

/// How long the incomplete sequence starting at `bytes[start] == ESC` may
/// grow while it waits for the rest of it.
fn hold_limit(bytes: &[u8], start: usize) -> usize {
    if bytes.get(start + 1) == Some(&b']') {
        MAX_HELD_OSC
    } else {
        // A CSI, or an ESC still waiting for its second or third byte.
        MAX_HELD_CSI
    }
}

/// The introducer of the incomplete sequence at `bytes[start] == ESC`: the
/// ESC and the byte after it, if there is one.
fn introducer_len(bytes: &[u8], start: usize) -> usize {
    (bytes.len() - start).min(2)
}

/// The length of the escape sequence starting at `bytes[start] == ESC`, or
/// `None` when it runs to the end of the buffer and may complete in the next
/// chunk.
fn escape_len(bytes: &[u8], start: usize) -> Option<usize> {
    match bytes.get(start + 1).copied() {
        None => None,
        Some(b'[') => csi_len(bytes, start),
        Some(b']') => osc_len(bytes, start),
        Some(b) if (0x20..=0x2F).contains(&b) => {
            // ESC <intermediate> <final>: charset selection and the like.
            match bytes.get(start + 2).copied() {
                None => None,
                Some(f) if (0x30..=0x7E).contains(&f) => Some(3),
                // Not a final byte: the intermediate stands alone, as a
                // two-byte sequence the reference regex would match.
                Some(_) => Some(2),
            }
        }
        Some(b) if (0x30..=0x7E).contains(&b) => Some(2),
        // A byte a sequence cannot start with: a lone ESC, stripped on its
        // own (the byte itself is handled by the main scan).
        Some(_) => Some(1),
    }
}

/// `ESC [ <params> <intermediates> <final>` — colours, cursor moves, clear,
/// and private modes such as `ESC[?1049h` (alt screen).
fn csi_len(bytes: &[u8], start: usize) -> Option<usize> {
    let mut j = start + 2;
    while bytes.get(j).is_some_and(|b| (0x30..=0x3F).contains(b)) {
        j += 1;
    }
    while bytes.get(j).is_some_and(|b| (0x20..=0x2F).contains(b)) {
        j += 1;
    }
    match bytes.get(j).copied() {
        Some(b) if (0x40..=0x7E).contains(&b) => Some(j + 1 - start),
        None => None,
        // Malformed: the reference falls back to its two-byte ESC+`[`
        // pattern and re-scans the rest.
        Some(_) => Some(2),
    }
}

/// ESC ] … BEL or ESC ] … ESC \ — window titles, OSC 8 hyperlinks, OSC 52
/// clipboard writes.
fn osc_len(bytes: &[u8], start: usize) -> Option<usize> {
    let mut j = start + 2;
    while let Some(&b) = bytes.get(j) {
        if b == 0x07 {
            return Some(j + 1 - start);
        }
        if b == ESC && bytes.get(j + 1) == Some(&b'\\') {
            return Some(j + 2 - start);
        }
        j += 1;
    }
    None
}

/// Sanitize a complete string — a line, or a block of them — carrying no
/// state in or out: a sequence it leaves open is treated as at the end of a
/// stream.
#[must_use]
pub fn sanitize_line(line: &str) -> String {
    TerminalSanitizer::new().scan(line, true)
}

/// `text` for this process's stdout: sanitized when stdout is a terminal,
/// byte-exact otherwise.
#[must_use]
pub fn for_stdout(text: &str) -> String {
    sanitize_line_if(std::io::stdout().is_terminal(), text)
}

/// `text` for this process's stderr: sanitized when stderr is a terminal,
/// byte-exact otherwise.
#[must_use]
pub fn for_stderr(text: &str) -> String {
    sanitize_line_if(std::io::stderr().is_terminal(), text)
}

fn sanitize_line_if(terminal: bool, line: &str) -> String {
    if terminal {
        sanitize_line(line)
    } else {
        line.to_string()
    }
}

fn sanitize_if(terminal: bool, sanitizer: &mut TerminalSanitizer, chunk: &str) -> String {
    if terminal {
        sanitizer.sanitize(chunk)
    } else {
        chunk.to_string()
    }
}

/// One printer's sanitizers, one per output stream: the model's text, its
/// thinking, and each tool call's stdout and stderr. A sequence one stream
/// leaves unfinished then never holds back another's output. What goes to a
/// terminal is sanitized; piped output stays byte-exact.
#[derive(Debug, Default)]
pub struct TerminalStreams {
    stdout_term: bool,
    stderr_term: bool,
    text: TerminalSanitizer,
    think: TerminalSanitizer,
    /// Each running tool call's streams, by tool-call id: created with its
    /// first output, finished with its result.
    tools: BTreeMap<String, ToolStreams>,
}

/// A tool call's stdout and stderr sanitizers.
#[derive(Debug, Default)]
struct ToolStreams {
    stdout: TerminalSanitizer,
    stderr: TerminalSanitizer,
}

impl TerminalStreams {
    /// For this process's stdout and stderr, each sanitized when it is a
    /// terminal.
    #[must_use]
    pub fn for_stdio() -> Self {
        Self::new(
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
        )
    }

    /// Sanitize what goes to stdout when `stdout_term`, and what goes to
    /// stderr when `stderr_term`.
    #[must_use]
    pub fn new(stdout_term: bool, stderr_term: bool) -> Self {
        Self {
            stdout_term,
            stderr_term,
            ..Self::default()
        }
    }

    /// A delta of the model's answer, for stdout.
    #[must_use]
    pub fn text(&mut self, delta: &str) -> String {
        sanitize_if(self.stdout_term, &mut self.text, delta)
    }

    /// A delta of the model's thinking, for stdout.
    #[must_use]
    pub fn think(&mut self, delta: &str) -> String {
        sanitize_if(self.stdout_term, &mut self.think, delta)
    }

    /// End a run of the model's output, returning what its text and thinking
    /// still held, for stdout.
    #[must_use]
    pub fn finish_model(&mut self) -> String {
        let mut tail = self.text.finish();
        tail.push_str(&self.think.finish());
        tail
    }

    /// A chunk of tool call `id`'s stdout, for stdout.
    #[must_use]
    pub fn tool_stdout(&mut self, id: &str, chunk: &str) -> String {
        if !self.stdout_term {
            return chunk.to_string();
        }
        self.tool(id).stdout.sanitize(chunk)
    }

    /// A chunk of tool call `id`'s stderr, for stderr.
    #[must_use]
    pub fn tool_stderr(&mut self, id: &str, chunk: &str) -> String {
        if !self.stderr_term {
            return chunk.to_string();
        }
        self.tool(id).stderr.sanitize(chunk)
    }

    /// End tool call `id`'s output, returning what its stdout and stderr
    /// still held.
    #[must_use]
    pub fn finish_tool(&mut self, id: &str) -> (String, String) {
        self.tools.remove(id).map_or_else(
            || (String::new(), String::new()),
            |mut tool| (tool.stdout.finish(), tool.stderr.finish()),
        )
    }

    /// A complete line for stdout, sanitized on its own.
    #[must_use]
    pub fn stdout_line(&self, line: &str) -> String {
        sanitize_line_if(self.stdout_term, line)
    }

    /// A complete line for stderr, sanitized on its own.
    #[must_use]
    pub fn stderr_line(&self, line: &str) -> String {
        sanitize_line_if(self.stderr_term, line)
    }

    /// End every stream, returning what they still held, for stdout and for
    /// stderr: the model's text and thinking, and every tool call that never
    /// reported its result.
    #[must_use]
    pub fn finish(&mut self) -> (String, String) {
        let mut out = self.finish_model();
        let mut err = String::new();
        for mut tool in std::mem::take(&mut self.tools).into_values() {
            out.push_str(&tool.stdout.finish());
            err.push_str(&tool.stderr.finish());
        }
        (out, err)
    }

    fn tool(&mut self, id: &str) -> &mut ToolStreams {
        self.tools.entry(id.to_string()).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_untouched() {
        let mut san = TerminalSanitizer::new();
        assert_eq!(san.sanitize("hello\nworld\t!"), "hello\nworld\t!");
    }

    #[test]
    fn strips_sgr_colours() {
        let mut san = TerminalSanitizer::new();
        assert_eq!(san.sanitize("\u{1B}[31mred\u{1B}[0m"), "red");
        assert_eq!(
            san.sanitize("\u{1B}[1;32mbold green\u{1B}[0m"),
            "bold green"
        );
    }

    #[test]
    fn strips_csi_private_modes_and_cursor_moves() {
        let mut san = TerminalSanitizer::new();
        assert_eq!(san.sanitize("\u{1B}[?1049h\u{1B}[?25l"), "");
        assert_eq!(san.sanitize("before\u{1B}[?2004hafter"), "beforeafter");
        assert_eq!(san.sanitize("\u{1B}[2J\u{1B}[Hhello"), "hello");
        assert_eq!(san.sanitize("\u{1B}[10;5Hhi"), "hi");
    }

    #[test]
    fn strips_osc_window_titles_and_clipboard_writes() {
        let mut san = TerminalSanitizer::new();
        // A clipboard write leaves the terminal untouched.
        assert_eq!(san.sanitize("\u{1B}]52;c;aGVsbG8=\u{7}text"), "text");
        assert_eq!(san.sanitize("\u{1B}]0;my title\u{7}text"), "text");
    }

    #[test]
    fn strips_osc8_hyperlinks_but_keeps_the_link_text() {
        let mut san = TerminalSanitizer::new();
        let link = "\u{1B}]8;;https://example.com\u{1B}\\click here\u{1B}]8;;\u{1B}\\";
        assert_eq!(san.sanitize(link), "click here");
    }

    #[test]
    fn strips_carriage_returns_bells_and_nuls() {
        let mut san = TerminalSanitizer::new();
        assert_eq!(san.sanitize("frame1\rframe2\rframe3"), "frame1frame2frame3");
        assert_eq!(san.sanitize("line\r\nnext"), "line\nnext");
        assert_eq!(san.sanitize("a\u{8}b\u{7}c\u{0}d"), "abcd");
    }

    #[test]
    fn strips_single_byte_escape_commands() {
        let mut san = TerminalSanitizer::new();
        assert_eq!(san.sanitize("\u{1B}c\u{1B}7\u{1B}8text"), "text");
        assert_eq!(san.sanitize("\u{1B}(Btext"), "text");
    }

    #[test]
    fn keeps_newlines_and_tabs() {
        let mut san = TerminalSanitizer::new();
        assert_eq!(san.sanitize("a\nb\tc"), "a\nb\tc");
        // A malformed CSI drops its ESC and `[`, keeping what follows.
        assert_eq!(san.sanitize("\u{1B}[\nrest"), "\nrest");
    }

    #[test]
    fn strips_c0_and_c1_controls() {
        let mut san = TerminalSanitizer::new();
        assert_eq!(san.sanitize("a\u{0}b\u{1F}c"), "abc");
        // C1 controls go, printable multi-byte characters stay.
        assert_eq!(san.sanitize("a\u{85}b\u{9F}c"), "abc");
        assert_eq!(san.sanitize("héllo→"), "héllo→");
    }

    #[test]
    fn holds_a_sequence_split_across_chunks() {
        let mut san = TerminalSanitizer::new();
        assert_eq!(san.sanitize("ev\u{1B}]8;;https://exa"), "ev");
        assert_eq!(san.sanitize("mple.com\u{1B}\\click\u{1B}[31m"), "click");
        assert_eq!(san.sanitize("red\u{1B}[0m"), "red");
        assert_eq!(san.sanitize("\u{1B}[10;"), "");
        assert_eq!(san.sanitize("5Hhi"), "hi");
        assert_eq!(san.finish(), "", "no residue between chunks");
    }

    #[test]
    fn a_split_osc52_write_reaches_the_terminal_as_nothing() {
        let mut san = TerminalSanitizer::new();
        assert_eq!(san.sanitize("\u{1B}]52;c;"), "");
        assert_eq!(san.sanitize("aGVsbG8=\u{7}"), "");
        assert_eq!(san.sanitize("done"), "done");
    }

    /// What an unfinished sequence held is output, not a command: the end of
    /// the stream prints it without its introducer.
    #[test]
    fn finish_prints_an_unterminated_sequence_as_text() {
        let mut san = TerminalSanitizer::new();
        assert_eq!(san.sanitize("text\u{1B}]0;never terminated"), "text");
        assert_eq!(san.finish(), "0;never terminated");
        assert_eq!(san.sanitize("after"), "after");
    }

    /// An OSC that never terminates — a binary file on stdout — is held only
    /// up to its bound; past it, the text that follows is printed.
    #[test]
    fn an_unterminated_osc_releases_the_text_after_it() {
        let mut san = TerminalSanitizer::new();
        assert_eq!(san.sanitize("\u{1B}]0;title"), "");
        let body = "x".repeat(MAX_HELD_OSC);
        assert_eq!(san.sanitize(&body), format!("0;title{body}"));
        assert_eq!(san.sanitize("\u{7}after"), "after");
        assert_eq!(san.finish(), "");

        // A terminated OSC is a command, however long: a large clipboard
        // write is stripped whole.
        let write = format!("\u{1B}]52;c;{}\u{7}after", "A".repeat(2 * MAX_HELD_OSC));
        assert_eq!(san.sanitize(&write), "after");

        // In one chunk too, with a control byte filtered from the text.
        let long = format!("\u{1B}]{}\u{8}tail", "y".repeat(MAX_HELD_OSC + 1));
        assert_eq!(
            san.sanitize(&long),
            format!("{}tail", "y".repeat(MAX_HELD_OSC + 1))
        );
    }

    /// An OSC within its bound still waits for its terminator, so a long
    /// hyperlink split across chunks is stripped whole.
    #[test]
    fn an_osc_within_its_bound_is_still_held() {
        let mut san = TerminalSanitizer::new();
        let url = "u".repeat(MAX_HELD_OSC - 16);
        assert_eq!(san.sanitize(&format!("\u{1B}]8;;{url}")), "");
        assert_eq!(san.sanitize("\u{1B}\\link"), "link");
    }

    /// A CSI is held for at most 64 bytes before its parameters are printed.
    #[test]
    fn an_unterminated_csi_is_released_past_its_bound() {
        let mut san = TerminalSanitizer::new();
        let params = "1;".repeat(MAX_HELD_CSI / 2);
        assert_eq!(san.sanitize(&format!("\u{1B}[{params}")), params);
        assert_eq!(san.sanitize("\u{1B}[1;3"), "");
        assert_eq!(san.sanitize("1mred"), "red");
    }

    #[test]
    fn a_lone_esc_at_the_end_of_a_stream_is_dropped() {
        let mut san = TerminalSanitizer::new();
        assert_eq!(san.sanitize("end\u{1B}"), "end");
        assert_eq!(san.finish(), "");
        assert_eq!(san.sanitize("\u{1B}("), "");
        assert_eq!(san.finish(), "");
    }

    #[test]
    fn a_line_is_sanitized_without_state() {
        assert_eq!(
            sanitize_line("[tool-call] f(\u{1B}[31mred\u{1B}]0;open)"),
            "[tool-call] f(red0;open)"
        );
        assert_eq!(sanitize_line("next"), "next");
    }

    /// A sequence one tool leaves open neither swallows another tool's
    /// output nor the model's text and tool lines; the tool's own result
    /// releases it.
    #[test]
    fn a_sequence_held_in_one_stream_never_affects_another() {
        let mut streams = TerminalStreams::new(true, true);
        assert_eq!(streams.tool_stdout("a", "out-a \u{1B}]0;open"), "out-a ");
        assert_eq!(streams.tool_stdout("b", "out-b\n"), "out-b\n");
        assert_eq!(streams.tool_stderr("a", "err-a\n"), "err-a\n");
        assert_eq!(streams.text("answer"), "answer");
        assert_eq!(
            streams.stdout_line("[tool] [b] Completed"),
            "[tool] [b] Completed"
        );
        assert_eq!(streams.finish_tool("b"), (String::new(), String::new()));
        assert_eq!(
            streams.finish_tool("a"),
            ("0;open".to_string(), String::new())
        );
    }

    #[test]
    fn thinking_is_sanitized_apart_from_the_answer() {
        let mut streams = TerminalStreams::new(true, true);
        assert_eq!(streams.think("plan \u{1B}[3"), "plan ");
        assert_eq!(streams.text("\u{1B}[1mbold"), "bold");
        assert_eq!(streams.think("1mmore"), "more");
        assert_eq!(streams.finish_model(), "");
    }

    #[test]
    fn finish_returns_what_every_stream_held() {
        let mut streams = TerminalStreams::new(true, true);
        let _ = streams.text("\u{1B}]0;text");
        let _ = streams.think("\u{1B}]0;think");
        let _ = streams.tool_stdout("t1", "\u{1B}]0;out");
        let _ = streams.tool_stderr("t1", "\u{1B}]0;err");
        assert_eq!(
            streams.finish(),
            ("0;text0;think0;out".to_string(), "0;err".to_string())
        );
        assert_eq!(streams.finish(), (String::new(), String::new()));
    }

    #[test]
    fn piped_output_passes_through_byte_exact() {
        let chunk = "raw\u{1B}[31m\r\n\u{1B}]52;c;x\u{7}bytes\u{1B}]0;open";
        let mut piped = TerminalStreams::new(false, false);
        assert_eq!(piped.text(chunk), chunk);
        assert_eq!(piped.think(chunk), chunk);
        assert_eq!(piped.tool_stdout("t", chunk), chunk);
        assert_eq!(piped.tool_stderr("t", chunk), chunk);
        assert_eq!(piped.stdout_line(chunk), chunk);
        assert_eq!(piped.stderr_line(chunk), chunk);
        assert_eq!(piped.finish(), (String::new(), String::new()));
        // While a terminal gets the stripped form.
        let mut terminal = TerminalStreams::new(true, false);
        assert_eq!(terminal.tool_stdout("t", chunk), "raw\nbytes");
        assert_eq!(terminal.tool_stderr("t", chunk), chunk, "stderr is piped");
        assert_eq!(
            terminal.finish_tool("t"),
            ("0;open".to_string(), String::new())
        );
    }

    #[test]
    fn cleans_a_realistic_tui_burst() {
        let mut san = TerminalSanitizer::new();
        let messy = "\u{1B}[?1049h\u{1B}[?25l\u{1B}[2J\u{1B}[H\u{1B}[1m\u{1B}[32mVITE\u{1B}[0m \
                     ready in 120ms\r\n\u{1B}]0;dev server\u{7}  Local: http://localhost:5173/";
        let result = san.sanitize(messy);
        assert!(!result.contains('\u{1B}'));
        assert!(!result.contains('\r'));
        assert!(result.contains("VITE ready in 120ms"));
        assert!(result.contains("Local: http://localhost:5173/"));
    }
}
