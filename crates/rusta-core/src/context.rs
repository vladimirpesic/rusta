//! Context manager — ADR §6.6 (R7): purity + JIT + compression.
//!
//! Three subsystems, none of them resident in the core prompt:
//!
//! * **JIT skill cards** ([`SkillCard`], [`CardDeck`]): markdown files with a
//!   flat YAML-style front-matter (little-coder's schema). On a trigger match
//!   at most [`MAX_INJECTED_CARDS`] cards are appended to the request as a
//!   trailing system note; the note is per-request, so eviction at task end
//!   is structural — a card is never stored in the session log.
//! * **History compression** ([`Compressor`]): when the assembled prompt
//!   would exceed 60% of the context window, the oldest turns collapse into a
//!   single `assistant` episodic summary, keeping verbatim the last
//!   [`KEEP_TURNS`] turns, every edit block, and every error-feedback
//!   observation. [`Compressor::apply_summary`] is the single code path for
//!   live compression *and* `Summary`-event replay, so `/resume` reproduces
//!   the live context byte-for-byte.
//! * **Loop mitigation, FAMA-lite** ([`LoopGuard`]): four detectors over
//!   `tool|args` fingerprints and validator outputs trip one-line imperative
//!   capsules (SmallCTL's proven set, adapted). Capsules are deduplicated,
//!   capped ([`CAPSULE_TOKEN_BUDGET`], [`MAX_ACTIVE_CAPSULES`]) and expire
//!   after [`CAPSULE_TTL_TURNS`] turns; a detector at 2× its trip threshold
//!   escalates: automatic `Editing → Planning` regression plus a user
//!   notification (§6.6).
//!
//! Token counts use rusta-llm's conservative `ceil(chars / 3)` estimator, so
//! every budget here is stricter than any real tokenizer.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use rusta_llm::tokens::estimate_tokens;
use rusta_llm::{Message, Role};
use serde_json::Value;

use crate::error::Error;

/// Hard cap for one skill card's body, in estimated tokens (§6.6).
pub const CARD_TOKEN_BUDGET: u64 = 120;
/// At most this many cards may be injected per request (§6.6).
pub const MAX_INJECTED_CARDS: usize = 2;
/// Turns kept verbatim by history compression (§6.6).
pub const KEEP_TURNS: usize = 3;
/// Compression trips when the assembled prompt exceeds this fraction of the
/// context window (§6.6), expressed as `numerator / denominator`.
///
/// Integer, not `0.6`: the threshold must round identically on every build,
/// and a `f64` constant sitting beside the live `window * 3 / 5` was an
/// edit-one-not-the-other trap — it was referenced only by a doc comment.
pub const CONTEXT_WINDOW_FRACTION: (u64, u64) = (3, 5);
/// Total token budget for active loop-mitigation capsules (§6.6).
pub const CAPSULE_TOKEN_BUDGET: u64 = 180;
/// At most this many capsules may be active at once (§6.6).
pub const MAX_ACTIVE_CAPSULES: usize = 5;
/// Capsules expire after this many turns (§6.6).
pub const CAPSULE_TTL_TURNS: u32 = 3;

/// Floor for an observation clipped to fit the window. Below this a `read`
/// result stops carrying usable evidence, so the clipper stops rather than
/// shaving a message into uselessness — and the §6.1 size guard reports the
/// overflow instead of hiding it.
pub const MIN_KEPT_OBSERVATION: u64 = 256;

/// Appended to an observation clipped by [`Compressor::compress`], so the
/// model narrows its next read instead of assuming it saw the whole file —
/// the same contract as the §6.1 tool caps.
const CLIP_MARKER: &str =
    "\n[… clipped to fit the context window — narrow the range and read again]";

/// Detector (a): trips at this many identical `tool|args` calls.
pub const STAGNATION_TRIP: u32 = 3;
/// Detector (b): trips at this many *consecutive* identical tool calls.
pub const CONSECUTIVE_TRIP: usize = 3;
/// Detector (c): trips at this many identical validator outputs in a row.
pub const VALIDATOR_REPEAT_TRIP: u32 = 2;

// ---------------------------------------------------------------------------
// JIT skill cards
// ---------------------------------------------------------------------------

/// Card category — little-coder's `type` axis (ADR §6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    /// Guidance for calling a specific tool well.
    Tool,
    /// Domain knowledge, triggered by keywords.
    Knowledge,
    /// Error-recovery recipes, triggered by error kinds.
    Recovery,
}

impl CardKind {
    /// Parses the front-matter `type` value.
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "tool" => Ok(Self::Tool),
            "knowledge" => Ok(Self::Knowledge),
            "recovery" => Ok(Self::Recovery),
            other => Err(format!(
                "invalid type {other:?}: expected tool, knowledge, or recovery"
            )),
        }
    }

    /// The front-matter spelling of this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Knowledge => "knowledge",
            Self::Recovery => "recovery",
        }
    }
}

/// One JIT skill card: front-matter metadata plus a short imperative body.
///
/// Bodies are data, versioned under `skills/` (ADR §5), and are the only
/// part of a card that costs context when injected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCard {
    name: String,
    kind: CardKind,
    triggers: Vec<String>,
    priority: u8,
    declared_cost: u64,
    user_invocable: bool,
    body: String,
}

impl SkillCard {
    /// Parses a card from `---`-delimited front-matter plus markdown body.
    ///
    /// The schema is little-coder's flat one and is validated strictly —
    /// every key required, every value checked, unknown keys rejected — so a
    /// typo in a card fails at load time with the remedy in the message,
    /// never silently at injection time.
    pub fn parse(text: &str) -> Result<Self, String> {
        // CRLF cards are ordinary on Windows checkouts; normalize once so
        // the delimiter scan and the body both see LF.
        let normalized;
        let text = if text.contains('\r') {
            normalized = text.replace("\r\n", "\n");
            normalized.as_str()
        } else {
            text
        };
        let rest = text
            .strip_prefix("---\n")
            .ok_or_else(|| "missing opening '---' front-matter delimiter".to_owned())?;
        let (front, body) = frontmatter_split(rest)
            .ok_or_else(|| "missing closing '---' front-matter delimiter".to_owned())?;
        let body = body.trim_start_matches(['\r', '\n']).trim_end().to_owned();

        let mut name: Option<String> = None;
        let mut kind: Option<CardKind> = None;
        let mut triggers: Option<Vec<String>> = None;
        let mut priority: Option<u8> = None;
        let mut declared_cost: Option<u64> = None;
        let mut user_invocable: Option<bool> = None;

        for (number, line) in front.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                return Err(format!(
                    "front-matter line {}: expected 'key: value', got {line:?}",
                    number + 1
                ));
            };
            let value = value.trim();
            match key.trim() {
                "name" => name = Some(parse_name(value, number + 1)?),
                "type" => kind = Some(CardKind::parse(value)?),
                "triggers" => triggers = Some(parse_triggers(value)?),
                "priority" => priority = Some(parse_priority(value, number + 1)?),
                "token_cost" => {
                    declared_cost = Some(parse_token_cost(value, number + 1)?);
                }
                "user-invocable" => {
                    user_invocable = Some(parse_bool(value, number + 1)?);
                }
                unknown => {
                    return Err(format!(
                        "front-matter line {}: unknown key {unknown:?}",
                        number + 1
                    ));
                }
            }
        }

        let card = Self {
            name: require(name, "name")?,
            kind: require(kind, "type")?,
            triggers: require(triggers, "triggers")?,
            priority: require(priority, "priority")?,
            declared_cost: require(declared_cost, "token_cost")?,
            user_invocable: require(user_invocable, "user-invocable")?,
            body,
        };
        card.validate()?;
        Ok(card)
    }

    /// Enforces the declared-cost and body-budget invariants (§6.6).
    fn validate(&self) -> Result<(), String> {
        if self.declared_cost > CARD_TOKEN_BUDGET {
            return Err(format!(
                "declared token_cost {} exceeds the {}-token card budget",
                self.declared_cost, CARD_TOKEN_BUDGET
            ));
        }
        let estimated = estimate_tokens(&self.body);
        if estimated > CARD_TOKEN_BUDGET {
            return Err(format!(
                "body is {estimated} estimated tokens, over the {}-token card budget: shorten the card",
                CARD_TOKEN_BUDGET
            ));
        }
        if self.body.is_empty() {
            return Err("body is empty: a card must carry an imperative".to_owned());
        }
        Ok(())
    }

    /// The card's front-matter name (unique within a deck).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The card category.
    pub fn kind(&self) -> CardKind {
        self.kind
    }

    /// Trigger cues: tool names, error kinds, or keywords.
    pub fn triggers(&self) -> &[String] {
        &self.triggers
    }

    /// Injection priority, 1–9; higher wins when cards compete.
    pub fn priority(&self) -> u8 {
        self.priority
    }

    /// The front-matter `token_cost`, as declared by the author.
    pub fn declared_cost(&self) -> u64 {
        self.declared_cost
    }

    /// Whether `/skills <name>` may invoke this card directly (§6.8).
    pub fn user_invocable(&self) -> bool {
        self.user_invocable
    }

    /// The card body — the only part that costs context when injected.
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Estimated token cost of the body (what injection actually spends).
    pub fn token_cost(&self) -> u64 {
        estimate_tokens(&self.body)
    }

    /// Whether `cue` fires this card: a trigger equals the cue as a whole
    /// word (case-insensitive), so `read` fires on `"re-read the file"` but
    /// not on `"spreadsheet"`. Multi-word triggers match as a padded phrase.
    pub fn matches(&self, cue: &str) -> bool {
        self.triggers.iter().any(|t| trigger_matches(t, cue))
    }
}

