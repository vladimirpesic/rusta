//! `/context`: what the next request to the model holds, part by part, in
//! estimated tokens and as a share of the context window
//! ([`agent_core::context_usage`]).

use agent_core::context_usage::{ContextPart, ContextUsage, PartKind, ToolSource};

/// How many tools' results are listed one by one; the rest share a line.
const RESULT_LINES: usize = 5;

/// The `/context` report for `usage` on `model`.
#[must_use]
pub fn render(usage: &ContextUsage, model: &str) -> String {
    let total = usage.total();
    let mut out = format!(
        "Context on {model}, estimated: {} of {} tokens ({}).\n",
        thousands(total),
        thousands(usage.window),
        share(total, usage.window)
    );

    let rows = rows(&usage.parts);
    if !rows.is_empty() {
        let label_width = rows.iter().map(|(label, _)| label.chars().count()).max();
        let token_width = rows
            .iter()
            .map(|(_, tokens)| thousands(*tokens).len())
            .max();
        out.push('\n');
        for (label, tokens) in &rows {
            out.push_str(&format!(
                "  {label:<label_width$}  {:>token_width$}  {:>6}\n",
                thousands(*tokens),
                share(*tokens, usage.window),
                label_width = label_width.unwrap_or_default(),
                token_width = token_width.unwrap_or_default(),
            ));
        }
    }

    out.push_str(&format!(
        "\nRusta compacts the conversation at {} tokens, the system prompt included and the tool \
         table not; it is at {}.",
        thousands(usage.compaction_at),
        thousands(usage.compaction_measure)
    ));
    if !usage.tools_left_out.is_empty() {
        let offered: usize = usage
            .parts
            .iter()
            .filter(|part| matches!(part.kind, PartKind::Tools(_)))
            .map(|part| part.items)
            .sum();
        out.push_str(&format!(
            "\n{model} takes at most {offered} tools per request, so these are left out: {}.",
            usage.tools_left_out.join(", ")
        ));
    }
    out
}

/// One line per part: its label and its tokens. The results of the tools
/// past the first few share a line.
fn rows(parts: &[ContextPart]) -> Vec<(String, u64)> {
    let mut rows = Vec::new();
    let mut other_results = (0usize, 0usize, 0u64);
    let mut results_listed = 0;
    for part in parts {
        if let PartKind::ToolResults(tool) = &part.kind {
            if results_listed == RESULT_LINES {
                other_results.0 += 1;
                other_results.1 += part.items;
                other_results.2 += part.tokens;
                continue;
            }
            results_listed += 1;
            rows.push((format!("Results: {tool} ({})", part.items), part.tokens));
            continue;
        }
        if other_results.0 > 0 {
            rows.push(other_results_row(other_results));
            other_results = (0, 0, 0);
        }
        rows.push((label(part), part.tokens));
    }
    if other_results.0 > 0 {
        rows.push(other_results_row(other_results));
    }
    rows
}

fn other_results_row((tools, items, tokens): (usize, usize, u64)) -> (String, u64) {
    let noun = if tools == 1 { "tool" } else { "tools" };
    (format!("Results: {tools} other {noun} ({items})"), tokens)
}

fn label(part: &ContextPart) -> String {
    let items = part.items;
    match &part.kind {
        PartKind::SystemPrompt => "System prompt".to_string(),
        PartKind::Tools(ToolSource::BuiltIn) => format!("Tools: built-in ({items})"),
        PartKind::Tools(ToolSource::Mcp(server)) => format!("Tools: {server} ({items})"),
        PartKind::LoadedTools => format!("Loaded tools ({items})"),
        PartKind::ToolAnnouncements => format!("Tool announcements ({items})"),
        PartKind::CompactionSummary => "Compaction summary".to_string(),
        PartKind::UserMessages => format!("Your messages ({items})"),
        PartKind::Notes => format!("Rusta's notes ({items})"),
        PartKind::Replies => format!("Replies ({items})"),
        PartKind::Thinking => format!("Thinking ({items})"),
        PartKind::ToolCalls => format!("Tool calls ({items})"),
        PartKind::ToolResults(tool) => format!("Results: {tool} ({items})"),
        PartKind::Media => format!("Media ({items})"),
    }
}

