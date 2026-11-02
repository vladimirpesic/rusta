//! `read_file`: bounded, resumable reads of text files.
//!
//! A file can be far larger than a context window: a minified bundle is
//! megabytes on one line, a log gigabytes. Every read is therefore held to a
//! character budget (`max_chars`), prefers whole lines, and ends with the exact
//! `offset` / `column_offset` to continue from when the budget cut it short. A
//! negative `offset` reads from the end of the file.
//!
//! Files are streamed line by line, so memory stays bounded however large the
//! file is. UTF-16 files are converted for display, bytes that are not valid
//! UTF-8 are shown as U+FFFD with a warning instead of failing the read, and
//! files that look binary are refused with a pointer to the right tool.
//!
//! Each line comes back numbered: its 1-based number, a tab, then its text as
//! stored (CRLF shown as LF). The numbers let a model cite a line and page by
//! it exactly; they are not part of the file, so `edit_file` takes text without
//! them (and drops them when a model copies them along). They count toward
//! `max_chars`. Notes about the read (the range shown, where to continue,
//! decoding warnings) follow the lines in brackets after a blank line.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::{Value, json};

use super::ReadMediaTool;
use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};
use crate::stale_guard::{DIGEST_SIZE_LIMIT, FileStamp};
use crate::tool_output::ToolOutputPolicy;

/// Characters one call returns when it does not set `max_chars`. The
/// configured ceiling
/// ([`ToolOutputPolicy::read_max_chars`]) lowers it when set below this.
pub const DEFAULT_READ_CHARS: usize = 100_000;

/// Leading bytes inspected to tell text from binary and to spot UTF-16.
const SNIFF_BYTES: usize = 512;

/// Share of control characters in the sniffed bytes above which a file is
/// treated as binary.
const BINARY_CONTROL_SHARE: f64 = 0.3;

/// Largest UTF-16 file converted for display; the conversion happens in
/// memory.
const TRANSCODE_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// The most of one line held in memory. A longer line is still counted, but
/// only its first part can be shown.
const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// Read buffer for files streamed from disk.
const STREAM_BUFFER_BYTES: usize = 64 * 1024;

pub struct ReadFileTool {
    tool: Tool,
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "read_file",
                "Read a text file. Each line comes back as `<line number><TAB><text>`: its \
                 1-based number, a tab, then its text exactly as stored (CRLF shown as LF). \
                 The number and tab are not part of the file: leave them out of edit_file's \
                 old_string and new_string. Output is limited to max_chars characters \
                 (default 100000), line numbers included; a read cut short ends with the exact \
                 offset (and column_offset) to continue from. Page large files with \
                 offset/limit; a negative offset reads from the end (-100 = the last 100 \
                 lines). UTF-16 files are converted, and bytes that are not valid UTF-8 are \
                 shown as U+FFFD with a warning. Binary files are refused: use read_media for \
                 images, video and audio.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute or relative file path" },
                        "offset": {
                            "type": "integer",
                            "description": "1-based line to start from. A negative value reads from the end of the file: -100 reads the last 100 lines. Defaults to 1."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines to read. Omit (or 0) to read toward the end of the file; max_chars still applies."
                        },
                        "column_offset": {
                            "type": "integer",
                            "description": "0-based character position in the starting line's text (its line number not counted), to resume a line that was cut short. Only with a positive offset; copy it from the previous result's continuation note."
                        },
                        "max_chars": {
                            "type": "integer",
                            "description": "Maximum characters to return, line numbers included. Defaults to 100000; values above the configured maximum (500000 unless changed) are capped."
                        }
                    },
                    "required": ["path"]
                }),
            ),
        }
    }
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for ReadFileTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, input: &Value) -> ToolAccesses {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_default();
        ToolAccesses::read_file(path)
    }

    fn approval_rule(&self, _input: &Value) -> &str {
        "silent"
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let raw_path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if raw_path.trim().is_empty() {
            return ExecutableToolResult::error("path is required");
        }
        let path = match ctx.resolve_readable_path(Path::new(raw_path)) {
            Ok(p) => p,
            Err(e) => return ExecutableToolResult::error(e),
        };
        let request = match ReadRequest::from_input(&input, &ctx.tool_output) {
            Ok(request) => request,
            Err(e) => return ExecutableToolResult::error(e),
        };

        let task_path = path.clone();
        let read = tokio::task::spawn_blocking(move || read_text_file(&task_path, &request)).await;
        match read {
            Ok(Ok(read)) => {
                // Stamp from the bytes just read rather than re-reading: it is
                // free here, and records exactly what the model is about to be
                // shown. The record is made even for a partial read
                // (`offset`/`limit`) — the guard answers "has this file moved
                // since you looked", not "have you seen every line of it".
                ctx.stale_guard.lock().unwrap().record(path, read.stamp);
                ExecutableToolResult::text(read.text)
            }
            Ok(Err(e)) => ExecutableToolResult::error(e.message(&path)),
            Err(e) => {
                ExecutableToolResult::error(format!("Failed to read {}: {e}", path.display()))
            }
        }
    }
}

/// One call's arguments, validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadRequest {
    start: Start,
    /// Most lines to return; `None` reads toward the end of the file.
    limit: Option<usize>,
    /// Character budget for the numbered lines.
    budget: usize,
    /// The `max_chars` asked for, when the configured ceiling lowered it.
    capped_from: Option<usize>,
}

/// Where a read starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// From 1-based `line`, skipping its first `column` characters.
    Forward { line: usize, column: usize },
    /// The last `lines` lines of the file.
    Tail { lines: usize },
}

