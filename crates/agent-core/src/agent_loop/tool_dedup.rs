//! Detection and suppression of repetitive tool calls within a single turn.
//!
//! Two behaviours are layered:
//!
//! * **Same-step dedup** — a duplicate `(name, args)` issued in the same LLM
//!   step reuses the original call's result instead of executing the tool
//!   twice.
//! * **Cross-step dedup** — when the exact same call repeats consecutively
//!   across steps, the result handed back to the model is suffixed with a
//!   system reminder once the streak reaches 3. The reminder escalates with
//!   the streak (3, 5, 8), and at 12 the turn is force-stopped so the loop
//!   cannot keep spinning.
//! * **Interleaved-repeat dedup** — a cyclic loop like `scan → bash → scan →
//!   bash …` never builds a *consecutive* streak, so the streak guard alone is
//!   blind to it and the turn can spin until an external timeout. Each call's
//!   *total* count within the turn therefore also drives the reminder ladder.
//!   Interleaved repetition only ever earns reminders — never a hard
//!   force-stop — because an identical call whose result changes each time
//!   (e.g. re-running one test across successive fixes) is legitimate progress,
//!   not a loop.
//!
//! The reminders *redirect* rather than *prohibit*: telling a model "do not
//! repeat this call" leaves it with nowhere to go, and it repeats the call
//! anyway. Each tier instead names a concrete alternative action.

use std::collections::HashMap;

use serde_json::Value;

use crate::agent_loop::types::ExecutableToolResult;

/// Streak length at which the first (softest) reminder is attached.
const REPEAT_REMINDER_1_START: usize = 3;
/// Streak length at which the model must pick from an explicit menu.
const REPEAT_REMINDER_2_START: usize = 5;
/// Streak length at which the model is told to hand back to the user.
const REPEAT_REMINDER_3_START: usize = 8;
/// Streak length at which the turn is force-stopped.
const REPEAT_FORCE_STOP_STREAK: usize = 12;

// Interleaved-repeat thresholds are deliberately higher than the consecutive
// ones: a call repeated with other calls in between (A,B,A,B,…) is a weaker
// loop signal than the same call fired back-to-back, and is more often
// legitimate iteration. This ladder only ever reminds — it never force-stops.
/// Total count at which the first interleaved-repeat reminder is attached.
const TOTAL_REMINDER_1_START: usize = 6;
/// Total count at which the interleaved menu reminder is attached.
const TOTAL_REMINDER_2_START: usize = 9;
/// Total count at which the strongest interleaved reminder is attached.
const TOTAL_REMINDER_3_START: usize = 12;

const REMINDER_TEXT_1: &str = "\n\n<system-reminder>\n\
The same tool call has been repeated several times in a row. \
Before making your next call, write one sentence stating what new information you expect it to produce. \
Then act on that sentence: if it names something this result does not already give you, choose the action that best provides it; otherwise, continue with the evidence you already have.\
\n</system-reminder>";

const REMINDER_TEXT_3: &str = "\n\n<system-reminder>\n\
Write your final response now, without any further tool calls. \
Cover: the current blocker, each approach you have tried and what it established, and the specific information or decision you need from the user to unblock progress. \
Text only.\
\n</system-reminder>";

fn reminder_text_2(repeat_count: usize) -> String {
    format!(
        "\n\n<system-reminder>\n\
The same tool call has now been issued {repeat_count} times in a row. \
Choose exactly one of the following and state your choice before acting:\n\
(1) Falsification check: run the cheapest test that could conclusively disprove your current approach, if such a test exists.\n\
(2) Missing input: tell the user precisely what information or decision you need to proceed, and ask for it.\n\
(3) Conclude: deliver your best result based on the evidence already gathered, listing anything that remains uncertain.\
\n</system-reminder>"
    )
}

