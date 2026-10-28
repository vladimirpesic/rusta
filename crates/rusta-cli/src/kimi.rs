//! Kimi specifics the CLI needs: probing the Coding hosts with a key, and
//! describing the models a host lists. The catalogue's cache is
//! [`crate::catalogs`]'s.

use lm_bridge::providers::kimi_catalog::{KimiModelInfo, parse_catalog};

/// What an endpoint said when asked for its models with a key.
#[derive(Debug)]
pub enum Probe {
    /// The key works. The models it lists — empty when the response carries
    /// none Rusta can read.
    Accepted(Vec<KimiModelInfo>),
    /// The key was refused (401/403).
    Rejected,
    /// The endpoint could not be asked, or answered something else.
    Unreachable(String),
}

/// Ask `base_url` for its models with `key` (and the custom `headers`):
/// proves the key and the endpoint in one request that costs no tokens.
pub async fn probe(base_url: &str, key: &str, headers: &reqwest::header::HeaderMap) -> Probe {
    use crate::probe::{ProbeAnswer, probe_models};
    match probe_models(base_url, key, headers).await {
        Ok(ProbeAnswer::Accepted(body)) => {
            Probe::Accepted(parse_catalog(&body).unwrap_or_default())
        }
        Ok(ProbeAnswer::Rejected) => Probe::Rejected,
        // A 5xx, a 402, or an odd gateway is not the key's fault.
        Ok(ProbeAnswer::Other(status)) => Probe::Unreachable(format!("HTTP {status}")),
        Err(e) => Probe::Unreachable(e),
    }
}

/// One line per model: id, name, window, input, effort ladder.
#[must_use]
pub fn describe_models(models: &[KimiModelInfo]) -> Vec<String> {
    models
        .iter()
        .map(|model| {
            let mut line = model.id.clone();
            if let Some(name) = &model.display_name {
                line.push_str(&format!(" ({name})"));
            }
            line.push_str(&format!(
                ", {} context",
                crate::provider_defaults::format_window(model.context_length)
            ));
            let input = match (model.image_in, model.video_in) {
                (true, true) => ", image and video input",
                (true, false) => ", image input",
                (false, true) => ", video input",
                (false, false) => "",
            };
            line.push_str(input);
            if let Some(ladder) = &model.efforts {
                line.push_str(&format!(", effort {}", ladder.valid.join("/")));
                if let Some(default) = &ladder.default {
                    line.push_str(&format!(" (default {default})"));
                }
            }
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lm_bridge::providers::kimi_catalog::{EffortLadder, ThinkingSupport};

    #[test]
    fn models_are_described_on_one_line_each() {
        let lines = describe_models(&[KimiModelInfo {
            id: "k3".into(),
            display_name: Some("K3".into()),
            context_length: 1_048_576,
            thinking: ThinkingSupport::Only,
            image_in: true,
            video_in: true,
            dynamic_tools: true,
            efforts: Some(EffortLadder::standard("high")),
        }]);
        assert_eq!(
            lines,
            ["k3 (K3), 1M context, image and video input, effort low/high/max (default high)"]
        );
    }

    #[tokio::test]
    async fn probing_tells_accepted_from_rejected() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer good"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":[{"id":"k3","context_length":1048576,"supports_thinking_type":"only"}]}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(401).set_body_string("{}"))
            .mount(&server)
            .await;

        let none = reqwest::header::HeaderMap::new();
        let Probe::Accepted(models) = probe(&server.uri(), "good", &none).await else {
            panic!("the good key is accepted");
        };
        assert_eq!(models[0].id, "k3");
        assert!(matches!(
            probe(&server.uri(), "bad", &none).await,
            Probe::Rejected
        ));
        assert!(matches!(
            probe("http://127.0.0.1:9", "good", &none).await,
            Probe::Unreachable(_)
        ));
    }
}
