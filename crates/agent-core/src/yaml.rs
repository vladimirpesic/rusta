//! YAML for agent profiles and skill frontmatter, read with `serde-saphyr`.
//!
//! The rules are YAML 1.2's: only `true` and `false` (in any case) are
//! booleans, so `yes` and `on` stay text. A key with no value, `~` or `null`
//! means its field's default where the field has one: an empty name, list or
//! table, as a file left that way intends ([`or_default`], [`or_default_values`]).
//! So does a document with nothing in it. An error names its line and quotes
//! it.

use std::collections::HashMap;
use std::hash::Hash;

use serde::de::{Deserialize, DeserializeOwned, Deserializer};

/// Parse `text` as a `T`; a document with nothing in it is `T::default()`.
/// `line_offset` is the number of lines before `text` in its file, so an
/// error names the file's line.
///
/// # Errors
/// Returns what makes `text` no `T`, with the line it is on.
pub fn from_str<T: DeserializeOwned + Default>(text: &str, line_offset: u64) -> Result<T, String> {
    let options = serde_saphyr::options! { strict_booleans: true };
    serde_saphyr::from_str_with_options::<Option<T>>(text, options)
        .map(Option::unwrap_or_default)
        .map_err(|error| describe(&error, text, line_offset))
}

/// One line saying what is wrong and where: the parser's message, then the
/// line it points at.
fn describe(error: &serde_saphyr::Error, text: &str, line_offset: u64) -> String {
    let message = error.render_with_options(serde_saphyr::render_options! {
        snippets: serde_saphyr::SnippetMode::Off,
        line_offset: line_offset,
    });
    let line = error
        .location()
        .map(|at| at.line())
        .filter(|&line| line > 0)
        .and_then(|line| text.lines().nth(usize::try_from(line - 1).ok()?))
        .map(str::trim)
        .filter(|line| !line.is_empty());
    match line {
        Some(line) => format!("{message}: `{line}`"),
        None => message,
    }
}

/// Field attribute (`deserialize_with`): a value of `null` is the field's
/// default.
///
/// # Errors
/// Returns the deserializer's error for a value that is neither null nor a `T`.
pub fn or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Field attribute (`deserialize_with`) for a table: `null` is an empty
/// table, and a key whose value is `null` has the value's default.
///
/// # Errors
/// Returns the deserializer's error for anything else that is no such table.
pub fn or_default_values<'de, D, K, V>(deserializer: D) -> Result<HashMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Deserialize<'de> + Eq + Hash,
    V: Deserialize<'de> + Default,
{
    let table = Option::<HashMap<K, Option<V>>>::deserialize(deserializer)?.unwrap_or_default();
    Ok(table
        .into_iter()
        .map(|(key, value)| (key, value.unwrap_or_default()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Default, PartialEq, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    struct Settings {
        #[serde(deserialize_with = "or_default")]
        name: String,
        #[serde(deserialize_with = "or_default")]
        tools: Vec<String>,
        #[serde(deserialize_with = "or_default_values")]
        variables: HashMap<String, String>,
        enabled: Option<bool>,
        label: Option<String>,
    }

    fn parse(text: &str) -> Result<Settings, String> {
        from_str(text, 0)
    }

    /// A key left without a value, or set to `~` or `null`, is its default,
    /// as is a document with nothing in it.
    #[test]
    fn nothing_means_the_default() {
        let settings = parse("name:\ntools: ~\nvariables:\n  a:\n  b: text\n").unwrap();
        assert_eq!(settings.name, "");
        assert!(settings.tools.is_empty());
        assert_eq!(settings.variables["a"], "");
        assert_eq!(settings.variables["b"], "text");
        assert_eq!(parse("variables: null").unwrap(), Settings::default());
        for empty in ["", "\n", "# only a comment\n", "---\n", "~"] {
            assert_eq!(parse(empty).unwrap(), Settings::default(), "{empty:?}");
        }
    }

    /// Only YAML 1.2's spellings are booleans: `yes` is no boolean, and a
    /// text field keeps it as text.
    #[test]
    fn booleans_are_yaml_1_2s() {
        for (text, enabled) in [("true", true), ("False", false), ("TRUE", true)] {
            let settings = parse(&format!("enabled: {text}")).unwrap();
            assert_eq!(settings.enabled, Some(enabled), "{text}");
        }
        for text in ["yes", "on", "n"] {
            assert!(parse(&format!("enabled: {text}")).is_err(), "{text}");
            let settings = parse(&format!("label: {text}")).unwrap();
            assert_eq!(settings.label.as_deref(), Some(text));
        }
    }

    /// An error is one line that names the line, counted in the whole file,
    /// and quotes it.
    #[test]
    fn an_error_names_and_quotes_its_line() {
        let text = "name: n\nnmae: typo\n";
        let error = parse(text).unwrap_err();
        assert!(error.contains("unknown field `nmae`"), "{error}");
        assert!(error.contains("line 2, column 1"), "{error}");
        assert!(error.ends_with(": `nmae: typo`"), "{error}");
        assert!(!error.contains('\n'), "{error}");

        let shifted = from_str::<Settings>(text, 3).unwrap_err();
        assert!(shifted.contains("line 5, column 1"), "{shifted}");
        assert!(shifted.ends_with(": `nmae: typo`"), "{shifted}");

        let twice = parse("name: a\nname: b\n").unwrap_err();
        assert!(twice.contains("duplicate"), "{twice}");
    }
}
