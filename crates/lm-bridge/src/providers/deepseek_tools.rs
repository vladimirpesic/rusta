//! DeepSeek's Search and Fetch APIs: `POST {host}/v1/tools/search` and
//! `POST {host}/v1/tools/fetch`, authenticated with the API key and billed
//! per successful request (api-docs.deepseek.com/guides/search).

use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::errors::LlmError;
use crate::providers::kimi_services::SearchHit;

/// A search may take this long.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
/// A fetch reads the page, so it gets longer.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest query, or question behind it, the Search API takes, in
/// characters.
const MAX_QUERY_CHARS: usize = 128;
/// The longest a result's text may be in the model's results, in
/// characters: the length DeepSeek documents for a summary. Live summaries
/// ran to 3,000, which ten results a search would make 30,000.
const MAX_SNIPPET_CHARS: usize = 800;

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchResult>,
}

/// One result as the Search API gives it. `summary` is the long one,
/// written for a model to read; `snippet` is the short one, for display.
#[derive(Deserialize)]
struct SearchResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    site_name: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

#[derive(Deserialize)]
struct FetchResponse {
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
}

/// A client for DeepSeek's Search and Fetch APIs on one host.
#[derive(Debug, Clone)]
pub struct DeepseekTools {
    client: reqwest::Client,
    tools_url: String,
    api_key: String,
}

impl DeepseekTools {
    /// A client for the tools of the host `base_url` names — the session's
    /// DeepSeek base URL, with or without its `/v1` or `/beta` — with
    /// `api_key`.
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, LlmError> {
        let host = base_url.trim_end_matches('/');
        let host = host
            .strip_suffix("/v1")
            .or_else(|| host.strip_suffix("/beta"))
            .unwrap_or(host);
        Ok(Self {
            client: crate::http::client_builder().build()?,
            tools_url: format!("{host}/v1/tools"),
            api_key: api_key.to_string(),
        })
    }

    /// Search the web for `query`, cut to the 128 characters the API takes.
    /// `question`, the prompt the search serves, goes along as the API's
    /// `ori_question`, which DeepSeek reads the query's intent from: on one
    /// line and cut alike, and left out when blank.
    ///
    /// Each hit's snippet is the result's summary, the text DeepSeek writes
    /// for a model, or its short snippet where it has no summary: on one
    /// line, and at most `MAX_SNIPPET_CHARS` long.
    pub async fn search(
        &self,
        query: &str,
        question: Option<&str>,
    ) -> Result<Vec<SearchHit>, LlmError> {
        let mut request = json!({ "query": bounded_query(query) });
        let question = question.map(one_line).unwrap_or_default();
        let question = cut_at_word(&question, MAX_QUERY_CHARS);
        if !question.is_empty() {
            request["ori_question"] = json!(question);
        }
        let body: SearchResponse = self.post("search", request, SEARCH_TIMEOUT).await?;
        Ok(body
            .results
            .into_iter()
            .map(|result| SearchHit {
                snippet: model_snippet(&result.summary, &result.snippet),
                title: result.title,
                url: result.url,
                date: result.date,
                site_name: result.site_name,
            })
            .collect())
    }

    /// The page at `url` as Markdown, under its title when the content does
    /// not open with it. The API escapes every punctuation mark (`\# Title`,
    /// `\- item`, `\.`), which hides the page's headings and lists; those
    /// escapes are undone (`unescape_markdown`).
    pub async fn fetch(&self, url: &str) -> Result<String, LlmError> {
        let page: FetchResponse = self
            .post("fetch", json!({ "url": url }), FETCH_TIMEOUT)
            .await?;
        if page.content.trim().is_empty() {
            return Err(LlmError::Provider(format!(
                "DeepSeek fetch gave no content for {url}"
            )));
        }
        let content = unescape_markdown(&page.content);
        let title = page.title.trim();
        if title.is_empty() || content.trim_start().starts_with('#') {
            Ok(content)
        } else {
            Ok(format!("# {title}\n\n{content}"))
        }
    }

    /// POST `body` to the tool `tool`, and read its answer. A failure names
    /// the status and DeepSeek's own message.
    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        tool: &str,
        body: Value,
        timeout: Duration,
    ) -> Result<T, LlmError> {
        let response = self
            .client
            .post(format!("{}/{tool}", self.tools_url))
            .bearer_auth(&self.api_key)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .timeout(timeout)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|error| error["error"]["message"].as_str().map(str::to_string))
                .unwrap_or(text);
            return Err(LlmError::Provider(
                format!("DeepSeek {tool} failed: HTTP {status}. {message}")
                    .trim()
                    .to_string(),
            ));
        }
        Ok(response.json().await?)
    }
}

/// `query` within [`MAX_QUERY_CHARS`] ([`cut_at_word`]).
fn bounded_query(query: &str) -> &str {
    cut_at_word(query.trim(), MAX_QUERY_CHARS)
}