/// Splits `rest` (text after the opening delimiter) into front-matter and
/// body at the closing `---` line: `(front, body)` excludes both delimiters.
fn frontmatter_split(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0usize;
    for line in rest.lines() {
        let line_end = offset + line.len();
        if line.trim() == "---" {
            let body = rest[line_end..]
                .strip_prefix('\n')
                .unwrap_or(&rest[line_end..]);
            return Some((&rest[..offset], body));
        }
        offset = line_end + 1;
    }
    None
}

/// `require(field, "key")` — turns a missing key into a loud parse error.
fn require<T>(value: Option<T>, key: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("missing required front-matter key {key:?}"))
}

fn parse_name(value: &str, line: usize) -> Result<String, String> {
    if value.is_empty() || value.contains(char::is_whitespace) {
        return Err(format!(
            "front-matter line {line}: name must be one non-empty word"
        ));
    }
    Ok(value.to_owned())
}

/// Parses the triggers list `[a, b, c]`; an empty list `[]` is allowed.
fn parse_triggers(value: &str) -> Result<Vec<String>, String> {
    let value = value.trim();
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .ok_or_else(|| format!("invalid triggers {value:?}: expected [cue, …]"))?;
    Ok(inner
        .split(',')
        .map(|cue| cue.trim().to_owned())
        .filter(|cue| !cue.is_empty())
        .collect())
}

fn parse_priority(value: &str, line: usize) -> Result<u8, String> {
    let parsed: u8 = value
        .parse()
        .map_err(|_| format!("front-matter line {line}: priority must be an integer 1-9"))?;
    if !(1..=9).contains(&parsed) {
        return Err(format!(
            "front-matter line {line}: priority {parsed} out of range 1-9"
        ));
    }
    Ok(parsed)
}

fn parse_token_cost(value: &str, line: usize) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("front-matter line {line}: token_cost must be a non-negative integer"))
}

fn parse_bool(value: &str, line: usize) -> Result<bool, String> {
    value
        .parse()
        .map_err(|_| format!("front-matter line {line}: user-invocable must be true or false"))
}

/// Word-boundary trigger match (case-insensitive). Single-word triggers
/// compare against each cue word; multi-word triggers match as a padded
/// phrase, so `binary search` fires inside "use binary search here".
fn trigger_matches(trigger: &str, cue: &str) -> bool {
    let trigger = trigger.to_lowercase();
    if trigger.contains(' ') {
        let padded = format!(" {trigger} ");
        return format!(" {cue} ").to_lowercase().contains(&padded);
    }
    cue.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| word == trigger)
}

/// The starter deck (ADR §6.6), embedded so it ships with the binary.
static SHIPPED_CARDS: [&str; 4] = [
    include_str!("../../../skills/edit-recovery.md"),
    include_str!("../../../skills/read-large-files.md"),
    include_str!("../../../skills/verify-focus.md"),
    include_str!("../../../skills/write-vs-edit.md"),
];

/// A loaded, trigger-matched deck of skill cards.
///
/// Cards are kept sorted by priority (desc) then name, so selection order is
/// deterministic regardless of directory iteration order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CardDeck {
    cards: Vec<SkillCard>,
}

impl CardDeck {
    /// The starter deck, compiled into the binary.
    ///
    /// Cards are Rusta's own data (ADR §5 ships them beside the crates), so
    /// they must travel with the binary. Reading them from the *target*
    /// repository instead meant the shipped deck only ever loaded when Rusta
    /// was run on Rusta, and that a user repo with an unrelated `skills/`
    /// directory failed to start at all.
    pub fn shipped() -> Self {
        let cards = SHIPPED_CARDS
            .iter()
            .map(|text| {
                SkillCard::parse(text)
                    .expect("the shipped deck is pinned by shipped_cards_parse_and_fit")
            })
            .collect();
        Self::from_cards(cards)
    }

    /// The shipped deck plus any project cards under `<root>/.rusta/skills/`.
    ///
    /// The path is Rusta-owned and unambiguous, so a malformed card there is
    /// still a loud error — unlike the old generic `skills/`, which collides
    /// with several other tools' conventions. A project card with the same
    /// `name` as a shipped one replaces it.
    pub fn load_for_repo(root: &Path) -> Result<Self, Error> {
        let mut cards: Vec<SkillCard> = Self::shipped().cards;
        let project = Self::load(&root.join(".rusta").join("skills"))?;
        for card in project.cards {
            match cards.iter().position(|c| c.name == card.name) {
                Some(index) => cards[index] = card,
                None => cards.push(card),
            }
        }
        Ok(Self::from_cards(cards))
    }

    /// Loads every `*.md` card in `dir` (non-recursive), sorted by file name.
    /// A missing directory is an empty deck — no cards, no injection; a
    /// malformed card is a loud load error with the file named.
    pub fn load(dir: &Path) -> Result<Self, Error> {
        let entries = match dir.read_dir() {
            Ok(entries) => entries,
            // A missing deck directory is simply no cards.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(Error::from(error)),
        };
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(Error::from)?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md") {
                paths.push(path);
            }
        }
        paths.sort();
        let mut cards = Vec::with_capacity(paths.len());
        for path in paths {
            let text = std::fs::read_to_string(&path).map_err(Error::from)?;
            cards.push(SkillCard::parse(&text).map_err(|cause| Error::SkillCard {
                path: path.display().to_string(),
                cause,
            })?);
        }
        let mut deck = Self { cards };
        deck.sort();
        Ok(deck)
    }

    /// A deck from already-parsed cards (tests, embedded sets).
    pub fn from_cards(cards: Vec<SkillCard>) -> Self {
        let mut deck = Self { cards };
        deck.sort();
        deck
    }

    fn sort(&mut self) {
        self.cards.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.name.cmp(&b.name))
        });
    }

    /// All cards, best priority first.
    pub fn cards(&self) -> &[SkillCard] {
        &self.cards
    }

    /// The card a user may invoke by name via `/skills <name>` (§6.8);
    /// non-invocable cards are not reachable this way.
    pub fn invocable(&self, name: &str) -> Option<&SkillCard> {
        self.cards
            .iter()
            .find(|card| card.user_invocable && card.name.eq_ignore_ascii_case(name))
    }

    /// The cards fired by `cues`, best priority first, at most
    /// [`MAX_INJECTED_CARDS`] (§6.6). Cues are tool names about to run,
    /// error kinds observed, or free text such as the user's message.
    pub fn select(&self, cues: &[&str]) -> Vec<&SkillCard> {
        let mut hits: Vec<&SkillCard> = self
            .cards
            .iter()
            .filter(|card| cues.iter().any(|cue| card.matches(cue)))
            .collect();
        hits.truncate(MAX_INJECTED_CARDS);
        hits
    }

    /// The trailing system note for the cards fired by `cues` — `None` when
    /// nothing fires. The note exists only for the request it was built for:
    /// eviction at task end is structural, and cards never enter the session
    /// log (§6.6).
    pub fn skill_note(&self, cues: &[&str]) -> Option<Message> {
        let selected = self.select(cues);
        if selected.is_empty() {
            return None;
        }
        let bodies: Vec<&str> = selected.iter().map(|card| card.body.as_str()).collect();
        Some(Message::system(format!(
            "SKILL NOTE:\n{}",
            bodies.join("\n---\n")
        )))
    }
}

// ---------------------------------------------------------------------------
// History compression
// ---------------------------------------------------------------------------

/// One episodic summary planned by [`Compressor::compress`] — the payload of
/// the `Summary` session event (§6.10), so replay reproduces the compressed
/// context exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryPlan {
    /// How many leading turns the summary replaces.
    pub covers_turns: u32,
    /// The summary text (without the `Summary of earlier turns:` header).
    pub text: String,
}

/// The result of a compression pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Compression {
    /// The new message list (the input when nothing compressed).
    pub messages: Vec<Message>,
    /// The episodic summary, if compression ran.
    pub summary: Option<SummaryPlan>,
}

