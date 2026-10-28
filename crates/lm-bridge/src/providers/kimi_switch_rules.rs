//! Kimi's rules for confirming a model switch: the `model_switch_confirm`
//! client config of the Kimi Code endpoints (`POST {base}/client_configs`).
//!
//! Kimi advises against switching to K3.1 inside a session begun on another
//! model — it lowers output quality and cache hits — and K3.1 Pro Highspeed
//! costs about twice K3.1 Pro's quota. The rules say which switches to
//! confirm, and with what words; Kimi changes them without a release.

use serde::Deserialize;
use serde_json::Value;

/// Whether the conversation has begun: an `old` session has at least one
/// user message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    New,
    Old,
}

/// The models a rule names: one id, `*` for any, or a list of ids.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum ModelIds {
    One(String),
    Many(Vec<String>),
}

impl ModelIds {
    /// Ids match exactly; `*` matches any, but only as the whole field.
    fn matches(&self, id: &str) -> bool {
        match self {
            Self::One(one) => one == "*" || one == id,
            Self::Many(many) => many.iter().any(|m| m == id),
        }
    }
}

/// Which sessions a rule is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionType {
    Old,
    New,
    All,
}

/// A string Kimi serves per locale; Rusta reads the English one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Localized {
    en: Option<String>,
}

/// One rule: a switch it applies to, and what to tell the user.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ModelSwitchRule {
    from: ModelIds,
    to: ModelIds,
    session_type: SessionType,
    hint: Option<Localized>,
    warning: Option<Localized>,
}

impl ModelSwitchRule {
    /// Why the switch is worth a second thought.
    #[must_use]
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_ref()?.en.as_deref()
    }

    /// What the switch will cost, shown apart from the hint.
    #[must_use]
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_ref()?.en.as_deref()
    }

    fn applies(&self, from: &str, to: &str, session: SessionKind) -> bool {
        let session_matches = match self.session_type {
            SessionType::All => true,
            SessionType::Old => session == SessionKind::Old,
            SessionType::New => session == SessionKind::New,
        };
        session_matches && self.from.matches(from) && self.to.matches(to)
    }
}

/// The `model_switch_confirm` config: whether it is on, and its rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelSwitchRules {
    enable: bool,
    rules: Vec<ModelSwitchRule>,
}

impl ModelSwitchRules {
    /// Read the config's `config` object. A rule that does not parse is skipped
    /// rather than failing the rest, a missing or malformed `enable` is off,
    /// and a list of ids must name at least one.
    #[must_use]
    pub fn from_config(config: &Value) -> Self {
        let rules = config
            .get("rules")
            .and_then(Value::as_array)
            .map(|rules| {
                rules
                    .iter()
                    .filter_map(|rule| serde_json::from_value::<ModelSwitchRule>(rule.clone()).ok())
                    .filter(|rule| {
                        [&rule.from, &rule.to]
                            .iter()
                            .all(|ids| !matches!(ids, ModelIds::Many(many) if many.is_empty()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            enable: config
                .get("enable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            rules,
        }
    }

    /// The first rule for switching from `from` to `to` in a `session`, if
    /// the config is on.
    #[must_use]
    pub fn matching(&self, from: &str, to: &str, session: SessionKind) -> Option<&ModelSwitchRule> {
        if !self.enable {
            return None;
        }
        self.rules
            .iter()
            .find(|rule| rule.applies(from, to, session))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The config as Kimi Code serves it, on both hosts.
    fn live() -> Value {
        json!({
            "rules": [
                {"to": ["k3.1-pro-highspeed"], "from": "*", "session_type": "new",
                 "hint": {"en": "Note: K3.1 Pro Highspeed consumption is approximately 2x that of K3.1 Pro", "zh": "注意"}},
                {"to": ["k3.1-pro-highspeed"], "from": "*", "session_type": "old",
                 "hint": {"en": "Switching to K3.1 Pro Highspeed in this session may affect output quality. Choose how to continue"},
                 "warning": {"en": "Note: K3.1 Pro Highspeed consumption is approximately 2x that of K3.1 Pro"}},
                {"to": ["k3.1-pro", "k3.1-pro-highspeed"],
                 "from": ["k3", "k3-256k", "kimi-for-coding", "kimi-for-coding-highspeed"],
                 "session_type": "old",
                 "hint": {"en": "Switching to a K3.1 series model in this session may affect output quality. Choose how to continue"}}
            ],
            "enable": true
        })
    }

    #[test]
    fn the_live_rules_pick_the_first_that_applies() {
        use SessionKind::{New, Old};
        let rules = ModelSwitchRules::from_config(&live());

        let rule = rules.matching("kimi-for-coding", "k3.1-pro", Old).unwrap();
        assert!(
            rule.hint()
                .unwrap()
                .starts_with("Switching to a K3.1 series model")
        );
        assert_eq!(rule.warning(), None);

        let rule = rules
            .matching("kimi-for-coding", "k3.1-pro-highspeed", Old)
            .unwrap();
        assert!(
            rule.hint()
                .unwrap()
                .starts_with("Switching to K3.1 Pro Highspeed")
        );
        assert!(rule.warning().unwrap().contains("approximately 2x"));

        let rule = rules
            .matching("k3.1-pro", "k3.1-pro-highspeed", New)
            .unwrap();
        assert!(rule.hint().unwrap().starts_with("Note: K3.1 Pro Highspeed"));

        assert!(rules.matching("kimi-for-coding", "k3.1-pro", New).is_none());
        assert!(rules.matching("k3.1-pro", "k3", Old).is_none());
        assert!(
            rules
                .matching("k3.1-pro-highspeed", "k3.1-pro", Old)
                .is_none()
        );
    }

    /// Lenient parsing: a bad rule is skipped, not fatal; `enable`
    /// missing or not a boolean is off; `*` matches only as the whole field;
    /// `all` covers both kinds of session.
    #[test]
    fn the_rules_are_read_leniently() {
        use SessionKind::{New, Old};
        let config = json!({
            "enable": true,
            "rules": [
                {"to": "k9", "from": [], "session_type": "all"},
                {"to": "k9", "from": "k1", "session_type": "sometimes"},
                {"to": "k9", "from": ["*"], "session_type": "all"},
                {"to": "k9", "from": "*", "session_type": "all", "hint": {"zh": "只有中文"}}
            ]
        });
        let rules = ModelSwitchRules::from_config(&config);
        assert_eq!(rules.rules.len(), 2, "two rules do not parse");
        let rule = rules.matching("k1", "k9", New).unwrap();
        assert_eq!(rule.hint(), None, "no English arm");
        assert!(rules.matching("k1", "k9", Old).is_some());
        assert!(rules.matching("*", "k9", Old).is_some());

        for enable in [json!(null), json!("yes")] {
            let mut off = live();
            off["enable"] = enable;
            let rules = ModelSwitchRules::from_config(&off);
            assert!(rules.matching("k3", "k3.1-pro", Old).is_none());
        }
        assert_eq!(
            ModelSwitchRules::from_config(&json!("nonsense")),
            ModelSwitchRules::default()
        );
    }
}
