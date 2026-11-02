//! Extra HTTP headers for every model request: a gateway's routing key, a
//! tenant id, a tracing header.
//!
//! They are the lowest layer: a header the provider sends itself keeps its
//! value, and `Authorization` never comes from here.

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};

use crate::errors::LlmError;

/// Parse newline-separated `Name: value` lines, the common format for a
/// custom-headers variable: a line without a colon or with an empty name is
/// skipped; names and values are trimmed.
#[must_use]
pub fn parse_header_lines(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Build a header map from `(name, value)` pairs; a later pair replaces an
/// earlier one of the same name. An invalid name or value is an error that
/// names it; `Authorization` is refused, since the key is sent on its own.
pub fn header_map<I, N, V>(pairs: I) -> Result<HeaderMap, LlmError>
where
    I: IntoIterator<Item = (N, V)>,
    N: AsRef<str>,
    V: AsRef<str>,
{
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        let (name, value) = (name.as_ref(), value.as_ref());
        let header = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| LlmError::Config(format!("custom header name {name:?}: {e}")))?;
        if header == AUTHORIZATION {
            return Err(LlmError::Config(
                "custom headers cannot set Authorization; the API key is sent on its own"
                    .to_string(),
            ));
        }
        let value = HeaderValue::from_str(value)
            .map_err(|e| LlmError::Config(format!("custom header {name}: {e}")))?;
        headers.insert(header, value);
    }
    Ok(headers)
}

/// Add `custom` to `headers` under the rule above: names already present
/// keep their values.
pub fn add_below(headers: &mut HeaderMap, custom: &HeaderMap) {
    for (name, value) in custom {
        if *name != AUTHORIZATION && !headers.contains_key(name) {
            headers.insert(name.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lines_parse() {
        let pairs = parse_header_lines(
            "X-Route: blue\n  X-Tenant :  acme  \nnot a header\n: empty name\nX-Empty:",
        );
        assert_eq!(
            pairs,
            [
                ("X-Route".to_string(), "blue".to_string()),
                ("X-Tenant".to_string(), "acme".to_string()),
                ("X-Empty".to_string(), String::new()),
            ]
        );
        assert!(parse_header_lines("").is_empty());
    }

    #[test]
    fn invalid_headers_are_named() {
        let err = header_map([("Bad Name", "x")]).unwrap_err();
        assert!(err.to_string().contains("\"Bad Name\""), "{err}");
        let err = header_map([("X-Ok", "line\nbreak")]).unwrap_err();
        assert!(err.to_string().contains("X-Ok"), "{err}");
        let err = header_map([("authorization", "Bearer x")]).unwrap_err();
        assert!(err.to_string().contains("Authorization"), "{err}");
    }

    #[test]
    fn a_later_pair_wins_and_the_providers_own_headers_stay() {
        let custom = header_map([
            ("X-Route", "blue"),
            ("x-route", "green"),
            ("Accept-Language", "de"),
        ])
        .unwrap();
        assert_eq!(custom["x-route"], "green");

        let mut headers = HeaderMap::new();
        headers.insert("accept-language", HeaderValue::from_static("en-US,en"));
        add_below(&mut headers, &custom);
        assert_eq!(headers["accept-language"], "en-US,en");
        assert_eq!(headers["x-route"], "green");
    }
}