impl ReadRequest {
    fn from_input(input: &Value, policy: &ToolOutputPolicy) -> Result<Self, String> {
        let offset = integer_arg(input, "offset")?.unwrap_or(1);
        let column = integer_arg(input, "column_offset")?.unwrap_or(0);
        if column < 0 {
            return Err("column_offset must be 0 or more".to_string());
        }
        let start = if offset < 0 {
            if column > 0 {
                return Err(
                    "column_offset works only with a positive offset (a forward read). \
                     Start the tail read without it, or continue from the offset the previous \
                     result gave."
                        .to_string(),
                );
            }
            Start::Tail {
                lines: to_usize(offset.unsigned_abs()),
            }
        } else {
            Start::Forward {
                line: to_usize(offset.max(1).unsigned_abs()),
                column: to_usize(column.unsigned_abs()),
            }
        };

        let limit = match integer_arg(input, "limit")? {
            None | Some(0) => None,
            Some(n) if n < 0 => {
                return Err("limit must be a positive number of lines".to_string());
            }
            Some(n) => Some(to_usize(n.unsigned_abs())),
        };

        let ceiling = policy.read_max_chars;
        let default = if ceiling == 0 {
            DEFAULT_READ_CHARS
        } else {
            DEFAULT_READ_CHARS.min(ceiling)
        };
        let (budget, capped_from) = match integer_arg(input, "max_chars")? {
            None | Some(0) => (default, None),
            Some(n) if n < 0 => {
                return Err("max_chars must be a positive number of characters".to_string());
            }
            Some(n) => {
                let asked = to_usize(n.unsigned_abs());
                if ceiling != 0 && asked > ceiling {
                    (ceiling, Some(asked))
                } else {
                    (asked, None)
                }
            }
        };

        Ok(Self {
            start,
            limit,
            budget,
            capped_from,
        })
    }
}

/// An integer argument. Integral floats and numeric strings are accepted too:
/// models are not always exact about JSON types.
fn integer_arg(input: &Value, key: &str) -> Result<Option<i64>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Ok(Some(i)),
            (None, Some(f)) if f.fract() == 0.0 && f.abs() < 9.0e15 => Ok(Some(f as i64)),
            _ => Err(format!("{key} must be an integer, got {n}")),
        },
        Some(Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .map(Some)
            .map_err(|_| format!("{key} must be an integer, got {s:?}")),
        Some(other) => Err(format!("{key} must be an integer, got {other}")),
    }
}

/// `n` as a `usize`, saturating on 32-bit targets.
fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// A successful read: the text for the model, and the stamp for the stale
/// guard.
struct FileRead {
    text: String,
    stamp: FileStamp,
}

/// Why a read produced no text.
#[derive(Debug)]
enum ReadError {
    IsDirectory,
    Binary,
    Utf16TooLarge {
        encoding: Encoding,
        bytes: u64,
    },
    ColumnPastEnd {
        line: usize,
        column: usize,
        line_chars: usize,
    },
    LineTooLong {
        line: usize,
    },
    Io(io::Error),
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl ReadError {
    fn message(&self, path: &Path) -> String {
        let shown = path.display();
        match self {
            Self::Io(e) if e.kind() == io::ErrorKind::NotFound => {
                format!("File not found: {shown}")
            }
            Self::Io(e) => format!("Failed to read {shown}: {e}"),
            Self::IsDirectory => {
                format!("{shown} is a directory; use list_dir or glob to see what it contains.")
            }
            Self::Binary => match ReadMediaTool::mime_type(path) {
                Some(mime) => {
                    format!("{shown} is a media file ({mime}); read it with read_media instead.")
                }
                None => format!(
                    "{shown} looks like a binary file, so read_file cannot show it. Inspect it \
                     with bash (e.g. `file`, `xxd | head`, `strings`). If it is text in another \
                     encoding, convert it to UTF-8 first (e.g. with iconv)."
                ),
            },
            Self::Utf16TooLarge { encoding, bytes } => format!(
                "{shown} is {} text, too large to convert for display ({bytes} bytes; the limit \
                 is {TRANSCODE_MAX_BYTES}). Convert it to UTF-8 first (e.g. with iconv).",
                encoding.label()
            ),
            Self::ColumnPastEnd {
                line,
                column,
                line_chars,
            } => format!(
                "column_offset={column} is past the end of line {line}, which is {line_chars} \
                 characters long. Read the line from column 0 to see its current contents."
            ),
            Self::LineTooLong { line } => format!(
                "Line {line} of {shown} is longer than read_file can page ({} MiB). Inspect it \
                 with bash (e.g. `cut -c`, `head -c`, `jq`).",
                MAX_LINE_BYTES / (1024 * 1024)
            ),
        }
    }
}

/// Text encodings `read_file` can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
        }
    }
}

/// What a file's first bytes say about its contents.
#[derive(Debug, PartialEq, Eq)]
enum Sniffed {
    /// Text in `encoding`, after a byte-order mark `bom` bytes long.
    Text {
        encoding: Encoding,
        bom: usize,
    },
    Binary,
}

fn sniff(sample: &[u8]) -> Sniffed {
    if sample.starts_with(&[0xFE, 0xFF]) {
        return Sniffed::Text {
            encoding: Encoding::Utf16Be,
            bom: 2,
        };
    }
    if sample.starts_with(&[0xFF, 0xFE]) {
        return Sniffed::Text {
            encoding: Encoding::Utf16Le,
            bom: 2,
        };
    }
    if sample.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Sniffed::Text {
            encoding: Encoding::Utf8,
            bom: 3,
        };
    }

    let sample = &sample[..sample.len().min(SNIFF_BYTES)];
    let (mut even, mut odd) = (0usize, 0usize);
    for (i, _) in sample.iter().enumerate().filter(|(_, b)| **b == 0) {
        if i % 2 == 0 {
            even += 1;
        } else {
            odd += 1;
        }
    }
    // UTF-16 without a byte-order mark: mostly-ASCII text has a NUL in nearly
    // every other byte, all on one side. A few NULs on one side are a binary
    // header instead (a JPEG's, for one).
    let utf16_like = |nuls: usize| nuls >= 2 && nuls * 4 >= sample.len() / 2;
    match (even, odd) {
        (0, 0) => {}
        (0, nuls) if utf16_like(nuls) => {
            return Sniffed::Text {
                encoding: Encoding::Utf16Le,
                bom: 0,
            };
        }
        (nuls, 0) if utf16_like(nuls) => {
            return Sniffed::Text {
                encoding: Encoding::Utf16Be,
                bom: 0,
            };
        }
        _ => return Sniffed::Binary,
    }

    let text = String::from_utf8_lossy(sample);
    let (mut total, mut control) = (0usize, 0usize);
    for c in text.chars() {
        total += 1;
        if c.is_control() && !matches!(c, '\t' | '\n' | '\r' | '\x0c') {
            control += 1;
        }
    }
    let mostly_control = total > 0 && control as f64 > total as f64 * BINARY_CONTROL_SHARE;
    if mostly_control {
        Sniffed::Binary
    } else {
        Sniffed::Text {
            encoding: Encoding::Utf8,
            bom: 0,
        }
    }
}