/// Episodic history compression (§6.6): when the assembled prompt would
/// exceed [`CONTEXT_WINDOW_FRACTION`] of the window, the oldest turns
/// collapse into one `assistant` summary, keeping verbatim the last
/// [`KEEP_TURNS`] turns, every edit block, and every error-feedback
/// observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compressor {
    window_tokens: u64,
}

impl Compressor {
    /// A compressor for a model context window of `window_tokens`.
    pub fn new(window_tokens: u64) -> Self {
        Self { window_tokens }
    }

    /// The token budget history may occupy before compression trips: 60% of
    /// the window (§6.6), computed in integer math — `0.6` is not exact in
    /// binary, and budgets must round consistently.
    pub fn history_budget(&self) -> u64 {
        self.window_tokens * CONTEXT_WINDOW_FRACTION.0 / CONTEXT_WINDOW_FRACTION.1
    }

    /// Estimated history tokens plus `reserved` (core prompt, cards, notes)
    /// against the compression threshold.
    fn over_budget(&self, history: &[Message], reserved: u64) -> bool {
        let spent: u64 = reserved
            + history
                .iter()
                .map(|message| estimate_tokens(&message.content))
                .sum::<u64>();
        spent > self.history_budget()
    }

    /// Compresses `history` when it (plus `reserved` tokens of system
    /// material) exceeds the threshold. Deterministic and best-effort: if
    /// the mandated verbatim material alone exceeds the budget, the result
    /// still returns — the caller's window sizing and turn cap handle the
    /// remainder (§6.1).
    pub fn compress(&self, history: Vec<Message>, reserved: u64) -> Compression {
        let total_turns = count_turns(&history);
        if !self.over_budget(&history, reserved) {
            return Compression {
                messages: history,
                summary: None,
            };
        }
        if total_turns <= KEEP_TURNS {
            // Dropping turns is not available: §6.6 keeps the last
            // [`KEEP_TURNS`] verbatim, and there are no others. But three
            // `read` observations at the §6.1 cap is 65k estimated tokens
            // against a 32k window — twice the whole context — and the
            // request used to ship at that size, because nothing downstream
            // measured it. Clip the oversized observations instead, exactly
            // as a sub-coder clips its own (§6.8 `OBSERVATION_TOKEN_CAP`):
            // "verbatim" becomes "verbatim up to a cap", which keeps every
            // turn present rather than losing the whole request.
            return Compression {
                messages: self.clip_oversized(history, reserved),
                summary: None,
            };
        }
        let covers = (total_turns - KEEP_TURNS) as u32;
        let text = summarize_turns(&history, covers);
        let mut messages = history;
        Self::apply_summary(&mut messages, covers, &text);
        Compression {
            messages,
            summary: Some(SummaryPlan {
                covers_turns: covers,
                text,
            }),
        }
    }

    /// Clips the largest observations until the history fits the §6.6
    /// budget, largest first, never below [`MIN_KEPT_OBSERVATION`] tokens
    /// each.
    ///
    /// Only `user`-role observations are clipped: assistant turns carry the
    /// model's own reasoning and edit blocks, which §6.6 keeps verbatim and
    /// which are small in practice. A clip is marked in the text so the
    /// model narrows its next read rather than assuming it saw everything —
    /// the same contract as the §6.1 tool caps.
    fn clip_oversized(&self, mut history: Vec<Message>, reserved: u64) -> Vec<Message> {
        loop {
            if !self.over_budget(&history, reserved) {
                return history;
            }
            // The biggest clippable observation, if any is still worth cutting.
            let target = history
                .iter()
                .enumerate()
                .filter(|(_, message)| message.role == Role::User)
                .map(|(index, message)| (index, estimate_tokens(&message.content)))
                .filter(|(_, cost)| *cost > MIN_KEPT_OBSERVATION)
                .max_by_key(|(_, cost)| *cost);
            let Some((index, cost)) = target else {
                return history; // nothing left to give; the guard reports it
            };
            // Halve it, with a floor — repeated passes converge quickly and
            // spread the loss across turns instead of gutting the first one.
            let budget = (cost / 2).max(MIN_KEPT_OBSERVATION);
            let kept: String = history[index]
                .content
                .chars()
                .take(budget as usize * 3)
                .collect();
            let clipped = format!("{kept}{CLIP_MARKER}");
            // Strict progress, or stop. The marker costs tokens of its own,
            // so a message only just above the floor clips to something no
            // smaller — and since this always picks the *largest* candidate,
            // nothing smaller could shrink either. Without this the loop
            // spins forever on exactly that input.
            if estimate_tokens(&clipped) >= cost {
                return history;
            }
            history[index].content = clipped;
        }
    }

    /// Replaces the oldest `covers_turns` turns with the episodic summary
    /// message, preserving verbatim keepers (edit blocks, error feedback) in
    /// order after it. A turn is one model completion (§6.1): an assistant
    /// message plus the observations that follow it. This is the *only*
    /// implementation of turn dropping — `Session` replay calls it for
    /// `Summary` events, so a resumed session reconstructs the live context
    /// byte-for-byte.
    pub fn apply_summary(messages: &mut Vec<Message>, covers_turns: u32, text: &str) {
        if covers_turns == 0 {
            return;
        }
        let mut seen = 0usize;
        let mut split = messages.len();
        for (index, message) in messages.iter().enumerate() {
            if message.role == Role::Assistant {
                seen += 1;
                if seen > covers_turns as usize {
                    split = index;
                    break;
                }
            }
        }
        let keepers: Vec<Message> = messages[..split]
            .iter()
            .filter(|message| is_verbatim_keeper(message))
            .cloned()
            .collect();
        messages.drain(..split);
        let mut head = Vec::with_capacity(keepers.len() + 1);
        head.push(Message::assistant(format!(
            "Summary of earlier turns:\n{text}"
        )));
        head.extend(keepers);
        messages.splice(..0, head);
    }
}

/// Whether a message must survive compression verbatim (§6.6): an applied
/// edit block, or an error-feedback observation.
fn is_verbatim_keeper(message: &Message) -> bool {
    match message.role {
        Role::Assistant => message.content.contains("<<<<<<< SEARCH"),
        Role::User => {
            message.content.starts_with("TOOL RESULT") && message.content.contains("(error)")
        }
        Role::System => false,
    }
}

/// Number of turns — model completions (§6.1): assistant messages.
fn count_turns(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .count()
}

/// Maximum turn lines kept in one episodic summary.
const SUMMARY_MAX_LINES: usize = 30;
/// Character clip for user/assistant excerpts inside summary lines.
const SUMMARY_CLIP: usize = 60;

/// Builds the extractive episodic summary of the oldest `covers` turns —
/// one line per turn (a turn = one model completion, §6.1), tools and
/// edited paths counted, error feedback noted. A previous summary heading
/// the dropped range is carried over line by line, so episodic memory
/// accumulates across successive compressions instead of being lost. Fully
/// deterministic: no model call, no clocks.
fn summarize_turns(messages: &[Message], covers: u32) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut turn_index = 0usize;
    let mut turn_line = String::new();
    let mut overflow = 0usize;

    let flush = |turn_index: usize,
                 turn_line: &mut String,
                 lines: &mut Vec<String>,
                 overflow: &mut usize| {
        if turn_line.is_empty() {
            return;
        }
        if lines.len() < SUMMARY_MAX_LINES {
            lines.push(format!("{turn_index}. {turn_line}"));
        } else {
            *overflow += 1;
        }
        turn_line.clear();
    };

    for message in messages {
        // A model completion opens a turn; material before the first one
        // (the user's request, a prior summary) folds into turn 1.
        if message.role == Role::Assistant {
            if turn_index > 0 {
                flush(turn_index, &mut turn_line, &mut lines, &mut overflow);
            }
            turn_index += 1;
        }
        if (turn_index as u32) > covers {
            break; // tail turns are kept verbatim, not summarized
        }
        digest_message(message, &mut turn_line, &mut lines);
    }
    flush(turn_index, &mut turn_line, &mut lines, &mut overflow);

    if overflow > 0 {
        lines.push(format!("… (+{overflow} more turns)"));
    }
    lines.join("\n")
}

/// Folds one message into its turn's digest line, or into carried summary
/// lines when it *is* a previous episodic summary.
fn digest_message(message: &Message, turn_line: &mut String, lines: &mut Vec<String>) {
    let content = message.content.as_str();
    match message.role {
        Role::User => {
            if let Some(rest) = content.strip_prefix("TOOL RESULT ") {
                let (name, failed) = match rest.split_once(" (") {
                    Some((name, status)) => (name, status.starts_with("error")),
                    None => (rest, false),
                };
                push_field(
                    turn_line,
                    &format!("{name}{}", if failed { "!" } else { "" }),
                );
            } else if !content.trim().is_empty() {
                push_field(turn_line, &format!("user: {}", clip(content)));
            }
        }
        Role::Assistant => {
            if let Some(rest) = content.strip_prefix("Summary of earlier turns:\n") {
                // Prior episodic memory: carry it verbatim, ahead of new lines.
                let carried = rest.trim_end();
                if !carried.is_empty() {
                    let mut merged: Vec<String> = carried.lines().map(str::to_owned).collect();
                    merged.append(lines);
                    *lines = merged;
                }
            } else if let Some(path) = edit_block_path(content) {
                push_field(turn_line, &format!("edit {path}"));
            } else if !content.trim().is_empty() {
                push_field(turn_line, &format!("said: {}", clip(content)));
            }
        }
        Role::System => {}
    }
}

