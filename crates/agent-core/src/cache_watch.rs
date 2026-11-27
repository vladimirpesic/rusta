//! A watch on the provider's prompt cache, which says when it stops working.
//!
//! A conversation's requests share their beginning: the system prompt, the
//! tools and every earlier message go again, byte for byte, with something
//! new at the end. The provider reads that beginning from its cache, at a
//! small part of the input price. Something that changes it on every request
//! — a date in the system prompt, tools in another order, a message rewritten
//! — makes every request a full-price one, and nothing else shows it.
//!
//! The provider's own figures do: a request whose cache read falls far short
//! of the request before it, which the cache should hold, missed. Three such
//! requests in a row are a pattern rather than chance, and the watch says so
//! once, until a request reads from the cache again. Requests it cannot judge
//! are passed over: the first of a conversation, a small one, one after a
//! pause long enough for the cache to expire, one smaller than the request
//! before it (a compaction or an undo started it afresh), and those of a
//! provider that reports no cache figures.

use std::time::{Duration, Instant};

use lm_bridge::usage::TokenUsage;

/// Requests smaller than this are not judged: a miss costs little, and some
/// providers do not cache short prompts at all.
const MIN_PROMPT_TOKENS: u64 = 16_384;
/// A request after a pause this long may have outlived the cache, whose
/// shortest life among Rusta's providers is five minutes.
const CACHE_LIFE: Duration = Duration::from_secs(5 * 60);
/// Misses in a row that make the pattern.
const MISSES: u32 = 3;

/// The watch's word that the cache keeps missing.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheNotice {
    /// The requests in a row that missed.
    pub requests: u32,
    /// The share of the last one's prompt read from the cache, 0 to 1.
    pub read_share: f64,
    /// The last one's prompt, in tokens.
    pub prompt_tokens: u64,
}

impl CacheNotice {
    /// One line for the front-ends.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "the provider's prompt cache missed {} requests in a row: the last read {:.0}% of \
             its {} tokens from the cache, where most of the conversation before it should have \
             been cached, so each is billed nearly in full",
            self.requests,
            self.read_share * 100.0,
            self.prompt_tokens
        )
    }
}

/// The last request the watch saw.
#[derive(Debug, Clone, Copy)]
struct Seen {
    prompt: u64,
    estimate: u64,
    at: Instant,
}

/// The state of the watch over one conversation's requests.
#[derive(Debug, Default, Clone)]
pub struct CacheWatch {
    last: Option<Seen>,
    misses: u32,
    told: bool,
}

impl CacheWatch {
    /// Judge a request that reported `usage`, sent with Rusta's estimate of
    /// its size `estimate`: the notice, when it makes [`MISSES`] in a row and
    /// none has been given since the cache last worked.
    pub fn observe(&mut self, usage: &TokenUsage, estimate: u64) -> Option<CacheNotice> {
        self.observe_at(usage, estimate, Instant::now())
    }

    fn observe_at(
        &mut self,
        usage: &TokenUsage,
        estimate: u64,
        now: Instant,
    ) -> Option<CacheNotice> {
        let Some(read) = usage.input_cache_read else {
            self.last = None;
            return None;
        };
        let prompt = usage.input_tokens;
        let before = self.last.replace(Seen {
            prompt,
            estimate,
            at: now,
        });
        let before = before.filter(|before| {
            prompt >= MIN_PROMPT_TOKENS
                && estimate >= before.estimate
                && now.duration_since(before.at) < CACHE_LIFE
        })?;
        if read.saturating_mul(2) >= before.prompt {
            self.misses = 0;
            self.told = false;
            return None;
        }
        self.misses += 1;
        if self.misses < MISSES || self.told {
            return None;
        }
        self.told = true;
        #[allow(clippy::cast_precision_loss)]
        let read_share = read as f64 / prompt.max(1) as f64;
        Some(CacheNotice {
            requests: self.misses,
            read_share,
            prompt_tokens: prompt,
        })
    }

    /// Start over: the requests from now on share no beginning with those
    /// before, as after a switch to another model.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: u64, read: Option<u64>) -> TokenUsage {
        TokenUsage {
            input_tokens: prompt,
            input_cache_read: read,
            ..TokenUsage::empty()
        }
    }

    /// Requests a minute apart, each a little larger than the last.
    fn requests(watch: &mut CacheWatch, reads: &[Option<u64>]) -> Vec<Option<CacheNotice>> {
        let start = Instant::now();
        reads
            .iter()
            .enumerate()
            .map(|(i, read)| {
                let prompt = 40_000 + 1_000 * i as u64;
                watch.observe_at(
                    &usage(prompt, *read),
                    prompt,
                    start + Duration::from_secs(60 * i as u64),
                )
            })
            .collect()
    }

    #[test]
    fn a_cache_that_works_is_never_mentioned() {
        let mut watch = CacheWatch::default();
        let told = requests(
            &mut watch,
            &[Some(0), Some(39_000), Some(40_500), Some(41_800)],
        );
        assert!(told.iter().all(Option::is_none));
    }

    #[test]
    fn three_misses_in_a_row_are_told_once() {
        let mut watch = CacheWatch::default();
        let told = requests(
            &mut watch,
            &[Some(0), Some(1_000), Some(0), Some(500), Some(0), Some(0)],
        );
        let notices: Vec<_> = told.iter().flatten().collect();
        assert_eq!(notices.len(), 1, "{told:?}");
        assert_eq!(told[3].as_ref().unwrap().requests, 3);
        assert!(
            told[3]
                .as_ref()
                .unwrap()
                .describe()
                .contains("missed 3 requests in a row")
        );
    }

    #[test]
    fn a_hit_between_misses_starts_the_count_again_and_rearms_the_notice() {
        let mut watch = CacheWatch::default();
        let told = requests(
            &mut watch,
            &[
                Some(0),
                Some(0),
                Some(0),
                Some(42_000),
                Some(0),
                Some(0),
                Some(0),
                Some(0),
            ],
        );
        assert!(told[..5].iter().all(Option::is_none), "{told:?}");
        assert!(told[6].is_some(), "{told:?}");
    }

    #[test]
    fn requests_it_cannot_judge_are_passed_over() {
        let mut watch = CacheWatch::default();
        let start = Instant::now();
        let at = |minutes: u64| start + Duration::from_secs(60 * minutes);
        // Without cache figures, small, after a pause, or smaller than the
        // one before: none counts as a miss.
        assert_eq!(
            watch.observe_at(&usage(40_000, Some(0)), 40_000, at(0)),
            None
        );
        assert_eq!(watch.observe_at(&usage(41_000, None), 41_000, at(1)), None);
        assert_eq!(
            watch.observe_at(&usage(42_000, Some(0)), 42_000, at(2)),
            None
        );
        assert_eq!(
            watch.observe_at(&usage(8_000, Some(0)), 43_000, at(3)),
            None
        );
        assert_eq!(
            watch.observe_at(&usage(44_000, Some(0)), 44_000, at(20)),
            None
        );
        assert_eq!(
            watch.observe_at(&usage(30_000, Some(0)), 20_000, at(21)),
            None
        );
        assert_eq!(watch.misses, 0);
        watch.reset();
        assert!(watch.last.is_none());
    }
}