/// UTF-16 `bytes` as a `String`, and whether malformed sequences had to be
/// replaced.
fn decode_utf16(bytes: &[u8], big_endian: bool) -> (String, bool) {
    let units = bytes.as_chunks::<2>().0.iter().map(|pair| {
        let pair = [pair[0], pair[1]];
        if big_endian {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    });
    let mut text = String::with_capacity(bytes.len() / 2);
    let mut lossy = false;
    for decoded in char::decode_utf16(units) {
        text.push(decoded.unwrap_or_else(|_| {
            lossy = true;
            char::REPLACEMENT_CHARACTER
        }));
    }
    if bytes.len() % 2 == 1 {
        // A dangling half of a code unit.
        text.push(char::REPLACEMENT_CHARACTER);
        lossy = true;
    }
    (text, lossy)
}

/// One line's UTF-8 bytes as text, and whether invalid bytes were replaced.
fn decode_line(bytes: &[u8]) -> (Cow<'_, str>, bool) {
    let text = String::from_utf8_lossy(bytes);
    let lossy = matches!(text, Cow::Owned(_));
    (text, lossy)
}

/// Characters in UTF-8 `bytes`: every byte that does not continue a sequence.
fn count_chars(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b & 0xC0 != 0x80).count()
}

/// Length of `bytes` without a trailing UTF-8 sequence that was cut short.
fn complete_prefix_len(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for i in (len.saturating_sub(4)..len).rev() {
        let b = bytes[i];
        if b & 0xC0 == 0x80 {
            continue;
        }
        let needed = match b {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if i + needed > len { i } else { len };
    }
    len
}

/// Characters of the `N<TAB>` prefix that numbers line `number`.
fn number_width(number: usize) -> usize {
    number.checked_ilog10().map_or(1, |log| log as usize + 1) + 1
}

/// The characters of `text` in `[from, to)`.
fn char_slice(text: &str, from: usize, to: usize) -> &str {
    let mut boundaries = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()));
    let start = boundaries.nth(from).unwrap_or(text.len());
    let end = if to > from {
        boundaries.nth(to - from - 1).unwrap_or(text.len())
    } else {
        start
    };
    &text[start..end]
}

/// A file's text, ready to be read line by line — once, or twice for a tail
/// read that stops short of the end.
enum Source {
    /// Contents in memory as UTF-8, starting at byte `start`.
    Memory { bytes: Vec<u8>, start: usize },
    /// A file streamed from disk, starting at byte `start`.
    Disk { file: File, start: u64 },
}

impl Source {
    /// A reader over the text from its beginning.
    fn reader(&mut self) -> io::Result<Box<dyn BufRead + '_>> {
        match self {
            Self::Memory { bytes, start } => Ok(Box::new(&bytes[(*start).min(bytes.len())..])),
            Self::Disk { file, start } => {
                file.seek(SeekFrom::Start(*start))?;
                Ok(Box::new(BufReader::with_capacity(
                    STREAM_BUFFER_BYTES,
                    &*file,
                )))
            }
        }
    }
}

/// Bookkeeping for one line from [`read_line`].
#[derive(Debug, Default)]
struct LineInfo {
    /// Characters of the line past the part kept (non-zero only when
    /// `clipped`).
    dropped_chars: usize,
    /// The line was longer than the cap ([`MAX_LINE_BYTES`]); only its start
    /// was kept.
    clipped: bool,
    /// The line holds a NUL byte.
    has_nul: bool,
}

/// Reads the next line, without its terminator. With `keep`, its bytes go to
/// `buf` (cleared first); otherwise the line is only scanned past.
///
/// A line ends at `\n`, and a `\r` right before it is dropped, as with
/// [`str::lines`]. Returns `None` at the end of the input.
fn read_line(
    reader: &mut dyn BufRead,
    buf: &mut Vec<u8>,
    keep: bool,
) -> io::Result<Option<LineInfo>> {
    read_line_capped(reader, buf, keep, MAX_LINE_BYTES)
}

/// [`read_line`], keeping at most `cap` bytes of the line.
fn read_line_capped(
    reader: &mut dyn BufRead,
    buf: &mut Vec<u8>,
    keep: bool,
    cap: usize,
) -> io::Result<Option<LineInfo>> {
    buf.clear();
    let mut info = LineInfo::default();
    let mut any = false;
    let mut terminated = false;
    let mut last = None;
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            break;
        }
        any = true;
        let (chunk, consumed) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                terminated = true;
                (&available[..i], i + 1)
            }
            None => (available, available.len()),
        };
        info.has_nul |= chunk.contains(&0);
        if let Some(&b) = chunk.last() {
            last = Some(b);
        }
        if keep {
            let room = cap - buf.len();
            if chunk.len() <= room {
                buf.extend_from_slice(chunk);
            } else {
                buf.extend_from_slice(&chunk[..room]);
                info.dropped_chars += count_chars(&chunk[room..]);
                info.clipped = true;
            }
        }
        reader.consume(consumed);
        if terminated {
            break;
        }
    }
    if !any {
        return Ok(None);
    }
    if terminated && last == Some(b'\r') {
        // Once a line is clipped, everything after the cut — this `\r`
        // included — was counted rather than kept.
        if info.clipped {
            info.dropped_chars -= 1;
        } else if keep {
            buf.pop();
        }
    }
    if info.clipped {
        // Keep whole characters only; a split one goes with the dropped part.
        let whole = complete_prefix_len(buf);
        info.dropped_chars += count_chars(&buf[whole..]);
        buf.truncate(whole);
    }
    Ok(Some(info))
}

/// The lines a read returns, and where it stopped.
#[derive(Debug, Default)]
struct Page {
    lines: Vec<String>,
    /// 1-based number of `lines[0]`.
    first_line: usize,
    /// Characters of the first line skipped (`column_offset`).
    first_column: usize,
    total_lines: usize,
    cut: Option<Cut>,
    /// Invalid bytes in the text shown were replaced.
    lossy: bool,
    /// A tail read whose newest line alone overran the budget, shown from the
    /// start of its range instead.
    tail_fallback: bool,
}

/// How the budget ended a read early.
#[derive(Debug, PartialEq, Eq)]
enum Cut {
    /// A forward read stopped before line `next_line`.
    BeforeLine {
        next_line: usize,
        /// Lines of the requested `limit` still unread.
        remaining: Option<usize>,
    },
    /// A forward read stopped inside `line`, after character `end_column`.
    InLine {
        line: usize,
        end_column: usize,
        line_chars: usize,
        remaining: Option<usize>,
        /// Whether `column_offset` can continue the line; not past
        /// [`MAX_LINE_BYTES`].
        resumable: bool,
    },
    /// A tail read left out lines `range_start` up to the first line shown.
    TailHead { range_start: usize },
}