/// Appends `; field` unless the line is empty (then just `field`).
fn push_field(line: &mut String, field: &str) {
    if line.is_empty() {
        line.push_str(field);
    } else {
        line.push_str("; ");
        line.push_str(field);
    }
}

/// First line of `text`, whitespace-collapsed, clipped to [`SUMMARY_CLIP`]
/// characters with an ellipsis marker.
fn clip(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= SUMMARY_CLIP {
        return collapsed;
    }
    let mut clipped: String = collapsed.chars().take(SUMMARY_CLIP).collect();
    clipped.push('…');
    clipped
}

/// The file path of the first edit block in `content`, per the §6.3 layout:
/// a path line directly above `<<<<<<< SEARCH`.
fn edit_block_path(content: &str) -> Option<&str> {
    let mut previous: Option<&str> = None;
    for line in content.lines() {
        if line.trim() == "<<<<<<< SEARCH" {
            let candidate = previous?;
            let trimmed = candidate.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("```")
                || trimmed.starts_with("<<<<")
                || trimmed.starts_with("====")
                || trimmed.starts_with(">>>>")
            {
                return None;
            }
            return Some(trimmed);
        }
        previous = Some(line);
    }
    None
}

// ---------------------------------------------------------------------------
// Loop mitigation — FAMA-lite (§6.6)
// ---------------------------------------------------------------------------

/// One mitigation capsule: a single imperative line naming the exact next
/// action (SmallCTL's proven set, adapted per ADR §6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capsule {
    /// Stable capsule name (detector-facing identity).
    pub name: &'static str,
    /// The imperative line injected on the model's next prompt.
    pub text: &'static str,
    /// When capsules compete for the token budget, higher wins.
    pub priority: u8,
}

/// The capsule registry. `repeat_breaker` serves detector (a) stagnation,
/// `evidence_reuse` detector (b) consecutive identical calls,
/// `mutation_required` detector (c) repeated identical validator outputs,
/// and `no_op_edit` detector (d) REPLACE == SEARCH.
static CAPSULES: [Capsule; 7] = [
    Capsule {
        name: "read_the_error",
        text: "Two tool calls rejected for malformed arguments. Re-read the tool's argument list in the prompt before the next call.",
        priority: 8,
    },
    Capsule {
        name: "confirm_the_name",
        text: "That path or identifier does not exist. Confirm it with `glob` or `map_refresh` — do not guess another spelling.",
        priority: 9,
    },
    Capsule {
        name: "whole_file_rewrite",
        text: "SEARCH has missed twice on this file. Stop composing SEARCH text — call `write` with the whole corrected file.",
        priority: 9,
    },
    Capsule {
        name: "repeat_breaker",
        text: "Do not repeat the same tool call unchanged; use prior output or switch to a different action.",
        priority: 8,
    },
    Capsule {
        name: "evidence_reuse",
        text: "Use the evidence already in context before reading or running anything again.",
        priority: 7,
    },
    Capsule {
        name: "mutation_required",
        text: "MUTATION REQUIRED: You have read enough. Emit ONE edit block this turn, then verify.",
        priority: 9,
    },
    Capsule {
        name: "no_op_edit",
        text: "No-op edit: REPLACE equals SEARCH. Write the actually changed lines in REPLACE and re-apply.",
        priority: 6,
    },
];

/// Looks a capsule up by name (tests, M8 display).
pub fn capsule(name: &str) -> Option<&'static Capsule> {
    CAPSULES.iter().find(|capsule| capsule.name == name)
}

/// What one observation produced: the capsules that tripped (possibly from
/// several detectors at once) and whether any detector reached 2× its trip
/// threshold — the escalation condition (§6.6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trip {
    /// Names of the capsules fired by this observation.
    pub capsules: Vec<&'static str>,
    /// A detector hit 2× its trip threshold: regress and notify.
    pub escalate: bool,
}

impl Trip {
    fn none() -> Self {
        Self::default()
    }
}

/// An active mitigation and when it was activated (for TTL expiry).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveCapsule {
    capsule: &'static Capsule,
    activated: u32,
}

/// The escalation directive (§6.6): a detector reached 2× its trip
/// threshold, so the agent regresses `Editing → Planning` and the user is
/// notified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escalation {
    /// Which detector latched (a capsule name).
    pub reason: &'static str,
    /// One-line user notification.
    pub notify: String,
}

/// The loop guard — FAMA-lite's four detectors, capsule state, and the
/// escalation latch (ADR §6.6). Observations come in from the agent loop
/// as things happen; the guard is task-scoped and in-memory (it is not
/// journaled: `/resume` starts detectors fresh, which is the safe side).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopGuard {
    /// Detector (a): lifetime call count per `tool|args` fingerprint.
    stagnation: HashMap<String, u32>,
    /// Rolling window of recent fingerprints, feeding detector (b).
    recent: VecDeque<String>,
    /// Detector (c): consecutive-identical-output count per validator
    /// command. Keyed by command because the §7 default configures three of
    /// them: with a single slot, running `check`, `clippy`, `test` in
    /// sequence resets the counter on every call and the detector could
    /// never trip.
    last_validator: HashMap<String, (String, u32)>,
    /// Detector (d): no-op edits seen.
    noop_edits: u32,
    /// Consecutive failed edits per file (§6.6).
    ///
    /// Aider's default `edit_format` is `whole` and it promotes a model to
    /// `diff` only when that model is known to handle SEARCH/REPLACE. Rusta
    /// is diff-only, so the equivalent is to notice when the *format* is
    /// failing rather than the attempt, and route to `write`. Two misses on
    /// one file is that signal: the 7B in §16.9 failed repeatedly against
    /// SEARCH text it had invented, which no retry could ever match.
    patch_failures: std::collections::BTreeMap<String, u32>,
    /// Tool calls rejected for malformed arguments, this task (§6.6).
    bad_args: u32,
    /// Fingerprints of calls that have failed, and how often (§6.6).
    ///
    /// Advice is not a control. A 7B spent a whole turn budget re-issuing
    /// one malformed `map_drill`: the error was identical 30 times, the
    /// `bad_tool_args` card fired and was ignored, the stagnation detector
    /// tripped — and `LoopEscalated` exists only as `(Editing → Planning)`,
    /// so in `Exploring` it was a no-op. §6.4's answer to an action that
    /// must not happen is to make it unreachable; this is that answer for a
    /// call that cannot succeed.
    failed_calls: std::collections::BTreeMap<String, u32>,
    /// Tool calls naming a path or identifier that does not exist (§6.6).
    wrong_paths: u32,
    /// Active capsules with activation turns.
    active: Vec<ActiveCapsule>,
    /// The current turn (incremented by [`LoopGuard::end_turn`]).
    turn: u32,
    /// Latched escalation reason, until acknowledged.
    escalation: Option<&'static str>,
}

impl Default for LoopGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Fingerprints kept in the consecutive-run window.
const RECENT_WINDOW: usize = 8;

impl LoopGuard {
    /// A fresh guard at turn 0.
    pub fn new() -> Self {
        Self {
            stagnation: HashMap::new(),
            recent: VecDeque::new(),
            last_validator: HashMap::new(),
            noop_edits: 0,
            patch_failures: std::collections::BTreeMap::new(),
            bad_args: 0,
            failed_calls: std::collections::BTreeMap::new(),
            wrong_paths: 0,
            active: Vec::new(),
            turn: 0,
            escalation: None,
        }
    }

    /// Detector (a)+(b): observes a tool call. `input` is the tool-call JSON;
    /// the fingerprint uses `canonical_json`, so equal arguments spelled
    /// with different key orders are the same call.
    pub fn observe_tool_call(&mut self, name: &str, input: &Value) -> Trip {
        let fingerprint = fingerprint(name, input);
        let calls = {
            let entry = self.stagnation.entry(fingerprint.clone()).or_insert(0);
            *entry += 1;
            *entry
        };
        self.recent.push_back(fingerprint.clone());
        while self.recent.len() > RECENT_WINDOW {
            self.recent.pop_front();
        }
        let consecutive = self
            .recent
            .iter()
            .rev()
            .take_while(|seen| **seen == fingerprint)
            .count();

        let mut trip = Trip::none();
        if calls >= STAGNATION_TRIP {
            trip.capsules.push("repeat_breaker");
        }
        if consecutive >= CONSECUTIVE_TRIP {
            trip.capsules.push("evidence_reuse");
        }
        trip.escalate = calls >= 2 * STAGNATION_TRIP || consecutive >= 2 * CONSECUTIVE_TRIP;
        self.finish(trip)
    }