/// `tokens` as a share of `window`, to a tenth of a percent.
fn share(tokens: u64, window: u64) -> String {
    if window == 0 {
        return "-".to_string();
    }
    let tenths = tokens.saturating_mul(1000) / window;
    if tenths == 0 && tokens > 0 {
        "<0.1%".to_string()
    } else {
        format!("{}.{}%", tenths / 10, tenths % 10)
    }
}

/// `n` with a comma between each group of three digits.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(kind: PartKind, items: usize, tokens: u64) -> ContextPart {
        ContextPart {
            kind,
            items,
            tokens,
        }
    }

    fn usage(parts: Vec<ContextPart>, tools_left_out: Vec<String>) -> ContextUsage {
        ContextUsage {
            parts,
            tools_left_out,
            window: 1_048_576,
            compaction_at: 943_718,
            compaction_measure: 31_200,
        }
    }

    #[test]
    fn numbers_are_grouped_and_shares_rounded_down() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_048_576), "1,048,576");
        assert_eq!(share(0, 100), "0.0%");
        assert_eq!(share(1, 100_000), "<0.1%");
        assert_eq!(share(62_410, 1_048_576), "5.9%");
        assert_eq!(share(5, 0), "-");
    }

    /// The report names the model, the total against the window, a line
    /// per part with its share, and where compaction starts.
    #[test]
    fn each_part_has_a_line_with_its_share() {
        let text = render(
            &usage(
                vec![
                    part(PartKind::SystemPrompt, 1, 9_812),
                    part(PartKind::Tools(ToolSource::BuiltIn), 25, 14_230),
                    part(
                        PartKind::Tools(ToolSource::Mcp("devscriptor".to_string())),
                        6,
                        7_314,
                    ),
                    part(PartKind::UserMessages, 4, 540),
                    part(PartKind::ToolResults("read_file".to_string()), 12, 9_800),
                ],
                Vec::new(),
            ),
            "glm-5.4",
        );
        assert!(
            text.starts_with(
                "Context on glm-5.4, estimated: 41,696 of 1,048,576 tokens (3.9%).\n\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("  System prompt             9,812    0.9%\n"),
            "{text}"
        );
        assert!(
            text.contains("  Tools: built-in (25)     14,230    1.3%\n"),
            "{text}"
        );
        assert!(
            text.contains("  Tools: devscriptor (6)    7,314    0.6%\n"),
            "{text}"
        );
        assert!(
            text.contains("  Your messages (4)           540   <0.1%\n"),
            "{text}"
        );
        assert!(
            text.contains("  Results: read_file (12)   9,800    0.9%\n"),
            "{text}"
        );
        assert!(
            text.ends_with(
                "Rusta compacts the conversation at 943,718 tokens, the system prompt included \
                 and the tool table not; it is at 31,200."
            ),
            "{text}"
        );
    }

    /// The results of the tools past the first five share a line, and the
    /// tools a cap leaves out are named.
    #[test]
    fn many_tools_results_share_a_line_and_left_out_tools_are_named() {
        let mut parts = vec![part(PartKind::Tools(ToolSource::BuiltIn), 3, 900)];
        for (i, tool) in ["a", "b", "c", "d", "e", "f", "g"].into_iter().enumerate() {
            parts.push(part(
                PartKind::ToolResults(tool.to_string()),
                1,
                700 - 100 * i as u64,
            ));
        }
        parts.push(part(PartKind::Media, 1, 2_000));
        let text = render(
            &usage(parts, vec!["grep".to_string(), "fetch".to_string()]),
            "three-tools",
        );
        assert!(text.contains("Results: e (1)"), "{text}");
        assert!(!text.contains("Results: f (1)"), "{text}");
        assert!(text.contains("  Results: 2 other tools (2)  "), "{text}");
        let others = text.find("2 other tools").unwrap();
        assert!(others < text.find("Media (1)").unwrap(), "{text}");
        assert!(
            text.ends_with(
                "three-tools takes at most 3 tools per request, so these are left out: grep, fetch."
            ),
            "{text}"
        );
    }
}
