//! Tag extraction — ADR.md §6.5 step 2.
//!
//! Parse a source file with its language's grammar, run the embedded tags
//! query, and turn `@name.definition.*` / `@name.reference.*` captures into
//! [`Tag`]s (Aider's `get_tags_raw`, sans pygments). Outer `@definition.*`
//! captures record where each definition *ends*, powering the `map_drill`
//! full-span view.
//!
//! Files whose query yields defs but no refs (c/cpp queries have no reference
//! patterns) get refs backfilled by an identifier word scan so they still
//! connect to the reference graph (§6.5 step 2).

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use crate::lang::Lang;
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

/// Bumped **by hand** when an embedded query changes what tags mean — part
/// of the cache key (§6.5 step 7).
///
/// It is not derived from the query text, so editing a `.scm` without
/// bumping this serves stale tag shapes from the in-memory cache until the
/// process restarts (the cache is per-process, so the blast radius is one
/// session). Treat a query edit and a bump here as one change.
pub(crate) const QUERY_VERSION: u32 = 1;

/// Reserved words across the supported languages; a merged set suffices
/// because a keyword that leaks in from another language can never match a
/// definition (it is reserved there too) and is dropped by the
/// defines ∩ references intersection.
const KEYWORDS: &str = "as async await break case catch class const continue def default defer del do else enum except export extends extern false finally fn for from func go goto if impl import in interface is let loop match mod module mut namespace new None not null nullptr or package pass print pub raise return self static struct super switch template this throw trait true True False try type typedef union unsafe use using var virtual void where while with yield";

/// Whether `word` is a reserved word in any supported language.
fn is_keyword(word: &str) -> bool {
    KEYWORDS.split(' ').any(|kw| kw == word)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TagKind {
    Def,
    Ref,
}

/// One identifier occurrence: `name` at 0-based `line` (Aider's
/// `start_point[0]`), with the definition's last line in `line_end`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tag {
    pub(crate) name: String,
    pub(crate) kind: TagKind,
    pub(crate) line: usize,
    /// 0-based line of the end of the enclosing definition node — equals
    /// `line` for refs and for queries without an outer capture.
    pub(crate) line_end: usize,
}

/// Compiled tags queries, one per language, built on first use.
///
/// `Query::new` re-parses the `.scm` text every call, which on a large repo
/// meant recompiling seven queries once per *file*. The compiled query is
/// immutable and `Send + Sync`, so it is cached for the process (the same
/// reasoning as devscriptor's `ParserCache`, which caches the `Language`
/// rather than the non-`Send` `Parser`).
fn cached_query(lang: Lang) -> Option<&'static Query> {
    static CACHE: OnceLock<RwLock<HashMap<Lang, &'static Query>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    if let Some(query) = cache.read().ok()?.get(&lang) {
        return Some(*query);
    }
    // Compiling twice on a race is harmless; the loser's copy is dropped.
    let compiled: &'static Query = Box::leak(Box::new(lang.compile_query().ok()?));
    cache.write().ok()?.insert(lang, compiled);
    Some(compiled)
}

/// Extract def/ref tags from `source`. `None` means "not a map candidate"
/// (parse failure or query/grammar mismatch) — callers skip the file.
pub(crate) fn extract_tags(source: &str, lang: Lang) -> Option<Vec<Tag>> {
    let query = cached_query(lang)?;
    let mut parser = Parser::new();
    parser.set_language(&lang.grammar()).ok()?;
    let tree = parser.parse(source, None)?;
    let names = query.capture_names();

    let mut cursor = QueryCursor::new();
    let text = |node: Node| source.get(node.start_byte()..node.end_byte()).unwrap_or("");
    // `&[u8]` implements `TextProvider` via byte-range lookup; the cursor
    // evaluates `#eq?`/`#match?` predicates with it during iteration.
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    let mut tags = Vec::new();
    while let Some(m) = matches.next() {
        // Outer `definition.*` / `reference.*` nodes of this match, by capture
        // name — the full spans defs adopt for `map_drill`.
        let mut outers: Vec<(&str, Node)> = Vec::new();
        for cap in m.captures {
            let cname = names[cap.index as usize];
            if cname.starts_with("definition.") || cname.starts_with("reference.") {
                outers.push((cname, cap.node));
            }
        }
        for cap in m.captures {
            let cname = names[cap.index as usize];
            let Some((kind, suffix)) = name_capture(cname) else {
                continue;
            };
            let node = cap.node;
            let line = node.start_position().row;
            let line_end = if kind == TagKind::Def {
                outers
                    .iter()
                    .find(|(outer, _)| *outer == suffix)
                    .map_or(line, |&(_, o)| o.end_position().row)
                    .max(line)
            } else {
                line
            };
            let name = text(node).trim();
            if name.is_empty() || name.len() > 128 {
                continue;
            }
            tags.push(Tag {
                name: name.to_string(),
                kind,
                line,
                line_end,
            });
        }
    }

    if !tags.iter().any(|t| t.kind == TagKind::Ref) && tags.iter().any(|t| t.kind == TagKind::Def) {
        // Defs without refs (c/cpp): backfill refs with an identifier word
        // scan (§6.5 step 2 — Aider's pygments fallback, minus the dependency).
        for (line, ident) in scan_identifiers(source) {
            tags.push(Tag {
                name: ident,
                kind: TagKind::Ref,
                line,
                line_end: line,
            });
        }
    }
    Some(tags)
}