    /// Detector (c): observes a completed validator run. Two identical
    /// `command|output` runs in a row mean the model is re-verifying
    /// without changing anything.
    pub fn observe_validation(&mut self, command: &str, output: &str) -> Trip {
        let fingerprint = output.trim().to_owned();
        let entry = self
            .last_validator
            .entry(command.to_owned())
            .or_insert_with(|| (String::new(), 0));
        let count = if entry.0 == fingerprint {
            entry.1 + 1
        } else {
            1
        };
        *entry = (fingerprint, count);
        let mut trip = Trip::none();
        if count >= VALIDATOR_REPEAT_TRIP {
            trip.capsules.push("mutation_required");
        }
        trip.escalate = count >= 2 * VALIDATOR_REPEAT_TRIP;
        self.finish(trip)
    }

    /// Detector (d): observes an edit block; REPLACE == SEARCH is a no-op.
    pub fn observe_edit(&mut self, search: &str, replace: &str) -> Trip {
        let mut trip = Trip::none();
        if search == replace {
            self.noop_edits += 1;
            trip.capsules.push("no_op_edit");
            trip.escalate = self.noop_edits >= 2;
        }
        self.finish(trip)
    }

    /// How many identical failures are allowed before a call is refused.
    ///
    /// Three, so the model sees the error and has two chances to act on it
    /// before the door closes. A bar at one would punish a typo.
    const FAILURE_BAR: u32 = 3;

    /// Whether this exact call has already failed three times and must not
    /// be executed again this task (§6.6).
    pub fn is_barred(&self, tool: &str, input: &Value) -> bool {
        self.failed_calls
            .get(&fingerprint(tool, input))
            .is_some_and(|misses| *misses >= Self::FAILURE_BAR)
    }

    /// Records that this exact call failed. Only failures count: a call that
    /// succeeds is never barred however often it repeats, because re-reading
    /// a file after editing it is ordinary.
    pub fn observe_failed_call(&mut self, tool: &str, input: &Value) {
        *self
            .failed_calls
            .entry(fingerprint(tool, input))
            .or_insert(0) += 1;
    }

    /// Records a failed tool call (§6.6), classifying it by the shape of the
    /// error so a *repeated* mistake escalates from advice to an imperative.
    ///
    /// The cue vocabulary already fires a skill card on the first
    /// occurrence; this is the second-occurrence response. SmallCTL's
    /// `detect_bad_tool_args` and `detect_wrong_path` are the references.
    ///
    /// Deliberately not ported: SmallCTL's `detect_tool_output_misread`,
    /// which asks whether the model's next action contradicts the result it
    /// just read. Every formulation of that test reachable from here is a
    /// guess about intent, and a detector that fires on a guess is worse
    /// than none — it spends context telling a model it is wrong when it is
    /// not. The identical-call fingerprint already covers the concrete case.
    pub fn observe_tool_error(&mut self, _tool: &str, message: &str) -> Trip {
        let cues = error_cues(_tool, message);
        let mut trip = Trip::none();
        if message.trim().is_empty() {
            return trip;
        }
        if cues.iter().any(|cue| cue == "bad_tool_args") {
            self.bad_args += 1;
            if self.bad_args >= 2 {
                trip.capsules.push("read_the_error");
                trip.escalate = self.bad_args >= 4;
            }
        }
        if cues.iter().any(|cue| cue == "wrong_path") {
            self.wrong_paths += 1;
            if self.wrong_paths >= 2 {
                trip.capsules.push("confirm_the_name");
                trip.escalate = trip.escalate || self.wrong_paths >= 4;
            }
        }
        self.finish(trip)
    }

    /// Records an edit that failed to apply to `path` (§6.6).
    ///
    /// The second consecutive failure on one file trips
    /// `whole_file_rewrite`. Counted per file because a miss on one file
    /// says nothing about another, and escalating at 2× as every other
    /// detector does.
    pub fn observe_edit_failure(&mut self, path: &str) -> Trip {
        let misses = self.patch_failures.entry(path.to_owned()).or_insert(0);
        *misses += 1;
        let misses = *misses;
        let mut trip = Trip::none();
        if misses >= 2 {
            trip.capsules.push("whole_file_rewrite");
            trip.escalate = misses >= 4;
        }
        self.finish(trip)
    }

    /// Records an edit that applied to `path`: the model recovered, so the
    /// next miss starts from zero rather than inheriting a stale strike.
    pub fn observe_edit_success(&mut self, path: &str) {
        self.patch_failures.remove(path);
    }

    /// Activates every capsule in `trip`, latches escalation, returns `trip`.
    fn finish(&mut self, trip: Trip) -> Trip {
        if trip.escalate && self.escalation.is_none() {
            self.escalation = Some(trip.capsules.first().copied().unwrap_or("repeat_breaker"));
        }
        for name in &trip.capsules {
            self.activate(name);
        }
        trip
    }

    /// Activates (or refreshes) a capsule. Re-tripping refreshes its TTL:
    /// still looping means still mitigating. Over [`MAX_ACTIVE_CAPSULES`]
    /// the lowest-priority capsule is evicted first.
    fn activate(&mut self, name: &&'static str) {
        let Some(found) = capsule(name) else {
            return;
        };
        if let Some(existing) = self
            .active
            .iter_mut()
            .find(|active| active.capsule.name == *name)
        {
            existing.activated = self.turn;
            return;
        }
        if self.active.len() >= MAX_ACTIVE_CAPSULES {
            let victim = self
                .active
                .iter()
                .enumerate()
                .min_by_key(|(index, active)| (active.capsule.priority, *index))
                .map(|(index, _)| index);
            if let Some(victim) = victim {
                self.active.remove(victim);
            }
        }
        self.active.push(ActiveCapsule {
            capsule: found,
            activated: self.turn,
        });
    }

    /// Advances one turn and expires capsules older than
    /// [`CAPSULE_TTL_TURNS`] turns.
    pub fn end_turn(&mut self) {
        self.turn += 1;
        self.active
            .retain(|active| self.turn - active.activated < CAPSULE_TTL_TURNS);
    }

    /// Names of the currently active capsules, highest priority first.
    pub fn active_capsule_names(&self) -> Vec<&'static str> {
        self.sorted_active()
            .iter()
            .map(|active| active.capsule.name)
            .collect()
    }

    /// Active capsules, highest priority first.
    fn sorted_active(&self) -> Vec<&ActiveCapsule> {
        let mut active: Vec<&ActiveCapsule> = self.active.iter().collect();
        active.sort_by(|a, b| {
            b.capsule
                .priority
                .cmp(&a.capsule.priority)
                .then_with(|| a.capsule.name.cmp(b.capsule.name))
        });
        active
    }

    /// The trailing system note carrying the active capsules — `None` when
    /// none are active. Highest priority first, deduplicated, at most
    /// [`MAX_ACTIVE_CAPSULES`] lines, the whole note within
    /// [`CAPSULE_TOKEN_BUDGET`] tokens.
    pub fn capsule_note(&self) -> Option<Message> {
        let mut note = String::from("LOOP MITIGATION:");
        let mut included = 0usize;
        for active in self.sorted_active() {
            if included >= MAX_ACTIVE_CAPSULES {
                break;
            }
            let candidate = format!("{note}\n- {}", active.capsule.text);
            if estimate_tokens(&candidate) > CAPSULE_TOKEN_BUDGET {
                if included > 0 {
                    break; // budget reached: keep what fits
                }
                // A38: `&& included > 0` meant a *first* capsule over budget
                // shipped regardless, so §6.6's ≤ 180-token guarantee had a
                // hole. Skipping it instead would be worse — a loop-mitigation
                // note that silently fails to ship is an inert safeguard,
                // the §16.1 shape. So it ships, clipped, and says so, which
                // is the §6.1 discipline applied here.
                note = clip_to_tokens(&candidate, CAPSULE_TOKEN_BUDGET);
                included += 1;
                break;
            }
            note = candidate;
            included += 1;
        }
        (included > 0).then(|| Message::system(note))
    }
}

/// Clip `text` to `budget` estimated tokens, char-boundary safe, with a
/// marker that is charged against the budget rather than appended past it
/// (§6.1). Used only for the pathological single-capsule case in
/// [`LoopGuard::capsule_note`].
fn clip_to_tokens(text: &str, budget: u64) -> String {
    const MARKER: &str = " […]";
    if estimate_tokens(text) <= budget {
        return text.to_owned();
    }
    let chars = (budget.saturating_sub(estimate_tokens(MARKER)) as usize) * 3;
    let head: String = text.chars().take(chars).collect();
    format!("{head}{MARKER}")
}