fn read_forward(
    source: &mut Source,
    line: usize,
    column: usize,
    limit: Option<usize>,
    budget: usize,
) -> Result<Page, ReadError> {
    let mut reader = source.reader()?;
    let mut buf = Vec::new();
    let mut page = Page {
        first_line: line,
        first_column: column,
        ..Page::default()
    };
    let mut used = 0usize;
    let mut closed = false;
    let mut number = 0usize;
    loop {
        let collecting = !closed && number + 1 >= line;
        let Some(info) = read_line(&mut *reader, &mut buf, collecting)? else {
            break;
        };
        number += 1;
        if info.has_nul {
            return Err(ReadError::Binary);
        }
        if !collecting {
            continue;
        }
        if limit.is_some_and(|limit| page.lines.len() >= limit) {
            closed = true;
            continue;
        }

        let (text, lossy) = decode_line(&buf);
        let kept_chars = text.chars().count();
        let line_chars = kept_chars + info.dropped_chars;
        let from = if number == line { column } else { 0 };
        if from > line_chars {
            return Err(ReadError::ColumnPastEnd {
                line: number,
                column: from,
                line_chars,
            });
        }
        if info.clipped && from >= kept_chars {
            return Err(ReadError::LineTooLong { line: number });
        }
        let end = if info.clipped { kept_chars } else { line_chars };
        let wanted = end - from;
        // A line costs its number, and the newline before it after the first,
        // besides its text.
        let width = number_width(number);
        let cost = usize::from(!page.lines.is_empty()) + width + wanted;
        if !info.clipped && used + cost <= budget {
            page.lines.push(char_slice(&text, from, end).to_owned());
            page.lossy |= lossy;
            used += cost;
            continue;
        }

        if page.lines.is_empty() {
            // Not even this line fits: show as much of it as the budget leaves
            // beside its number, and at least one character, so that every
            // read moves on.
            let taken = wanted.min(budget.saturating_sub(width).max(1));
            page.lines
                .push(char_slice(&text, from, from + taken).to_owned());
            page.lossy |= lossy;
            if taken == wanted && !info.clipped {
                // The budget had no room beside the number, and the rest of
                // the line was that one character or none: it is whole.
                used = budget;
                continue;
            }
            closed = true;
            page.cut = Some(Cut::InLine {
                line: number,
                end_column: from + taken,
                line_chars,
                // A partial line still counts against the limit until its end
                // has been read.
                remaining: limit,
                resumable: !(info.clipped && from + taken >= kept_chars),
            });
        } else {
            closed = true;
            page.cut = Some(Cut::BeforeLine {
                next_line: number,
                remaining: limit.map(|limit| limit - page.lines.len()),
            });
        }
    }
    page.total_lines = number;
    Ok(page)
}

/// The newest lines of a tail read that fit the budget, oldest first.
struct TailWindow {
    lines: VecDeque<TailLine>,
    /// Characters held, separators included.
    used: usize,
    max_lines: usize,
    budget: usize,
}

struct TailLine {
    number: usize,
    text: String,
    /// Characters it takes on the page: its number and its text.
    cost: usize,
    lossy: bool,
}

impl TailWindow {
    fn new(max_lines: usize, budget: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            used: 0,
            max_lines,
            budget,
        }
    }

    /// Adds line `number`, dropping the oldest lines to stay within the limits.
    fn push(&mut self, number: usize, bytes: &[u8], info: &LineInfo) {
        let (text, lossy) = decode_line(bytes);
        let cost = number_width(number) + text.chars().count();
        if info.clipped || cost > self.budget {
            // This line can never be shown whole, so neither can anything
            // before it: what a tail read shows must run up to its end.
            self.lines.clear();
            self.used = 0;
            return;
        }
        self.used += usize::from(!self.lines.is_empty()) + cost;
        self.lines.push_back(TailLine {
            number,
            text: text.into_owned(),
            cost,
            lossy,
        });
        while self.lines.len() > self.max_lines || self.used > self.budget {
            let Some(dropped) = self.lines.pop_front() else {
                break;
            };
            self.used -= dropped.cost + usize::from(!self.lines.is_empty());
        }
    }
}

fn read_tail(
    source: &mut Source,
    count: usize,
    limit: Option<usize>,
    budget: usize,
) -> Result<Page, ReadError> {
    let mut window = TailWindow::new(count, budget);
    let mut buf = Vec::new();
    let (total, range_start, range_end) = match limit {
        // The range stops short of the end of the file: count the lines
        // first, then collect the range.
        Some(limit) if limit < count => {
            let total = count_lines(source)?;
            let range_start = total.saturating_sub(count) + 1;
            let range_end = (range_start + limit - 1).min(total);
            let mut reader = source.reader()?;
            let mut number = 0usize;
            while number < range_end {
                let keep = number + 1 >= range_start;
                let Some(info) = read_line(&mut *reader, &mut buf, keep)? else {
                    break;
                };
                number += 1;
                if info.has_nul {
                    return Err(ReadError::Binary);
                }
                if keep {
                    window.push(number, &buf, &info);
                }
            }
            (total, range_start, range_end)
        }
        // The range runs to the end: one pass keeps the newest lines as it
        // goes.
        _ => {
            let mut reader = source.reader()?;
            let mut number = 0usize;
            while let Some(info) = read_line(&mut *reader, &mut buf, true)? {
                number += 1;
                if info.has_nul {
                    return Err(ReadError::Binary);
                }
                window.push(number, &buf, &info);
            }
            (number, number.saturating_sub(count) + 1, number)
        }
    };

    if total == 0 {
        return Ok(Page {
            first_line: 1,
            ..Page::default()
        });
    }
    let Some(first) = window.lines.front().map(|line| line.number) else {
        let mut page = read_forward(
            source,
            range_start,
            0,
            Some(range_end - range_start + 1),
            budget,
        )?;
        page.tail_fallback = true;
        return Ok(page);
    };
    Ok(Page {
        first_line: first,
        first_column: 0,
        total_lines: total,
        cut: (first > range_start).then_some(Cut::TailHead { range_start }),
        lossy: window.lines.iter().any(|line| line.lossy),
        lines: window.lines.into_iter().map(|line| line.text).collect(),
        tail_fallback: false,
    })
}