/// Split `name.definition.X` / `name.reference.Y` capture names into kind plus
/// the suffix shared with the outer capture (`definition.X` / `reference.Y`).
fn name_capture(cname: &str) -> Option<(TagKind, &str)> {
    let suffix = cname.strip_prefix("name.")?;
    if suffix.starts_with("definition.") {
        Some((TagKind::Def, suffix))
    } else if suffix.starts_with("reference.") {
        Some((TagKind::Ref, suffix))
    } else {
        None
    }
}

/// Identifier-token word scan: ASCII `[A-Za-z_][A-Za-z0-9_]*` runs that are
/// not keywords, as `(0-based line, identifier)` pairs.
fn scan_identifiers(source: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (line, text) in source.lines().enumerate() {
        let mut start: Option<usize> = None;
        for (i, b) in text.char_indices() {
            let ident_ch = b == '_' || b.is_ascii_alphanumeric();
            if ident_ch && start.is_none() && !b.is_ascii_digit() {
                start = Some(i);
            } else if !ident_ch {
                if let Some(s) = start.take() {
                    push_ident(&mut out, line, &text[s..i]);
                }
            }
        }
        if let Some(s) = start {
            push_ident(&mut out, line, &text[s..]);
        }
    }
    out
}

fn push_ident(out: &mut Vec<(usize, String)>, line: usize, ident: &str) {
    if !ident.is_empty() && !is_keyword(ident) {
        out.push((line, ident.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_defs_and_refs_with_full_spans() {
        let src = "mod inner {\n    pub fn alpha(x: u32) -> u32 { x + 1 }\n}\nfn main() {\n    let v = inner::alpha(2);\n}\n";
        let tags = extract_tags(src, Lang::Rust).expect("parses");
        let alpha = tags
            .iter()
            .find(|t| t.name == "alpha" && t.kind == TagKind::Def)
            .expect("alpha def");
        assert_eq!(alpha.line, 1);
        assert_eq!(alpha.line_end, 1, "fn body ends on its own line");
        assert!(
            tags.iter()
                .any(|t| t.name == "alpha" && t.kind == TagKind::Ref && t.line == 4)
        );
        assert!(
            tags.iter()
                .any(|t| t.name == "inner" && t.kind == TagKind::Def)
        );
    }

    #[test]
    fn multi_line_definition_spans_its_body() {
        let src = "fn long_one() {\n    let a = 1;\n    let b = 2;\n}\n";
        let tags = extract_tags(src, Lang::Rust).expect("parses");
        let def = tags
            .iter()
            .find(|t| t.name == "long_one")
            .expect("def present");
        assert_eq!(def.line, 0);
        assert_eq!(def.line_end, 3);
    }

    #[test]
    fn def_only_language_gets_reference_backfill() {
        // C queries yield defs but no refs; the word scan must connect them.
        let src = "int compute(void) {\n    return helper(41) + 1;\n}\n";
        let tags = extract_tags(src, Lang::C).expect("parses");
        assert!(
            tags.iter()
                .any(|t| t.name == "compute" && t.kind == TagKind::Def)
        );
        assert!(
            tags.iter()
                .any(|t| t.name == "helper" && t.kind == TagKind::Ref && t.line == 1)
        );
        assert!(
            tags.iter()
                .any(|t| t.name == "compute" && t.kind == TagKind::Ref)
        );
    }

    #[test]
    fn empty_source_yields_no_tags() {
        assert_eq!(extract_tags("", Lang::Rust).expect("parses").len(), 0);
    }
}