impl LoopGuard {
    /// The latched escalation directive, if any and not yet acknowledged
    /// (§6.6): regress `Editing → Planning` and notify the user.
    pub fn escalation(&self) -> Option<Escalation> {
        self.escalation.map(|reason| Escalation {
            reason,
            notify: format!(
                "Loop detected ({reason}); regressing Editing to Planning. Re-draft a short plan before editing again."
            ),
        })
    }

    /// Clears the escalation latch after the caller applied the regression.
    pub fn acknowledge_escalation(&mut self) {
        self.escalation = None;
    }

    /// Starts a new task: every user request earns its own detector state,
    /// mirroring the §6.7 gate's per-request repair bound. The latched
    /// escalation is acknowledged (its regression fired during the previous
    /// task) and stagnation, window, and capsule state is dropped, so a
    /// fresh request is never pre-biased by the previous one. The turn
    /// counter keeps running — it only feeds capsule TTLs, which are empty
    /// now anyway. Within a request the detectors accumulate as usual;
    /// that is where loops actually run.
    pub fn start_task(&mut self) {
        self.acknowledge_escalation();
        self.stagnation.clear();
        self.recent.clear();
        self.last_validator.clear();
        self.noop_edits = 0;
        self.patch_failures.clear();
        self.bad_args = 0;
        self.failed_calls.clear();
        self.wrong_paths = 0;
        self.active.clear();
    }
}

/// Canonical error-kind cues (§6.6: "triggers: [tool names, **error kinds**,
/// keywords]"). Recovery cards trigger on these, so the vocabulary is
/// derived from observation text in one place — otherwise a card declares a
/// trigger no code path ever emits, which is exactly what shipped in v1.
pub fn error_cues(tool: &str, content: &str) -> Vec<String> {
    let lower = content.to_lowercase();
    let has = |needle: &str| lower.contains(needle);
    let mut cues = vec!["error".to_owned()];
    let mut add = |cue: &str| cues.push(cue.to_owned());

    // Tool-independent shapes first (round 10). Each names a failure a real
    // model produced (§16.9); the vocabulary, not the deck size, was what
    // limited how much any set of cards could help.
    if has("must be a positive integer")
        || has("must be a non-empty")
        || has("must satisfy")
        || has("takes either")
        || has("must be at least")
        || has("must be ≥")
    {
        add("bad_tool_args");
    }
    if has("no such file") || has("no definition named") || has("resolves outside") {
        add("wrong_path");
    }
    if has("past the end of the file") {
        add("past_eof");
    }

    match tool {
        "edit" | "write" => {
            add("edit_failed");
            if has("failed to exactly match") || has("no such file") {
                add("not_found");
            }
            if has("failed to match") {
                // Distinct from `not_found`, which also covers a missing
                // file: this is the file existing and the SEARCH text not.
                // It is the cue a whole-file rewrite should answer.
                add("patch_target_not_found");
            }
            if has("did you mean") || has("already in") {
                add("duplicate_match");
            }
        }
        "validation" => {
            add("validation_failed");
            if has("test result:") || has("test failed") || has("panicked") || has("0 tests") {
                add("test_failure");
            }
        }
        "read" if has("no such file") => add("not_found"),
        _ => {}
    }
    cues.sort_unstable();
    cues.dedup();
    cues
}

/// The `tool|args` fingerprint two calls share when they are the same call.
///
/// One definition, used by both the stagnation detector and the failure bar,
/// so the two can never disagree about what "the same call" means.
fn fingerprint(tool: &str, input: &Value) -> String {
    format!("{tool}|{}", canonical_json(input))
}