/// A result's text for the model: its summary, or its snippet without one,
/// on one line and within [`MAX_SNIPPET_CHARS`], with an ellipsis where it
/// was cut.
fn model_snippet(summary: &str, snippet: &str) -> String {
    let text = if summary.trim().is_empty() {
        snippet
    } else {
        summary
    };
    let line = one_line(text);
    let cut = cut_at_word(&line, MAX_SNIPPET_CHARS);
    if cut.len() < line.len() {
        format!("{cut}…")
    } else {
        line
    }
}

/// `text` on one line, each run of whitespace a single space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` within `max` characters, cut at the last whitespace before the
/// limit when there is one.
fn cut_at_word(text: &str, max: usize) -> &str {
    let Some((cut, _)) = text.char_indices().nth(max) else {
        return text;
    };
    let head = &text[..cut];
    head.rfind(char::is_whitespace)
        .map_or(head, |space| head[..space].trim_end())
}

/// `markdown` without the backslash before each punctuation mark, outside
/// code: `\# Luhn algorithm` is a heading again, and `\- Drop` a list item.
/// Fenced blocks and inline code keep their text as it is.
fn unescape_markdown(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut fenced = false;
    for line in markdown.split_inclusive('\n') {
        let opener = line.trim_start();
        if opener.starts_with("```") || opener.starts_with("~~~") {
            fenced = !fenced;
            out.push_str(line);
            continue;
        }
        if fenced {
            out.push_str(line);
            continue;
        }
        let mut chars = line.chars().peekable();
        let mut in_code = false;
        while let Some(c) = chars.next() {
            match c {
                '`' => {
                    in_code = !in_code;
                    out.push(c);
                }
                '\\' if !in_code => match chars.peek() {
                    Some(&next) if next.is_ascii_punctuation() => {
                        out.push(next);
                        chars.next();
                    }
                    _ => out.push(c),
                },
                _ => out.push(c),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// The search posts the query to the host's tools, and each hit carries
    /// the summary written for a model, or the snippet without one.
    #[tokio::test]
    async fn search_posts_the_query_and_reads_the_summaries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .and(header("authorization", "Bearer sk-test"))
            .and(body_json(json!({ "query": "DeepSeek V4 release" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    {
                        "rerank_score": 2.4,
                        "title": "DeepSeek V4 model released",
                        "url": "https://example.com/deepseek-v4",
                        "snippet": "DeepSeek V4 is now available.",
                        "summary": "# DeepSeek V4\nDeepSeek has released the V4 model,\n with tool calling.",
                        "content": "",
                        "site_name": "Example News",
                        "date": "2026-06-01T00:00:00+00:00",
                        "authority": 3
                    },
                    {"title": "Short", "url": "https://s.example", "snippet": "Only a snippet."}
                ]
            })))
            .mount(&server)
            .await;

        for base in [server.uri(), format!("{}/v1/", server.uri())] {
            let tools = DeepseekTools::new(&base, "sk-test").unwrap();
            let hits = tools.search("DeepSeek V4 release", None).await.unwrap();
            assert_eq!(
                hits,
                [
                    SearchHit {
                        title: "DeepSeek V4 model released".into(),
                        url: "https://example.com/deepseek-v4".into(),
                        snippet:
                            "# DeepSeek V4 DeepSeek has released the V4 model, with tool calling."
                                .into(),
                        date: Some("2026-06-01T00:00:00+00:00".into()),
                        site_name: Some("Example News".into()),
                    },
                    SearchHit {
                        title: "Short".into(),
                        url: "https://s.example".into(),
                        snippet: "Only a snippet.".into(),
                        date: None,
                        site_name: None,
                    },
                ],
                "base {base}"
            );
        }
    }

    /// A fetched page comes back as its Markdown, under its title when the
    /// content does not open with a heading.
    #[tokio::test]
    async fn fetch_gives_the_page_as_markdown() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/fetch"))
            .and(body_json(json!({ "url": "https://example.com/a" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "title": "Example article",
                "content": "# Example article\n\nArticle content."
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/fetch"))
            .and(body_json(json!({ "url": "https://example.com/b" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "title": "Plain",
                "content": "Body text."
            })))
            .mount(&server)
            .await;
        // As the live API gives it: every punctuation mark escaped.
        Mock::given(method("POST"))
            .and(path("/v1/tools/fetch"))
            .and(body_json(json!({ "url": "https://example.com/c" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "title": "Luhn algorithm - Wikipedia",
                "content": "\\# Luhn algorithm\n\nThe formula \\(creator\\: Luhn\\)\\.\n\n\\- Drop the digit\\.\n"
            })))
            .mount(&server)
            .await;
        let tools = DeepseekTools::new(&format!("{}/beta", server.uri()), "sk-test").unwrap();
        assert_eq!(
            tools.fetch("https://example.com/a").await.unwrap(),
            "# Example article\n\nArticle content."
        );
        assert_eq!(
            tools.fetch("https://example.com/b").await.unwrap(),
            "# Plain\n\nBody text."
        );
        assert_eq!(
            tools.fetch("https://example.com/c").await.unwrap(),
            "# Luhn algorithm\n\nThe formula (creator: Luhn).\n\n- Drop the digit.\n"
        );
    }

    /// Escapes are undone outside code, and a backslash before anything but
    /// punctuation stays; fenced blocks and inline code keep theirs.
    #[test]
    fn escapes_are_undone_outside_code() {
        let escaped = "\\#\\# Use \\`x\\`\nPath C:\\\\tmp\\, \\{\\\\displaystyle s\\} \\n stays\n`a\\.b`\n```\nlet x \\= 1\\.\n```\n";
        assert_eq!(
            unescape_markdown(escaped),
            "## Use `x`\nPath C:\\tmp, {\\displaystyle s} \\n stays\n`a\\.b`\n```\nlet x \\= 1\\.\n```\n"
        );
    }

    /// A result's text is put on one line and cut at a word, marked so.
    #[test]
    fn a_long_summary_is_one_line_and_cut() {
        let summary = format!("# Title\n...\n{}", "word ".repeat(400));
        let snippet = model_snippet(&summary, "short");
        assert!(!snippet.contains('\n'));
        assert!(snippet.ends_with("word…"), "{snippet}");
        assert!(snippet.chars().count() <= MAX_SNIPPET_CHARS + 1);
        assert_eq!(model_snippet("  ", "Only  a\nsnippet."), "Only a snippet.");
    }

    /// A failure names the tool, the status and DeepSeek's message.
    #[tokio::test]
    async fn failures_name_the_status_and_the_reason() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(ResponseTemplate::new(402).set_body_json(json!({
                "error": {
                    "message": "Insufficient Balance",
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "invalid_request_error"
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/fetch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "title": "",
                "content": "  "
            })))
            .mount(&server)
            .await;
        let tools = DeepseekTools::new(&server.uri(), "sk-test").unwrap();
        let error = tools.search("q", None).await.unwrap_err().to_string();
        assert!(
            error.contains(
                "DeepSeek search failed: HTTP 402 Payment Required. Insufficient Balance"
            ),
            "{error}"
        );
        let error = tools
            .fetch("https://e.example")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("gave no content for https://e.example"),
            "{error}"
        );
    }

    /// A long query reaches the API cut to what it takes, which would
    /// otherwise refuse it.
    #[tokio::test]
    async fn a_long_query_is_sent_cut() {
        let server = MockServer::start().await;
        let long = "luhn check digit ".repeat(10);
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .and(body_json(json!({ "query": bounded_query(&long) })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [] })))
            .mount(&server)
            .await;
        let tools = DeepseekTools::new(&server.uri(), "sk-test").unwrap();
        assert_eq!(tools.search(&long, None).await.unwrap(), []);
    }

    /// The prompt behind a query goes along as `ori_question`, on one line
    /// and cut at a word to the 128 characters the API takes; a blank one
    /// is left out.
    #[tokio::test]
    async fn the_question_behind_a_query_goes_along() {
        let server = MockServer::start().await;
        let question = format!(
            "  Why does the Luhn\n\tcheck double {}digits?",
            "every second ".repeat(10)
        );
        let sent = format!(
            "Why does the Luhn check double {}every",
            "every second ".repeat(7)
        );
        assert!(sent.chars().count() <= MAX_QUERY_CHARS);
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .and(body_json(json!({ "query": "luhn", "ori_question": sent })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [] })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .and(body_json(json!({ "query": "luhn" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [] })))
            .expect(1)
            .mount(&server)
            .await;
        let tools = DeepseekTools::new(&server.uri(), "sk-test").unwrap();
        assert_eq!(tools.search("luhn", Some(&question)).await.unwrap(), []);
        assert_eq!(tools.search("luhn", Some(" \n ")).await.unwrap(), []);
    }

    /// A query over 128 characters is cut at a word boundary before the
    /// limit; a shorter one, or one without spaces, as the limit allows.
    #[test]
    fn a_long_query_is_cut_at_a_word() {
        let word = "ä".repeat(10);
        let long = vec![word.as_str(); 20].join(" ");
        let cut = bounded_query(&long);
        assert!(
            cut.chars().count() <= MAX_QUERY_CHARS,
            "{}",
            cut.chars().count()
        );
        assert!(cut.ends_with(&word) && long.starts_with(cut));
        assert_eq!(bounded_query("  short query "), "short query");
        let solid = "x".repeat(200);
        assert_eq!(bounded_query(&solid).len(), MAX_QUERY_CHARS);
    }
}
