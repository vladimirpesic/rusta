//! In-stream degeneracy detection — ADR §6.6, the within-completion half.
//!
//! §6.6's FAMA-lite detectors all compare a call to the calls before it, so
//! they can only act *between* turns. A model that starts repeating itself
//! inside one completion is invisible to them and runs to `max_tokens` every
//! time. On CPU that is the most expensive failure available: at ~8 tok/s a
//! 2,048-token degenerate tail costs about four minutes of wall clock, and
//! the 7B runs recorded in §16.9 spent most of their budget that way.
//!
//! Ported from smallcode's `governor/early_stop.js`, including the reason it
//! only inspects the tail: scanning the whole buffer on every token is
//! O(n²) over a completion, which on a slow backend is a second cost on top
//! of the one being prevented.

/// How many consecutive identical cycles count as a loop.
///
/// Two is not enough — boilerplate genuinely repeats twice. Three identical
/// cycles in a row is not something correct output does.
const REPEATS: usize = 3;

/// Longest repeating unit considered, in lines.
///
/// A degenerate model repeats a line, a short block, or a fenced call: all
/// within four lines. Searching further costs more per token than it saves.
const MAX_CYCLE: usize = 4;

/// Lines of tail retained — all the comparison can ever read.
const TAIL_LINES: usize = MAX_CYCLE * REPEATS + 1;

/// Longest single line kept, in characters.
///
/// Output with no newlines at all must not grow the tail without bound, and
/// a line longer than this is compared by its first [`LINE_CAP`] characters.
const LINE_CAP: usize = 512;

/// Watches one completion for degenerate repetition (§6.6).
///
/// Cheap by construction: it keeps a bounded tail (13 lines) and compares
/// at most four cycle lengths, so the cost per token does not grow
/// with the length of the completion — the O(n²) trap smallcode's own
/// implementation documents.
#[derive(Debug, Default)]
pub struct StreamGuard {
    /// Completed lines, most recent last, capped at [`TAIL_LINES`].
    lines: Vec<String>,
    /// The line currently being streamed, not yet terminated.
    partial: String,
}

impl StreamGuard {
    /// Feeds the next streamed fragment. `Some(reason)` means the completion
    /// has started repeating itself and should be abandoned; the reason is
    /// user-facing text.
    pub fn observe(&mut self, fragment: &str) -> Option<&'static str> {
        let mut completed = false;
        for chunk in fragment.split_inclusive('\n') {
            if self.partial.chars().count() < LINE_CAP {
                self.partial.push_str(chunk);
            }
            if chunk.ends_with('\n') {
                let line = std::mem::take(&mut self.partial);
                self.lines.push(line.trim_end().to_owned());
                completed = true;
                if self.lines.len() > TAIL_LINES {
                    self.lines.remove(0);
                }
            }
        }
        // Only a completed line can close a cycle, so nothing changes
        // mid-line and the check runs once per line rather than per token.
        if !completed {
            return None;
        }
        (1..=MAX_CYCLE)
            .any(|cycle| self.cycles_at(cycle))
            .then_some(
                "the reply started repeating itself — abandoned before it \
                 ran out the token budget",
            )
    }

    /// Forgets the current completion. Called at the start of each turn, so
    /// repetition never carries across a turn boundary.
    pub fn reset(&mut self) {
        self.lines.clear();
        self.partial.clear();
    }

    /// Whether the last [`REPEATS`] groups of `cycle` lines are identical.
    fn cycles_at(&self, cycle: usize) -> bool {
        let span = cycle * REPEATS;
        if self.lines.len() < span {
            return false;
        }
        let tail = &self.lines[self.lines.len() - span..];
        let first = &tail[..cycle];
        // An all-blank cycle repeats trivially and means nothing.
        if first.iter().all(|line| line.trim().is_empty()) {
            return false;
        }
        (1..REPEATS).all(|step| &tail[step * cycle..(step + 1) * cycle] == first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_runs_are_not_a_loop() {
        let mut guard = StreamGuard::default();
        for _ in 0..200 {
            assert!(guard.observe("\n").is_none());
        }
    }

    #[test]
    fn reset_forgets_the_previous_completion() {
        let mut guard = StreamGuard::default();
        let line = "xyzzy plugh frobnicate quux ".repeat(2);
        let _ = guard.observe(&line);
        guard.reset();
        assert!(guard.observe(&line).is_none(), "the tail is gone");
    }

    #[test]
    fn multibyte_fragments_never_panic() {
        let mut guard = StreamGuard::default();
        for _ in 0..80 {
            let _ = guard.observe("δοκιμή 🎯 プログラム ");
        }
    }
}