/// Deterministic serialization of a tool-call JSON value: object keys are
/// sorted at every depth, arrays keep their order. `serde_json`'s own map
/// ordering depends on workspace features (`preserve_order`), so
/// fingerprints never rely on it — the same arguments are the same call
/// regardless of how the model spelled them.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<String> = map
                .iter()
                .map(|(key, value)| format!("{key:?}:{}", canonical_json(value)))
                .collect();
            entries.sort_unstable();
            format!("{{{}}}", entries.join(","))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", items.join(","))
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod round8_capsule_budget {
    use super::*;

    /// A38: the budget guard was `> CAPSULE_TOKEN_BUDGET && included > 0`,
    /// so a *first* capsule over 180 tokens shipped regardless and §6.6's
    /// guarantee had a hole. Latent — the shipped deck's longest text is
    /// ~25 tokens — which is exactly why the existing test could not see it:
    /// it exercised only the shipped set.
    ///
    /// Skipping an oversized capsule instead of clipping it would trade this
    /// for a worse defect: a loop-mitigation note that silently fails to
    /// ship is an inert safeguard (§16.1).
    #[test]
    fn an_oversized_first_capsule_is_clipped_not_shipped_over_budget() {
        let long = "X".repeat(CAPSULE_TOKEN_BUDGET as usize * 3 * 4);
        let clipped = clip_to_tokens(&format!("LOOP MITIGATION:\n- {long}"), CAPSULE_TOKEN_BUDGET);
        assert!(
            estimate_tokens(&clipped) <= CAPSULE_TOKEN_BUDGET,
            "a clipped capsule note is {} tokens against a {CAPSULE_TOKEN_BUDGET} cap",
            estimate_tokens(&clipped)
        );
        assert!(
            clipped.ends_with("[…]"),
            "and says it was clipped: {clipped}"
        );
        // Under budget, untouched.
        let small = "LOOP MITIGATION:\n- emit one edit block";
        assert_eq!(clip_to_tokens(small, CAPSULE_TOKEN_BUDGET), small);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;
    use rusta_llm::Role;
    use serde_json::json;

    /// The shipped deck, straight from the versioned `skills/` directory.
    fn shipped_deck() -> CardDeck {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills");
        CardDeck::load(&dir).expect("shipped skills load")
    }

    fn messages(parts: &[(Role, &str)]) -> Vec<Message> {
        parts
            .iter()
            .map(|&(role, content)| Message::new(role, content))
            .collect()
    }

    // -- skill cards --------------------------------------------------------

    #[test]
    fn shipped_cards_fit_the_token_budget_invariant() {
        let deck = shipped_deck();
        // Round 10 grew the deck from 4 to 9 against the reference decks
        // (little-coder ships 31). Cards are data and uncounted by §12, so
        // the only ceiling is the ≤ 120-token budget asserted below and the
        // ≤ 2-cards-per-note injection rule.
        assert_eq!(deck.cards().len(), 9, "the shipped starter deck");
        for card in deck.cards() {
            assert!(
                card.declared_cost() <= CARD_TOKEN_BUDGET,
                "{}: declared {} over {}",
                card.name(),
                card.declared_cost(),
                CARD_TOKEN_BUDGET
            );
            assert!(
                card.token_cost() <= CARD_TOKEN_BUDGET,
                "{}: body estimates {} tokens over {}",
                card.name(),
                card.token_cost(),
                CARD_TOKEN_BUDGET
            );
        }
    }

    #[test]
    fn trigger_matrix_fires_exactly_the_matching_cards() {
        let deck = shipped_deck();
        let matrix: &[(&str, &[&str])] = &[
            ("read", &["read-large-files"]),
            ("re-read src/lib.rs to be sure", &["read-large-files"]),
            ("spreadsheet", &[]), // word boundary: "read" is not in "spread"
            ("edit_failed", &["edit-recovery"]),
            ("duplicate_match", &["edit-recovery"]),
            ("write", &["write-vs-edit"]),
            ("validation_failed", &["verify-focus"]),
            ("shell", &[]),
            ("map_drill", &[]),
            // Round 10 cues, each named after a failure a real model
            // produced (§16.9). Order within a cue is priority-then-name.
            ("not_found", &["edit-recovery", "find-the-real-name"]),
            ("wrong_path", &["find-the-real-name"]),
            (
                "patch_target_not_found",
                &["locate-the-cause", "search-must-be-verbatim"],
            ),
            ("bad_tool_args", &["tool-arguments"]),
            ("past_eof", &["tool-arguments"]),
            ("test_failure", &["locate-the-cause", "verify-focus"]),
            ("fix the failing test", &["task-decomposition"]),
            ("refactor this module", &["task-decomposition"]),
            // Word boundaries still hold for the new keyword triggers.
            ("prefix suffix", &[]),
        ];
        for (cue, expected) in matrix {
            let names: Vec<&str> = deck.select(&[cue]).iter().map(|card| card.name()).collect();
            assert_eq!(&names, expected, "cue {cue:?}");
        }

        // Several cues at once: at most two cards, best priority first.
        let hot: Vec<&str> = deck
            .select(&["edit_failed", "read", "write", "validation_failed"])
            .iter()
            .map(|card| card.name())
            .collect();
        assert_eq!(hot, vec!["edit-recovery", "verify-focus"]);
        let pair: Vec<&str> = deck
            .select(&["read", "write"])
            .iter()
            .map(|card| card.name())
            .collect();
        assert_eq!(pair, vec!["write-vs-edit", "read-large-files"]);
    }

    #[test]
    fn skill_note_renders_at_most_two_bodies_as_a_system_note() {
        let deck = shipped_deck();
        assert!(deck.skill_note(&["shell"]).is_none());

        let note = deck.skill_note(&["read", "write"]).expect("fires");
        assert_eq!(note.role, Role::System);
        let read = deck
            .cards()
            .iter()
            .find(|card| card.name() == "read-large-files")
            .expect("card");
        let write = deck
            .cards()
            .iter()
            .find(|card| card.name() == "write-vs-edit")
            .expect("card");
        assert_eq!(
            note.content,
            format!("SKILL NOTE:\n{}\n---\n{}", write.body(), read.body())
        );
    }

    #[test]
    fn frontmatter_parsing_is_strict() {
        let good = "---\nname: x\ntype: tool\ntriggers: [read]\npriority: 5\n\
                    token_cost: 80\nuser-invocable: true\n---\nDo the thing.";
        let card = SkillCard::parse(good).expect("parses");
        assert_eq!(card.name(), "x");
        assert_eq!(card.kind(), CardKind::Tool);
        assert_eq!(card.triggers(), ["read"]);
        assert_eq!(card.priority(), 5);
        assert!(card.user_invocable());
        assert_eq!(card.body(), "Do the thing.");

        let bad: &[(&str, &str)] = &[
            ("no delimiters", "name: x"),
            (
                "missing key",
                "---\nname: x\ntype: tool\ntriggers: []\npriority: 5\n\
                 token_cost: 80\n---\nbody",
            ),
            (
                "unknown key",
                "---\nname: x\ntype: tool\ntriggers: []\npriority: 5\n\
                 token_cost: 80\nuser-invocable: false\nextra: 1\n---\nbody",
            ),
            (
                "priority out of range",
                "---\nname: x\ntype: tool\ntriggers: []\npriority: 10\n\
                 token_cost: 80\nuser-invocable: false\n---\nbody",
            ),
            (
                "bad type",
                "---\nname: x\ntype: magic\ntriggers: []\npriority: 5\n\
                 token_cost: 80\nuser-invocable: false\n---\nbody",
            ),
            (
                "bad triggers",
                "---\nname: x\ntype: tool\ntriggers: read\npriority: 5\n\
                 token_cost: 80\nuser-invocable: false\n---\nbody",
            ),
            (
                "declared cost over budget",
                "---\nname: x\ntype: tool\ntriggers: []\npriority: 5\n\
                 token_cost: 121\nuser-invocable: false\n---\nbody",
            ),
            (
                "empty body",
                "---\nname: x\ntype: tool\ntriggers: []\npriority: 5\n\
                 token_cost: 80\nuser-invocable: false\n---",
            ),
        ];
        for (what, text) in bad {
            assert!(SkillCard::parse(text).is_err(), "{what} must be rejected");
        }
        let oversized = format!(
            "---\nname: x\ntype: tool\ntriggers: []\npriority: 5\n\
             token_cost: 80\nuser-invocable: false\n---\n{}",
            "way over the one hundred twenty token budget. ".repeat(10)
        );
        assert!(SkillCard::parse(&oversized).is_err(), "oversized body");

        // A closing marker at end of input (no trailing newline) must not
        // panic; it yields the empty body, which validation then rejects.
        assert!(SkillCard::parse("---\nname: x\n---").is_err());
    }

    #[test]
    fn user_invocable_gate_for_slash_skills() {
        let deck = shipped_deck();
        // All shipped cards are trigger-injected, not user-invocable.
        assert!(deck.invocable("edit-recovery").is_none());
        let invocable = SkillCard::parse(
            "---\nname: hint\ntype: knowledge\ntriggers: [never]\npriority: 1\n\
             token_cost: 10\nuser-invocable: true\n---\nA hint.",
        )
        .expect("parses");
        let deck = CardDeck::from_cards(vec![invocable]);
        assert_eq!(deck.invocable("HINT").expect("reachable").name(), "hint");
    }

    // -- history compression ------------------------------------------------

    /// Six turns (a turn = one model completion): a request with a read, an
    /// edit block plus error exchange, a grep, then three filler turns.
    fn long_history() -> Vec<Message> {
        let edit_block =
            "src/lib.rs\n<<<<<<< SEARCH\nfn old() {}\n=======\nfn new() {}\n>>>>>>> REPLACE";
        let error_observation = "TOOL RESULT edit (error)\nsearch text not found in src/lib.rs";
        messages(&[
            (Role::User, "fix the bug in src/lib.rs"),
            (Role::Assistant, "on it"),
            (Role::User, "TOOL RESULT read (ok)\n1 fn old() {}"),
            (Role::Assistant, edit_block),
            (Role::User, error_observation),
            (Role::Assistant, "let me grep around"),
            (
                Role::User,
                "TOOL RESULT grep (ok)\nsrc/lib.rs:1: fn old() {}",
            ),
            (Role::Assistant, "turn five"),
            (Role::User, "TOOL RESULT read (ok)\n1 fn old() {}"),
            (Role::Assistant, "turn six"),
            (Role::User, "TOOL RESULT read (ok)\n1 fn old() {}"),
            (Role::Assistant, "turn seven"),
            (Role::User, "TOOL RESULT read (ok)\n1 fn old() {}"),
        ])
    }

    #[test]
    fn compression_trips_only_past_sixty_percent_of_the_window() {
        let compressor = Compressor::new(1_000);
        assert_eq!(compressor.history_budget(), 600);

        // Comfortably under: untouched.
        let calm = Compressor::new(100_000).compress(long_history(), 0);
        assert!(calm.summary.is_none());
        assert_eq!(calm.messages.len(), 13);

        // Over: the oldest turns (6 - 3) collapse into the summary.
        let tight = Compressor::new(150).compress(long_history(), 0);
        let summary = tight.summary.expect("compresses");
        assert_eq!(summary.covers_turns, 3);
    }

    #[test]
    fn compression_keeps_edit_blocks_errors_and_last_turns_verbatim() {
        let compression = Compressor::new(150).compress(long_history(), 0);
        let out = &compression.messages;

        // The summary heads the compressed history…
        assert_eq!(out[0].role, Role::Assistant);
        assert!(out[0].content.starts_with("Summary of earlier turns:\n"));
        let summary = &compression.summary.expect("ran").text;
        assert!(
            summary.starts_with("1. user: fix the bug in src/lib.rs"),
            "{summary}"
        );
        assert!(summary.contains("edit src/lib.rs"), "{summary}");
        assert!(summary.contains("edit!"), "{summary}"); // the failed edit

        // …the edit block and the error feedback survive verbatim, in order
        // after the summary…
        assert!(out.contains(&Message::assistant(
            "src/lib.rs\n<<<<<<< SEARCH\nfn old() {}\n=======\nfn new() {}\n>>>>>>> REPLACE"
        )));
        assert!(out.contains(&Message::user(
            "TOOL RESULT edit (error)\nsearch text not found in src/lib.rs"
        )));

        // …and the last three turns are untouched, byte-for-byte.
        assert_eq!(
            &out[out.len() - 6..],
            &messages(&[
                (Role::Assistant, "turn five"),
                (Role::User, "TOOL RESULT read (ok)\n1 fn old() {}"),
                (Role::Assistant, "turn six"),
                (Role::User, "TOOL RESULT read (ok)\n1 fn old() {}"),
                (Role::Assistant, "turn seven"),
                (Role::User, "TOOL RESULT read (ok)\n1 fn old() {}"),
            ])[..]
        );

        // The summarized-but-not-kept material is gone: the request text and
        // the successful early read only live in the digest now.
        assert!(!out.iter().any(|m| m.content == "fix the bug in src/lib.rs"));
        assert!(
            !out.iter()
                .any(|m| m.content.contains("TOOL RESULT grep (ok)")),
            "the old successful grep is summarized away"
        );
    }

    #[test]
    fn summary_carries_the_previous_episodic_summary() {
        // First compression over a 7-turn history.
        let first = Compressor::new(150).compress(long_history(), 0);
        let first_summary = first.summary.expect("first").text;

        // Grow the tail and compress again: the first summary now heads the
        // dropped range and its lines must be carried into the new summary.
        let mut grown = first.messages.clone();
        grown.push(Message::assistant("turn eight"));
        grown.push(Message::user("TOOL RESULT read (ok)\n1 fn old() {}"));
        let second = Compressor::new(120).compress(grown, 0);
        let second_summary = second.summary.expect("second").text;
        assert!(
            second_summary.contains(&first_summary),
            "carried:\n{second_summary}\nfirst:\n{first_summary}"
        );
    }

    #[test]
    fn replay_reproduces_live_compression_byte_for_byte() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        let history = long_history();
        {
            let mut session = crate::session::Session::open(&path).expect("open");
            for message in &history {
                match message.role {
                    Role::User => session
                        .record(crate::session::Event::UserMessage {
                            content: message.content.clone(),
                        })
                        .expect("record"),
                    Role::Assistant => session
                        .record(crate::session::Event::AssistantMessage {
                            content: message.content.clone(),
                        })
                        .expect("record"),
                    Role::System => unreachable!("history carries no system messages"),
                };
            }
            let live = Compressor::new(150).compress(history.clone(), 0);
            let summary = live.summary.expect("compresses");
            session
                .record(crate::session::Event::Summary {
                    covers_turns: summary.covers_turns,
                    text: summary.text.clone(),
                })
                .expect("record summary");
        }
        let session = crate::session::Session::open(&path).expect("reopen");
        let context = session.replay_context().expect("replay");
        let live = Compressor::new(150).compress(history, 0);
        assert_eq!(context.messages, live.messages);
    }

    // -- FAMA-lite ----------------------------------------------------------

    #[test]
    fn stagnation_detector_trips_repeat_breaker() {
        let mut guard = LoopGuard::new();
        let read = json!({"path": "src/lib.rs"});
        let grep = json!({"pattern": "old"});
        // A, B, A, B, A: the same call recurs, never consecutively.
        let calls = [
            ("read", &read),
            ("grep", &grep),
            ("read", &read),
            ("grep", &grep),
            ("read", &read),
        ];
        for (name, input) in calls {
            let trip = guard.observe_tool_call(name, input);
            if name == "read"
                && guard
                    .stagnation
                    .values()
                    .any(|&count| count == STAGNATION_TRIP)
            {
                assert_eq!(trip.capsules, ["repeat_breaker"], "{trip:?}");
                assert!(!trip.escalate);
                break;
            }
            assert!(trip.capsules.is_empty(), "{trip:?}");
        }
        let note = guard.capsule_note().expect("mitigating");
        assert_eq!(note.role, Role::System);
        assert!(
            note.content
                .contains("Do not repeat the same tool call unchanged"),
            "{}",
            note.content
        );
    }

    #[test]
    fn consecutive_detector_trips_evidence_reuse() {
        let mut guard = LoopGuard::new();
        let input = json!({"path": "src/lib.rs"});
        for _ in 1..CONSECUTIVE_TRIP {
            let trip = guard.observe_tool_call("read", &input);
            assert!(trip.capsules.is_empty(), "{trip:?}");
        }
        // The third identical call also makes the stagnation count three,
        // so both repeat detectors fire — evidence_reuse among them.
        let trip = guard.observe_tool_call("read", &input);
        assert!(trip.capsules.contains(&"evidence_reuse"), "{trip:?}");
        let note = guard.capsule_note().expect("mitigating");
        assert!(
            note.content.contains("Use the evidence already in context"),
            "{}",
            note.content
        );
    }

    #[test]
    fn validator_detector_trips_mutation_required() {
        let mut guard = LoopGuard::new();
        assert!(
            guard
                .observe_validation("cargo test", "error[E0308]: mismatched types")
                .capsules
                .is_empty()
        );
        let trip = guard.observe_validation("cargo test", "error[E0308]: mismatched types");
        assert_eq!(trip.capsules, ["mutation_required"], "{trip:?}");
        assert!(!trip.escalate);
        let note = guard.capsule_note().expect("mitigating");
        assert!(
            note.content
                .contains("MUTATION REQUIRED: You have read enough. Emit ONE edit block"),
            "{}",
            note.content
        );
        // A different output resets the run.
        assert!(
            guard
                .observe_validation("cargo test", "test result: ok")
                .capsules
                .is_empty()
        );
    }

    #[test]
    fn noop_edit_detector_trips_no_op_capsule() {
        let mut guard = LoopGuard::new();
        assert!(guard.observe_edit("old", "new").capsules.is_empty());
        let trip = guard.observe_edit("same", "same");
        assert_eq!(trip.capsules, ["no_op_edit"], "{trip:?}");
        assert!(!trip.escalate);
        let note = guard.capsule_note().expect("mitigating");
        assert!(
            note.content.contains("REPLACE equals SEARCH"),
            "{}",
            note.content
        );
    }

    #[test]
    fn escalation_latches_at_double_threshold_and_regresses() {
        let mut guard = LoopGuard::new();
        let input = json!({"path": "src/lib.rs"});
        for _ in 0..(2 * STAGNATION_TRIP) {
            if guard.observe_tool_call("read", &input).escalate {
                break;
            }
        }
        let escalation = guard.escalation().expect("latched");
        assert_eq!(escalation.reason, "repeat_breaker");
        assert!(
            escalation.notify.contains("Editing to Planning"),
            "{}",
            escalation.notify
        );
        // The regression itself is the machine's `LoopEscalated` edge
        // (§6.4 table) so that it journals; the guard only latches the
        // reason and the user notification.
        let mut machine = crate::state::Machine::resume_at(State::Editing);
        let transition = machine
            .fire(crate::state::PhaseEvent::LoopEscalated)
            .expect("legal in Editing")
            .expect("state change");
        assert_eq!(transition.to, State::Planning);
        guard.acknowledge_escalation();
        assert!(guard.escalation().is_none());

        // The validator detector escalates at its own double threshold.
        let mut guard = LoopGuard::new();
        for _ in 0..(2 * VALIDATOR_REPEAT_TRIP) {
            if guard
                .observe_validation("cargo test", "same failure")
                .escalate
            {
                break;
            }
        }
        assert_eq!(
            guard.escalation().expect("latched").reason,
            "mutation_required"
        );
    }

    #[test]
    fn start_task_gives_every_request_fresh_detector_state() {
        // Latch an escalation and accumulate detector + capsule state.
        let mut guard = LoopGuard::new();
        let input = json!({"path": "src/lib.rs"});
        for _ in 0..(2 * STAGNATION_TRIP) {
            if guard.observe_tool_call("read", &input).escalate {
                break;
            }
        }
        assert!(guard.escalation().is_some());
        assert!(!guard.active_capsule_names().is_empty());

        // A new user request starts clean: no latch, no capsules, and the
        // once-saturated fingerprint no longer trips on its first call.
        guard.start_task();
        assert!(guard.escalation().is_none());
        assert!(guard.active_capsule_names().is_empty());
        let trip = guard.observe_tool_call("read", &input);
        assert!(trip.capsules.is_empty(), "{trip:?}");
        assert!(!trip.escalate);
    }

    #[test]
    fn capsules_expire_after_three_turns() {
        let mut guard = LoopGuard::new();
        guard.observe_edit("same", "same");
        assert!(guard.capsule_note().is_some());
        guard.end_turn();
        assert!(guard.capsule_note().is_some(), "still turn 2 of 3");
        guard.end_turn();
        assert!(guard.capsule_note().is_some(), "still turn 3 of 3");
        guard.end_turn();
        assert!(guard.capsule_note().is_none(), "expired after 3 turns");
        assert!(guard.active_capsule_names().is_empty());
    }

    #[test]
    fn capsule_note_is_deduplicated_and_within_budget() {
        let mut guard = LoopGuard::new();
        let input = json!({"path": "src/lib.rs"});
        for _ in 0..CONSECUTIVE_TRIP {
            guard.observe_tool_call("read", &input);
        }
        for _ in 0..VALIDATOR_REPEAT_TRIP {
            guard.observe_validation("cargo test", "same failure");
        }
        guard.observe_edit("same", "same");
        guard.observe_edit("same", "same"); // re-trip: dedup, but escalate
        assert!(guard.escalation().is_some());

        let names = guard.active_capsule_names();
        assert!(names.len() <= MAX_ACTIVE_CAPSULES, "{names:?}");
        assert_eq!(
            names,
            vec![
                "mutation_required",
                "repeat_breaker",
                "evidence_reuse",
                "no_op_edit"
            ]
        );
        let note = guard.capsule_note().expect("mitigating");
        let lines: Vec<&str> = note.content.lines().collect();
        let mut unique = lines.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), lines.len(), "no duplicated capsule lines");
        assert!(estimate_tokens(&note.content) <= CAPSULE_TOKEN_BUDGET);
    }

    #[test]
    fn tool_fingerprints_are_stable_across_key_order() {
        let mut guard = LoopGuard::new();
        // serde_json's own ordering depends on workspace features; canonical
        // fingerprints must not. Two calls, one per spelling: both read as
        // the *second* call of one identity — no detector may trip at 2.
        let first = json!({"to": 4, "from": 1, "path": "src/lib.rs"});
        let second = json!({"path": "src/lib.rs", "from": 1, "to": 4});
        assert!(guard.observe_tool_call("read", &first).capsules.is_empty());
        assert!(
            guard.observe_tool_call("read", &second).capsules.is_empty(),
            "different key order must not look like a different call"
        );
        assert_eq!(guard.stagnation.len(), 1, "one identity, one fingerprint");
    }
}