/// Lines in `source`, refusing binary content on the way.
fn count_lines(source: &mut Source) -> Result<usize, ReadError> {
    let mut reader = source.reader()?;
    let mut buf = Vec::new();
    let mut lines = 0usize;
    while let Some(info) = read_line(&mut *reader, &mut buf, false)? {
        if info.has_nul {
            return Err(ReadError::Binary);
        }
        lines += 1;
    }
    Ok(lines)
}

fn read_text_file(path: &Path, request: &ReadRequest) -> Result<FileRead, ReadError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        // Windows does not open a directory as a file: "Access is denied".
        Err(_) if path.is_dir() => return Err(ReadError::IsDirectory),
        Err(e) => return Err(e.into()),
    };
    // Metadata before content: if the file changes mid-read, the stamp is the
    // older one and the stale guard asks for a fresh read before any edit.
    let metadata = file.metadata()?;
    if metadata.is_dir() {
        return Err(ReadError::IsDirectory);
    }
    let mut sample = Vec::with_capacity(SNIFF_BYTES);
    (&mut file)
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut sample)?;
    let Sniffed::Text { encoding, bom } = sniff(&sample) else {
        return Err(ReadError::Binary);
    };
    file.seek(SeekFrom::Start(0))?;

    let len = metadata.len();
    let mut transcoded_lossy = false;
    let (mut source, stamp) = match encoding {
        Encoding::Utf8 if len <= DIGEST_SIZE_LIMIT => {
            let bytes = read_all(&mut file, len)?;
            let stamp = FileStamp::from_parts(&metadata, Some(&bytes));
            (Source::Memory { bytes, start: bom }, stamp)
        }
        // Too large to digest anyway: stream it instead of holding it.
        Encoding::Utf8 => (
            Source::Disk {
                file,
                start: bom as u64,
            },
            FileStamp::from_parts(&metadata, None),
        ),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            if len > TRANSCODE_MAX_BYTES {
                return Err(ReadError::Utf16TooLarge {
                    encoding,
                    bytes: len,
                });
            }
            let bytes = read_all(&mut file, len)?;
            let stamp = FileStamp::from_parts(&metadata, Some(&bytes));
            let (text, lossy) = decode_utf16(
                &bytes[bom.min(bytes.len())..],
                encoding == Encoding::Utf16Be,
            );
            transcoded_lossy = lossy;
            (
                Source::Memory {
                    bytes: text.into_bytes(),
                    start: 0,
                },
                stamp,
            )
        }
    };

    let mut page = match request.start {
        Start::Forward { line, column } => {
            read_forward(&mut source, line, column, request.limit, request.budget)?
        }
        Start::Tail { lines } => read_tail(&mut source, lines, request.limit, request.budget)?,
    };
    page.lossy |= transcoded_lossy;
    Ok(FileRead {
        text: render(&page, request, encoding),
        stamp,
    })
}