/// What the deduplicator did to a finalized result. Recorded for tracing.
///
/// Ordered by severity (`None` < `Remind1` < … < `ForceStop`) so the streak and
/// total-count ladders can be combined with `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RepeatAction {
    None,
    Remind1,
    Remind2,
    Remind3,
    ForceStop,
}

impl RepeatAction {
    /// The action warranted by a consecutive-repeat `streak`.
    fn for_streak(streak: usize) -> Self {
        if streak >= REPEAT_FORCE_STOP_STREAK {
            Self::ForceStop
        } else if streak >= REPEAT_REMINDER_3_START {
            Self::Remind3
        } else if streak >= REPEAT_REMINDER_2_START {
            Self::Remind2
        } else if streak >= REPEAT_REMINDER_1_START {
            Self::Remind1
        } else {
            Self::None
        }
    }

    /// The action warranted by a call's *total* count within the turn (repeats
    /// that may be interleaved with other calls). Higher thresholds than the
    /// consecutive ladder, and it never force-stops — interleaved repetition is
    /// too often legitimate iteration (e.g. re-running one test across
    /// successive fixes) to kill outright.
    fn for_total(total: usize) -> Self {
        if total >= TOTAL_REMINDER_3_START {
            Self::Remind3
        } else if total >= TOTAL_REMINDER_2_START {
            Self::Remind2
        } else if total >= TOTAL_REMINDER_1_START {
            Self::Remind1
        } else {
            Self::None
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Remind1 => "r1",
            Self::Remind2 => "r2",
            Self::Remind3 => "r3",
            Self::ForceStop => "stop",
        }
    }
}

/// Where a registered call sits in the current step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallSlot {
    /// Position of this call in the step, in provider order.
    pub index: usize,
    /// When set, this call is a same-step duplicate of the call at that index,
    /// and must not be executed: it inherits the original's finalized result.
    pub duplicate_of: Option<usize>,
}

impl CallSlot {
    /// Whether this call must actually run.
    pub fn is_original(&self) -> bool {
        self.duplicate_of.is_none()
    }
}