fn read_all(file: &mut File, len: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(to_usize(len));
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The page's text followed by its notes.
fn render(page: &Page, request: &ReadRequest, encoding: Encoding) -> String {
    let mut notes = Vec::new();
    let budget = match request.capped_from {
        Some(asked) => format!(
            "{} (max_chars={asked} is above the configured maximum)",
            counted(request.budget, "char")
        ),
        None => counted(request.budget, "char"),
    };
    let last_line = page.first_line + page.lines.len().saturating_sub(1);
    let span = || match (page.lines.len(), page.first_column) {
        (1, 0) => format!("line {}", page.first_line),
        (1, column) => format!("line {} from column {column}", page.first_line),
        (_, 0) => format!("lines {}–{last_line}", page.first_line),
        (_, column) => format!(
            "lines {}–{last_line} (line {} from column {column})",
            page.first_line, page.first_line
        ),
    };
    let file_size = counted(page.total_lines, "line");
    let limit_arg = |remaining: Option<usize>| {
        remaining.map_or_else(String::new, |remaining| format!(" limit={remaining}"))
    };

    if page.tail_fallback {
        notes.push(format!(
            "[The last line of the requested range does not fit in {budget}, so the range is \
             shown from its start.]"
        ));
    }
    match page.cut {
        Some(Cut::BeforeLine {
            next_line,
            remaining,
        }) => notes.push(format!(
            "[Truncated at {budget} — showing {}; the file has {file_size}. Continue with \
             offset={next_line}{} to read more.]",
            span(),
            limit_arg(remaining)
        )),
        Some(Cut::InLine {
            line,
            end_column,
            line_chars,
            remaining,
            resumable: true,
        }) => notes.push(format!(
            "[Truncated at {budget} — showing columns {}–{end_column} of line {line}, which is \
             {line_chars} characters long; the file has {file_size}. Continue with \
             offset={line} column_offset={end_column}{} to read more.]",
            page.first_column,
            limit_arg(remaining)
        )),
        Some(Cut::InLine {
            line,
            end_column,
            line_chars,
            resumable: false,
            ..
        }) => notes.push(format!(
            "[Truncated — line {line} is {line_chars} characters long, more than read_file can \
             page; showing columns {}–{end_column}. Inspect the rest with bash (e.g. `cut -c`, \
             `head -c`, `jq`).]",
            page.first_column
        )),
        Some(Cut::TailHead { range_start }) => notes.push(format!(
            "[Truncated at {budget} — showing {}; the file has {file_size}. Lines \
             {range_start}–{} of the requested range were left out: read them with \
             offset={range_start} limit={}.]",
            span(),
            page.first_line - 1,
            page.first_line - range_start
        )),
        None if page.total_lines == 0 => notes.push("[The file is empty.]".to_string()),
        None if page.lines.is_empty() => {
            notes.push(format!("[No lines to show: the file has {file_size}.]"));
        }
        None => {
            if page.first_line > 1 || page.first_column > 0 || last_line < page.total_lines {
                notes.push(format!("[Showing {}; the file has {file_size}.]", span()));
            }
        }
    }

    if encoding != Encoding::Utf8 {
        notes.push(format!(
            "[Decoded from {}. edit_file and write_file work on UTF-8: convert the file first \
             (e.g. with iconv) before editing it.]",
            encoding.label()
        ));
    }
    if page.lossy {
        let label = match encoding {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16Le | Encoding::Utf16Be => "UTF-16",
        };
        let editing = if encoding == Encoding::Utf8 {
            " edit_file cannot change the file until it is valid UTF-8 (convert it, e.g. with \
             iconv)."
        } else {
            ""
        };
        notes.push(format!(
            "[Warning: bytes that are not valid {label} are shown as U+FFFD (\u{FFFD}), so this \
             text may differ from the file.{editing}]"
        ));
    }

    let mut text = (page.first_line..)
        .zip(&page.lines)
        .map(|(number, line)| format!("{number}\t{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    if !notes.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&notes.join("\n"));
    }
    text
}

/// `n` and `unit`, plural unless `n` is 1.
fn counted(n: usize, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path) -> ExecutableToolContext {
        ExecutableToolContext {
            cwd: dir.to_path_buf(),
            ..Default::default()
        }
    }

    async fn read(ctx: &ExecutableToolContext, args: Value) -> ExecutableToolResult {
        ReadFileTool::new().execute(args, ctx, "r").await
    }

    fn numbered_lines(n: usize) -> String {
        (1..=n)
            .map(|i| format!("line {i:04}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `body` without its line numbers, checking that they count up from
    /// `first`.
    fn unnumbered(body: &str, first: usize) -> String {
        body.split('\n')
            .zip(first..)
            .map(|(line, number)| {
                let (shown, text) = line.split_once('\t').expect("a numbered line");
                assert_eq!(shown, number.to_string(), "{body}");
                text
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Follow the continuation notes until the read is complete, returning the
    /// text reassembled from every page, without its line numbers.
    async fn read_all_pages(ctx: &ExecutableToolContext, path: &Path, max_chars: usize) -> String {
        let mut args = json!({"path": path, "max_chars": max_chars});
        let mut text = String::new();
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages < 10_000, "paging did not terminate");
            let first = args["offset"].as_u64().map_or(1, |n| n as usize);
            let result = read(ctx, args.clone()).await;
            assert!(!result.is_error, "{}", result.text_content());
            let out = result.text_content();
            let Some((body, note)) = out.rsplit_once("\n\n[Truncated at ") else {
                // The last page: drop any range note.
                text.push_str(&unnumbered(
                    out.split("\n\n[Showing").next().unwrap(),
                    first,
                ));
                return text;
            };
            text.push_str(&unnumbered(body, first));
            let offset: usize = note
                .split("offset=")
                .nth(1)
                .unwrap()
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap()
                .parse()
                .unwrap();
            let column = note.split("column_offset=").nth(1).map(|rest| {
                rest.split(|c: char| !c.is_ascii_digit())
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            });
            args = json!({"path": path, "max_chars": max_chars, "offset": offset});
            if let Some(column) = column {
                args["column_offset"] = json!(column);
            } else {
                // The next page starts a new line.
                text.push('\n');
            }
        }
    }

    #[tokio::test]
    async fn a_small_file_comes_back_numbered() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let result = read(&ctx(dir.path()), json!({"path": "a.txt"})).await;
        assert!(!result.is_error);
        assert_eq!(result.text_content(), "1\talpha\n2\tbeta");
    }

    /// Every line is numbered, an empty one too, counting from where the read
    /// starts; a tab in the text stays after the number's own.
    #[tokio::test]
    async fn every_line_is_numbered_from_where_the_read_starts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n\nkey\tvalue\n").unwrap();
        let ctx = ctx(dir.path());
        assert_eq!(
            read(&ctx, json!({"path": "a.txt"})).await.text_content(),
            "1\ta\n2\t\n3\tkey\tvalue"
        );
        assert_eq!(
            read(&ctx, json!({"path": "a.txt", "offset": 2}))
                .await
                .text_content(),
            "2\t\n3\tkey\tvalue\n\n[Showing lines 2–3; the file has 3 lines.]"
        );
    }

    #[tokio::test]
    async fn crlf_line_endings_are_shown_as_lf() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\r\ntwo\r\n").unwrap();
        let result = read(&ctx(dir.path()), json!({"path": "a.txt"})).await;
        assert_eq!(result.text_content(), "1\tone\n2\ttwo");
    }

    #[tokio::test]
    async fn offset_and_limit_select_lines_and_say_which() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered_lines(10)).unwrap();
        let result = read(
            &ctx(dir.path()),
            json!({"path": "a.txt", "offset": 3, "limit": 2}),
        )
        .await;
        assert_eq!(
            result.text_content(),
            "3\tline 0003\n4\tline 0004\n\n[Showing lines 3–4; the file has 10 lines.]"
        );
    }

    #[tokio::test]
    async fn a_negative_offset_reads_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered_lines(10)).unwrap();
        let result = read(&ctx(dir.path()), json!({"path": "a.txt", "offset": -3})).await;
        assert_eq!(
            result.text_content(),
            "8\tline 0008\n9\tline 0009\n10\tline 0010\n\n[Showing lines 8–10; the file has \
             10 lines.]"
        );

        // With a limit shorter than the tail, the range starts where the tail
        // does.
        let result = read(
            &ctx(dir.path()),
            json!({"path": "a.txt", "offset": -5, "limit": 2}),
        )
        .await;
        assert_eq!(
            result.text_content(),
            "6\tline 0006\n7\tline 0007\n\n[Showing lines 6–7; the file has 10 lines.]"
        );
    }

    #[tokio::test]
    async fn the_budget_cuts_at_a_line_boundary_with_a_continuation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered_lines(100)).unwrap();
        // A numbered line is 11 characters (`1<TAB>` and 9 of text); three
        // lines and their separators are 35, a fourth would make 47.
        let result = read(&ctx(dir.path()), json!({"path": "a.txt", "max_chars": 40})).await;
        assert_eq!(
            result.text_content(),
            "1\tline 0001\n2\tline 0002\n3\tline 0003\n\n[Truncated at 40 chars — showing \
             lines 1–3; the file has 100 lines. Continue with offset=4 to read more.]"
        );
    }

    #[tokio::test]
    async fn paging_through_the_continuations_reassembles_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let content = format!(
            "{}\n{}\n{}",
            numbered_lines(50),
            "x".repeat(333),
            numbered_lines(20)
        );
        std::fs::write(&path, &content).unwrap();
        let text = read_all_pages(&ctx(dir.path()), &path, 100).await;
        assert_eq!(text, content);
    }

    #[tokio::test]
    async fn a_long_line_is_paged_by_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("min.js");
        let line = "é".repeat(250);
        std::fs::write(&path, &line).unwrap();
        let ctx = ctx(dir.path());

        // The line's number takes two of the 100 characters.
        let first = read(&ctx, json!({"path": "min.js", "max_chars": 100})).await;
        assert_eq!(
            first.text_content(),
            format!(
                "1\t{}\n\n[Truncated at 100 chars — showing columns 0–98 of line 1, which is \
                 250 characters long; the file has 1 line. Continue with offset=1 \
                 column_offset=98 to read more.]",
                "é".repeat(98)
            )
        );
        let last = read(
            &ctx,
            json!({"path": "min.js", "max_chars": 100, "offset": 1, "column_offset": 200}),
        )
        .await;
        assert_eq!(
            last.text_content(),
            format!(
                "1\t{}\n\n[Showing line 1 from column 200; the file has 1 line.]",
                "é".repeat(50)
            )
        );
        assert_eq!(read_all_pages(&ctx, &path, 100).await, line);
    }

    /// A budget with no room beside a line's number still shows a character
    /// per read, or a whole empty line, so paging always gets to the end.
    #[tokio::test]
    async fn a_budget_too_small_for_a_number_still_moves_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let content = "ab\n\ncd";
        std::fs::write(&path, content).unwrap();
        let ctx = ctx(dir.path());
        assert_eq!(
            read(&ctx, json!({"path": "a.txt", "max_chars": 1}))
                .await
                .text_content(),
            "1\ta\n\n[Truncated at 1 char — showing columns 0–1 of line 1, which is 2 \
             characters long; the file has 3 lines. Continue with offset=1 column_offset=1 to \
             read more.]"
        );
        assert_eq!(read_all_pages(&ctx, &path, 1).await, content);
    }

    #[tokio::test]
    async fn a_limit_is_carried_into_the_continuation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered_lines(100)).unwrap();
        let result = read(
            &ctx(dir.path()),
            json!({"path": "a.txt", "limit": 10, "max_chars": 40}),
        )
        .await;
        assert!(
            result
                .text_content()
                .ends_with("Continue with offset=4 limit=7 to read more.]"),
            "{}",
            result.text_content()
        );
    }

    #[tokio::test]
    async fn a_tail_over_budget_keeps_the_newest_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered_lines(100)).unwrap();
        let result = read(
            &ctx(dir.path()),
            json!({"path": "a.txt", "offset": -10, "max_chars": 40}),
        )
        .await;
        // Lines 98 to 100 take 12, 12 and 13 characters with their numbers,
        // 39 with the separators; line 97 would make 52.
        assert_eq!(
            result.text_content(),
            "98\tline 0098\n99\tline 0099\n100\tline 0100\n\n[Truncated at 40 chars — showing \
             lines 98–100; the file has 100 lines. Lines 91–97 of the requested range were left \
             out: read them with offset=91 limit=7.]"
        );
    }

    #[tokio::test]
    async fn a_tail_whose_last_line_is_too_long_is_shown_from_its_start() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.txt"),
            format!("short\n{}", "y".repeat(500)),
        )
        .unwrap();
        let result = read(
            &ctx(dir.path()),
            json!({"path": "a.txt", "offset": -1, "max_chars": 100}),
        )
        .await;
        let text = result.text_content();
        assert!(
            text.starts_with(&format!("2\t{}\n\n[The last line", "y".repeat(98))),
            "{text}"
        );
        assert!(
            text.contains("Continue with offset=2 column_offset=98 limit=1"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn column_offset_needs_a_forward_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "abc").unwrap();
        let result = read(
            &ctx(dir.path()),
            json!({"path": "a.txt", "offset": -1, "column_offset": 2}),
        )
        .await;
        assert!(result.is_error);
        assert!(result.text_content().contains("positive offset"));
    }

    #[tokio::test]
    async fn a_column_past_the_end_of_the_line_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "abc\ndef").unwrap();
        let result = read(
            &ctx(dir.path()),
            json!({"path": "a.txt", "offset": 1, "column_offset": 4}),
        )
        .await;
        assert!(result.is_error);
        assert!(
            result
                .text_content()
                .contains("column_offset=4 is past the end of line 1, which is 3 characters")
        );
    }

    #[tokio::test]
    async fn invalid_utf8_is_shown_lossily_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("latin1.txt"), b"caf\xe9 au lait\n").unwrap();
        let result = read(&ctx(dir.path()), json!({"path": "latin1.txt"})).await;
        assert!(!result.is_error);
        let text = result.text_content();
        assert!(
            text.starts_with("1\tcaf\u{FFFD} au lait\n\n[Warning: bytes that are not valid UTF-8")
        );
    }

    #[tokio::test]
    async fn utf16_files_are_converted() {
        let dir = tempfile::tempdir().unwrap();
        let mut with_bom = vec![0xFF, 0xFE];
        with_bom.extend("héllo\r\nworld".encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(dir.path().join("le.txt"), with_bom).unwrap();
        let without_bom: Vec<u8> = "big endian text"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        std::fs::write(dir.path().join("be.txt"), without_bom).unwrap();
        let ctx = ctx(dir.path());

        let le = read(&ctx, json!({"path": "le.txt"})).await.text_content();
        assert!(
            le.starts_with("1\théllo\n2\tworld\n\n[Decoded from UTF-16 LE."),
            "{le}"
        );
        let be = read(&ctx, json!({"path": "be.txt"})).await.text_content();
        assert!(
            be.starts_with("1\tbig endian text\n\n[Decoded from UTF-16 BE."),
            "{be}"
        );
    }

    #[tokio::test]
    async fn binary_files_are_refused_with_the_right_pointer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2, 0, 0, 3, 4, 0]).unwrap();
        std::fs::write(
            dir.path().join("shot.png"),
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00",
        )
        .unwrap();
        let ctx = ctx(dir.path());

        let blob = read(&ctx, json!({"path": "blob.bin"})).await;
        assert!(blob.is_error);
        assert!(blob.text_content().contains("looks like a binary file"));
        let png = read(&ctx, json!({"path": "shot.png"})).await;
        assert!(png.is_error);
        assert!(png.text_content().contains("read it with read_media"));
    }

    #[tokio::test]
    async fn a_nul_byte_past_the_sniffed_bytes_still_refuses_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = "text\n".repeat(200).into_bytes();
        bytes.extend_from_slice(b"\0tail");
        std::fs::write(dir.path().join("mixed.dat"), bytes).unwrap();
        let result = read(&ctx(dir.path()), json!({"path": "mixed.dat", "limit": 1})).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn max_chars_is_capped_at_the_configured_maximum() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered_lines(100)).unwrap();
        let mut ctx = ctx(dir.path());
        ctx.tool_output = ToolOutputPolicy::default().with_read_max_chars(30);
        let result = read(&ctx, json!({"path": "a.txt", "max_chars": 1_000})).await;
        assert!(
            result.text_content().contains(
                "[Truncated at 30 chars (max_chars=1000 is above the configured maximum)"
            ),
            "{}",
            result.text_content()
        );
        // Without max_chars, the default is lowered to the ceiling as well.
        let result = read(&ctx, json!({"path": "a.txt"})).await;
        assert!(result.text_content().contains("[Truncated at 30 chars —"));
    }

    #[tokio::test]
    async fn empty_files_and_offsets_past_the_end_say_so() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let ctx = ctx(dir.path());
        assert_eq!(
            read(&ctx, json!({"path": "empty.txt"}))
                .await
                .text_content(),
            "[The file is empty.]"
        );
        assert_eq!(
            read(&ctx, json!({"path": "a.txt", "offset": 9}))
                .await
                .text_content(),
            "[No lines to show: the file has 2 lines.]"
        );
    }

    #[tokio::test]
    async fn directories_and_missing_files_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let ctx = ctx(dir.path());
        let sub = read(&ctx, json!({"path": "sub"})).await;
        assert!(sub.is_error);
        assert!(sub.text_content().contains("is a directory"));
        let missing = read(&ctx, json!({"path": "nope.txt"})).await;
        assert!(missing.is_error);
        assert!(missing.text_content().starts_with("File not found:"));
    }

    #[tokio::test]
    async fn integer_arguments_are_parsed_leniently() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered_lines(5)).unwrap();
        let ctx = ctx(dir.path());
        let result = read(&ctx, json!({"path": "a.txt", "offset": "2", "limit": 1.0})).await;
        assert_eq!(
            result.text_content(),
            "2\tline 0002\n\n[Showing line 2; the file has 5 lines.]"
        );
        let bad = read(&ctx, json!({"path": "a.txt", "limit": "some"})).await;
        assert!(bad.is_error);
        assert!(bad.text_content().contains("limit must be an integer"));
    }

    /// Files too large to hold for a digest are streamed from disk, and read
    /// the same way.
    #[tokio::test]
    async fn large_files_are_streamed() {
        let dir = tempfile::tempdir().unwrap();
        let line = format!("{}\n", "z".repeat(1023));
        let lines = usize::try_from(DIGEST_SIZE_LIMIT / 1024).unwrap() + 16;
        std::fs::write(dir.path().join("big.log"), line.repeat(lines)).unwrap();
        let ctx = ctx(dir.path());

        let head = read(&ctx, json!({"path": "big.log", "limit": 1})).await;
        assert_eq!(
            head.text_content(),
            format!(
                "1\t{}\n\n[Showing line 1; the file has {lines} lines.]",
                "z".repeat(1023)
            )
        );
        let tail = read(&ctx, json!({"path": "big.log", "offset": -2, "limit": 1})).await;
        assert_eq!(
            tail.text_content(),
            format!(
                "{}\t{}\n\n[Showing line {}; the file has {lines} lines.]",
                lines - 1,
                "z".repeat(1023),
                lines - 1
            )
        );
    }

    /// A read records a stamp even when it returns only part of the file.
    #[tokio::test]
    async fn partial_reads_still_stamp_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered_lines(10)).unwrap();
        let ctx = ctx(dir.path());
        read(&ctx, json!({"path": "a.txt", "offset": -1})).await;
        assert_eq!(ctx.stale_guard.lock().unwrap().len(), 1);
    }

    #[test]
    fn lines_are_split_like_str_lines() {
        let mut reader: &[u8] = b"a\r\nb\n\nc\r";
        let mut buf = Vec::new();
        let mut lines = Vec::new();
        while read_line(&mut reader, &mut buf, true).unwrap().is_some() {
            lines.push(String::from_utf8(buf.clone()).unwrap());
        }
        let expected: Vec<&str> = "a\r\nb\n\nc\r".lines().collect();
        assert_eq!(lines, expected);
    }

    #[test]
    fn sniffing_tells_text_from_binary() {
        assert_eq!(
            sniff(b"plain text"),
            Sniffed::Text {
                encoding: Encoding::Utf8,
                bom: 0
            }
        );
        assert_eq!(
            sniff(b"\xEF\xBB\xBFwith bom"),
            Sniffed::Text {
                encoding: Encoding::Utf8,
                bom: 3
            }
        );
        // A JPEG header has NULs on both sides.
        let jpeg = b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01\x01\x01\x00H\x00H\x00\x00\xFF\xDB";
        assert_eq!(sniff(jpeg), Sniffed::Binary);
        // A few NULs on one side are a binary header, not UTF-16.
        let mut header = vec![b'A'; 64];
        header[4] = 0;
        header[10] = 0;
        assert_eq!(sniff(&header), Sniffed::Binary);
        // UTF-16 without a byte-order mark has a NUL in every other byte.
        let utf16: Vec<u8> = "big endian"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        assert_eq!(
            sniff(&utf16),
            Sniffed::Text {
                encoding: Encoding::Utf16Be,
                bom: 0
            }
        );
        assert_eq!(sniff(b"\x01\x02\x03\x04\x05text"), Sniffed::Binary);
    }

    /// Past the cap a line is counted, not kept: its characters are tallied,
    /// a character split by the cap moves to the dropped part, and a `\r\n`
    /// ending is still not counted.
    #[test]
    fn lines_past_the_cap_are_counted_but_not_kept() {
        let mut reader: &[u8] = "abcdé fgh\r\nnext\n".as_bytes();
        let mut buf = Vec::new();
        let info = read_line_capped(&mut reader, &mut buf, true, 5)
            .unwrap()
            .unwrap();
        // "abcd" plus the first byte of "é" fit in 5 bytes; "é" is dropped whole.
        assert_eq!(buf, b"abcd");
        assert!(info.clipped);
        assert_eq!(info.dropped_chars, "é fgh".chars().count());
        let next = read_line_capped(&mut reader, &mut buf, true, 5)
            .unwrap()
            .unwrap();
        assert_eq!(buf, b"next");
        assert!(!next.clipped);
        assert!(
            read_line_capped(&mut reader, &mut buf, true, 5)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_character_split_by_the_line_cap_is_dropped_whole() {
        let bytes = "aé".as_bytes();
        assert_eq!(complete_prefix_len(&bytes[..2]), 1);
        assert_eq!(complete_prefix_len(bytes), 3);
        assert_eq!(count_chars(bytes), 2);
    }
}