/// Canonical JSON: object keys sorted recursively, so that semantically equal
/// arguments produce byte-identical keys regardless of the provider's field
/// ordering.
fn canonical_args(args: &Value) -> String {
    fn sort(value: &Value) -> Value {
        match value {
            Value::Array(items) => Value::Array(items.iter().map(sort).collect()),
            Value::Object(map) => {
                // `serde_json::Map` preserves insertion order under the
                // `preserve_order` feature, so sort explicitly rather than
                // relying on the map's own ordering.
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort_unstable();
                let mut out = serde_json::Map::with_capacity(map.len());
                for key in keys {
                    out.insert(key.clone(), sort(&map[key]));
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }
    sort(args).to_string()
}

fn make_key(name: &str, args: &Value) -> String {
    format!("{name} {}", canonical_args(args))
}

/// Appends `reminder` to the result's trailing text part, or pushes a new text
/// part when the output does not end in text (e.g. an image).
fn append_reminder(mut result: ExecutableToolResult, reminder: &str) -> ExecutableToolResult {
    use lm_bridge::message::ContentPart;
    match result.output.last_mut() {
        Some(ContentPart::Text { text }) => text.push_str(reminder),
        _ => result.output.push(ContentPart::text(reminder)),
    }
    result
}

/// Tracks repeated `(tool, args)` pairs for the lifetime of one turn.
///
/// The caller drives it: [`begin_step`](Self::begin_step) before registering a
/// step's calls, [`register`](Self::register) once per call in provider order,
/// [`finalize`](Self::finalize) once per *original* call after it executes, and
/// [`end_step`](Self::end_step) when the step's results are in hand.
#[derive(Debug)]
pub struct ToolCallDeduplicator {
    /// Dedup keys for the current step, in provider order, duplicates included.
    step_keys: Vec<String>,
    /// First occurrence of each key in the current step.
    first_index: HashMap<String, usize>,
    /// Key of the run of consecutive identical calls ending at the last step.
    consecutive_key: Option<String>,
    /// Length of that run.
    consecutive_count: usize,
    /// Number of steps in which each key has executed this turn — the basis for
    /// the interleaved-repeat ladder, which the consecutive streak cannot see.
    total_counts: HashMap<String, usize>,
    /// Whether repeats across steps earn reminders and, at the end of the
    /// ladder, a forced stop (`repeat_breaker`). Same-step duplicates share
    /// one execution either way.
    breaker: bool,
}

impl Default for ToolCallDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCallDeduplicator {
    /// A deduplicator with the repeat breaker on.
    pub fn new() -> Self {
        Self::with_breaker(true)
    }

    /// A deduplicator whose repeat breaker is on or off. Off, repeated calls
    /// get no reminders and never stop the turn; a duplicate within one step
    /// still runs once.
    pub fn with_breaker(breaker: bool) -> Self {
        Self {
            step_keys: Vec::new(),
            first_index: HashMap::new(),
            consecutive_key: None,
            consecutive_count: 0,
            total_counts: HashMap::new(),
            breaker,
        }
    }

    /// Clears per-step state. Cross-step streak state survives.
    pub fn begin_step(&mut self) {
        self.step_keys.clear();
        self.first_index.clear();
    }

    /// Registers a call in provider order and reports whether it duplicates an
    /// earlier call in the same step.
    pub fn register(&mut self, name: &str, args: &Value) -> CallSlot {
        let key = make_key(name, args);
        let index = self.step_keys.len();
        self.step_keys.push(key.clone());
        match self.first_index.get(&key) {
            Some(&original) => CallSlot {
                index,
                duplicate_of: Some(original),
            },
            None => {
                self.first_index.insert(key, index);
                CallSlot {
                    index,
                    duplicate_of: None,
                }
            }
        }
    }

    /// Length of the consecutive run of identical calls ending at `index`,
    /// counting the runs carried over from previous steps.
    fn streak_at(&self, index: usize) -> usize {
        let mut last = self.consecutive_key.as_deref();
        let mut streak = self.consecutive_count;
        for key in &self.step_keys[..=index] {
            if Some(key.as_str()) == last {
                streak += 1;
            } else {
                last = Some(key.as_str());
                streak = 1;
            }
        }
        streak
    }

    /// Attaches the escalating reminder warranted by the call's streak, and
    /// force-stops the turn once the streak is hopeless.
    ///
    /// Force-stop never flips a successful result into an error: the tool did
    /// its job, it is the model's use of it that has stalled.
    pub fn finalize(
        &self,
        slot: CallSlot,
        name: &str,
        result: ExecutableToolResult,
    ) -> ExecutableToolResult {
        debug_assert!(slot.is_original(), "duplicates inherit the original result");
        if !self.breaker {
            return result;
        }
        let streak = self.streak_at(slot.index);
        // `end_step` folds the current step's keys into `total_counts` only
        // afterwards, so add this call's own occurrence to the prior total.
        let total = self
            .step_keys
            .get(slot.index)
            .and_then(|key| self.total_counts.get(key))
            .copied()
            .unwrap_or(0)
            + 1;
        // Consecutive spinning may force-stop; interleaved repetition (same call,
        // total count) only escalates reminders. The more severe of the two wins.
        let action = RepeatAction::for_streak(streak).max(RepeatAction::for_total(total));

        if streak >= 2 || total >= REPEAT_REMINDER_1_START {
            tracing::debug!(
                tool = name,
                repeat_count = streak,
                total_count = total,
                action = action.as_str(),
                "repeated tool call"
            );
        }

        match action {
            RepeatAction::None => result,
            RepeatAction::Remind1 => append_reminder(result, REMINDER_TEXT_1),
            RepeatAction::Remind2 => append_reminder(result, &reminder_text_2(streak)),
            RepeatAction::Remind3 => append_reminder(result, REMINDER_TEXT_3),
            RepeatAction::ForceStop => {
                let mut result = append_reminder(result, REMINDER_TEXT_3);
                result.stop_turn = true;
                result
            }
        }
    }

    /// Folds the step's calls into the cross-step streak and total-count state.
    pub fn end_step(&mut self) {
        let mut counted_this_step: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for key in &self.step_keys {
            if self.consecutive_key.as_deref() == Some(key.as_str()) {
                self.consecutive_count += 1;
            } else {
                self.consecutive_key = Some(key.clone());
                self.consecutive_count = 1;
            }
            // Count each distinct key once per step (same-step duplicates never
            // execute), so the total tracks executions, not provider echoes.
            if counted_this_step.insert(key.as_str()) {
                *self.total_counts.entry(key.clone()).or_insert(0) += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(text: &str) -> ExecutableToolResult {
        ExecutableToolResult::text(text)
    }

    /// With the breaker off, a long streak earns no reminder and no stop, but
    /// a duplicate within one step is still recognised.
    #[test]
    fn the_breaker_off_keeps_only_same_step_dedup() {
        let mut d = ToolCallDeduplicator::with_breaker(false);
        let args = json!({"path": "."});
        for _ in 0..20 {
            d.begin_step();
            let slot = d.register("list_dir", &args);
            let result = d.finalize(slot, "list_dir", ok("listing"));
            assert!(!result.stop_turn);
            assert_eq!(result.text_content(), "listing");
            d.end_step();
        }
        d.begin_step();
        let first = d.register("list_dir", &args);
        let second = d.register("list_dir", &args);
        assert!(first.is_original());
        assert_eq!(second.duplicate_of, Some(first.index));
    }

    /// Argument key order must not defeat the detector: providers are free to
    /// serialize object fields in any order.
    #[test]
    fn canonical_args_is_key_order_independent() {
        let a = json!({"b": 1, "a": {"d": 2, "c": [3, {"f": 4, "e": 5}]}});
        let b = json!({"a": {"c": [3, {"e": 5, "f": 4}], "d": 2}, "b": 1});
        assert_eq!(canonical_args(&a), canonical_args(&b));
    }

    #[test]
    fn canonical_args_still_separates_different_values() {
        assert_ne!(
            canonical_args(&json!({"a": 1})),
            canonical_args(&json!({"a": 2}))
        );
        // Array order is meaningful and must be preserved.
        assert_ne!(
            canonical_args(&json!([1, 2])),
            canonical_args(&json!([2, 1]))
        );
    }

    #[test]
    fn same_step_duplicate_points_at_the_original() {
        let mut d = ToolCallDeduplicator::new();
        d.begin_step();
        let a = d.register("read_file", &json!({"path": "a.rs"}));
        let b = d.register("read_file", &json!({"path": "b.rs"}));
        let c = d.register("read_file", &json!({"path": "a.rs"}));
        assert_eq!(a.duplicate_of, None);
        assert_eq!(b.duplicate_of, None);
        assert_eq!(c.duplicate_of, Some(0));
        assert!(a.is_original() && b.is_original() && !c.is_original());
    }

    /// A different tool with identical args is a different call.
    #[test]
    fn key_includes_the_tool_name() {
        let mut d = ToolCallDeduplicator::new();
        d.begin_step();
        d.register("grep", &json!({"q": "x"}));
        let other = d.register("glob", &json!({"q": "x"}));
        assert_eq!(other.duplicate_of, None);
    }

    /// Streak 1 and 2 are silent; the nudge lands exactly on the third.
    #[test]
    fn reminder_lands_on_the_third_consecutive_step() {
        let mut d = ToolCallDeduplicator::new();
        let args = json!({"command": "ls"});

        for expected_streak in 1..=2 {
            d.begin_step();
            let slot = d.register("bash", &args);
            assert_eq!(d.streak_at(slot.index), expected_streak);
            let out = d.finalize(slot, "bash", ok("files"));
            assert_eq!(out.text_content(), "files", "no reminder before streak 3");
            assert!(!out.stop_turn);
            d.end_step();
        }

        d.begin_step();
        let slot = d.register("bash", &args);
        let out = d.finalize(slot, "bash", ok("files"));
        d.end_step();
        assert!(
            out.text_content()
                .contains("what new information you expect")
        );
        assert!(!out.stop_turn);
    }

    /// Escalation boundaries: 3→r1, 5→r2, 8→r3, 12→force stop.
    #[test]
    fn escalates_at_each_threshold() {
        let cases = [
            (1, RepeatAction::None),
            (2, RepeatAction::None),
            (3, RepeatAction::Remind1),
            (4, RepeatAction::Remind1),
            (5, RepeatAction::Remind2),
            (7, RepeatAction::Remind2),
            (8, RepeatAction::Remind3),
            (11, RepeatAction::Remind3),
            (12, RepeatAction::ForceStop),
            (99, RepeatAction::ForceStop),
        ];
        for (streak, expected) in cases {
            assert_eq!(
                RepeatAction::for_streak(streak),
                expected,
                "streak {streak}"
            );
        }
    }

    /// The r2 reminder names the actual count so the model can see it escalating.
    #[test]
    fn second_reminder_quotes_the_repeat_count() {
        let mut d = ToolCallDeduplicator::new();
        let args = json!({"command": "ls"});
        for _ in 0..4 {
            d.begin_step();
            d.register("bash", &args);
            d.end_step();
        }
        d.begin_step();
        let slot = d.register("bash", &args);
        let out = d.finalize(slot, "bash", ok("files"));
        assert!(out.text_content().contains("issued 5 times in a row"));
    }

    /// The turn stops at 12, and a successful tool result stays successful.
    #[test]
    fn force_stop_preserves_is_error_false() {
        let mut d = ToolCallDeduplicator::new();
        let args = json!({"command": "ls"});
        for _ in 0..11 {
            d.begin_step();
            d.register("bash", &args);
            d.end_step();
        }
        d.begin_step();
        let slot = d.register("bash", &args);
        let out = d.finalize(slot, "bash", ok("files"));
        assert!(out.stop_turn);
        assert!(
            !out.is_error,
            "force-stop must not fabricate a tool failure"
        );
        assert!(out.text_content().contains("Write your final response now"));
    }

    /// A failing tool that force-stops stays an error.
    #[test]
    fn force_stop_preserves_is_error_true() {
        let mut d = ToolCallDeduplicator::new();
        let args = json!({"command": "false"});
        for _ in 0..11 {
            d.begin_step();
            d.register("bash", &args);
            d.end_step();
        }
        d.begin_step();
        let slot = d.register("bash", &args);
        let out = d.finalize(slot, "bash", ExecutableToolResult::error("boom"));
        assert!(out.stop_turn);
        assert!(out.is_error);
    }

    /// Interleaving a different call resets the *consecutive* streak. (Total
    /// `ls` here stays below `TOTAL_REMINDER_1_START`, so the interleaved ladder
    /// stays quiet too and this test isolates streak-reset behaviour.)
    #[test]
    fn a_different_call_breaks_the_streak() {
        let mut d = ToolCallDeduplicator::new();
        let ls = json!({"command": "ls"});
        let pwd = json!({"command": "pwd"});

        for _ in 0..3 {
            d.begin_step();
            d.register("bash", &ls);
            d.end_step();
        }
        d.begin_step();
        d.register("bash", &pwd);
        d.end_step();

        d.begin_step();
        let slot = d.register("bash", &ls);
        assert_eq!(d.streak_at(slot.index), 1, "streak restarts after `pwd`");
        let out = d.finalize(slot, "bash", ok("files"));
        assert_eq!(out.text_content(), "files");
    }

    /// A cyclic loop `A,B,A,B,…` never builds a consecutive streak, but the
    /// per-key total climbs. It must earn escalating reminders and, crucially,
    /// must never be force-stopped (interleaved repeats can be real iteration).
    #[test]
    fn interleaved_repeats_remind_but_never_force_stop() {
        let mut d = ToolCallDeduplicator::new();
        let scan = json!({"path": "."});
        let bash = json!({"command": "cc x.c"});
        let mut reminded = false;
        for _ in 0..20 {
            d.begin_step();
            let a = d.register("scan_codebase", &scan);
            let b = d.register("bash", &bash);
            // Alternating results so a result-hash scheme could not help; the
            // total-count ladder is what must catch this.
            let ra = d.finalize(a, "scan_codebase", ok("entities=16 dur=varies"));
            let _rb = d.finalize(b, "bash", ok("error: undefined"));
            d.end_step();
            assert_eq!(d.streak_at(0), 1, "no consecutive streak ever forms");
            assert!(!ra.stop_turn, "interleaved repetition must not force-stop");
            if ra.text_content().contains("system-reminder") {
                reminded = true;
            }
        }
        assert!(reminded, "an interleaved loop must eventually be nudged");
    }

    /// The interleaved (total) ladder reminds but caps below force-stop.
    #[test]
    fn for_total_reminds_but_never_force_stops() {
        assert_eq!(RepeatAction::for_total(5), RepeatAction::None);
        assert_eq!(RepeatAction::for_total(6), RepeatAction::Remind1);
        assert_eq!(RepeatAction::for_total(9), RepeatAction::Remind2);
        assert_eq!(RepeatAction::for_total(12), RepeatAction::Remind3);
        assert_eq!(RepeatAction::for_total(999), RepeatAction::Remind3);
    }

    /// Repeats *within* one step also count toward the streak, so a model that
    /// emits the same call three times in a single batch is nudged too. The
    /// duplicates never execute, but they do advance the run.
    #[test]
    fn repeats_within_a_single_step_count_toward_the_streak() {
        let mut d = ToolCallDeduplicator::new();
        let args = json!({"command": "ls"});
        d.begin_step();
        let s0 = d.register("bash", &args);
        let s1 = d.register("bash", &args);
        let s2 = d.register("bash", &args);
        assert_eq!(s1.duplicate_of, Some(0));
        assert_eq!(s2.duplicate_of, Some(0));
        assert_eq!(d.streak_at(s0.index), 1);
        assert_eq!(d.streak_at(s2.index), 3);
    }

    /// A reminder must not be swallowed when the tool returned an image.
    #[test]
    fn reminder_appends_a_text_part_to_non_text_output() {
        use lm_bridge::message::ContentPart;
        let mut d = ToolCallDeduplicator::new();
        let args = json!({"path": "a.png"});
        for _ in 0..2 {
            d.begin_step();
            d.register("read_media", &args);
            d.end_step();
        }
        d.begin_step();
        let slot = d.register("read_media", &args);
        let image = ExecutableToolResult {
            output: vec![ContentPart::ImageUrl {
                image_url: lm_bridge::message::ImageUrl {
                    url: "data:image/png;base64,AA==".to_string(),
                    id: None,
                },
            }],
            is_error: false,
            stop_turn: false,
        };
        let out = d.finalize(slot, "read_media", image);
        assert_eq!(out.output.len(), 2, "text part appended after the image");
        assert!(matches!(out.output[0], ContentPart::ImageUrl { .. }));
        assert!(
            out.text_content()
                .contains("what new information you expect")
        );
    }

    /// `begin_step` must not leak the previous step's duplicate map.
    #[test]
    fn begin_step_clears_same_step_state() {
        let mut d = ToolCallDeduplicator::new();
        let args = json!({"command": "ls"});
        d.begin_step();
        d.register("bash", &args);
        d.end_step();

        d.begin_step();
        let slot = d.register("bash", &args);
        assert_eq!(
            slot.duplicate_of, None,
            "a repeat in the *next* step is an original, not a same-step dup"
        );
        assert_eq!(slot.index, 0);
    }
}
