//! Language registry — file-extension mapping and grammar/query loading
//! (DEVELOPMENT_PLAN.md §6.5 step 2, dependency set §10).
//!
//! Seven languages, seven grammar crates: the exact core/grammar matrix from
//! the workspace pinning (see the root `Cargo.toml` note). Grammars expose a
//! `LanguageFn` through the `tree-sitter-language` compatibility layer and are
//! loaded with `.into()`; the resulting `Language` is a cheap FFI pointer, so
//! [`Lang::grammar`] can be called freely.

use std::path::Path;
use tree_sitter::{Language, Query};

/// The repo-map language set (§10): rust, python, javascript, typescript, go,
/// c, cpp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Lang {
    Rust,
    Python,
    Javascript,
    Typescript,
    Tsx,
    Go,
    C,
    Cpp,
}

/// Extensions to skip even when the directory walk sees them — binary/build
/// outputs would otherwise slow every scan (§6.5 step 1 fallback walk).
pub(crate) const IGNORED_DIRS: &[&str] = &[".git", "target", "node_modules", "dist"];

impl Lang {
    /// Map a file extension to its language. `None` files never enter the
    /// pipeline.
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::Javascript),
            "ts" | "mts" | "cts" => Some(Self::Typescript),
            "tsx" => Some(Self::Tsx),
            "go" => Some(Self::Go),
            "c" | "h" => Some(Self::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(Self::Cpp),
            _ => None,
        }
    }

    /// The compiled grammar. `Parser::set_language` validates the ABI range at
    /// parse time, which is the real guard against core/grammar skew.
    pub(crate) fn grammar(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Javascript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Typescript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        }
    }

    /// Embedded tree-sitter tags query — verbatim Aider ports (see
    /// `queries/README.md` for origin and licensing).
    pub(crate) const fn query(self) -> &'static str {
        match self {
            Self::Rust => include_str!("../queries/rust-tags.scm"),
            Self::Python => include_str!("../queries/python-tags.scm"),
            Self::Javascript => include_str!("../queries/javascript-tags.scm"),
            // TSX shares TypeScript's tags query (Aider does the same).
            Self::Typescript | Self::Tsx => include_str!("../queries/typescript-tags.scm"),
            Self::Go => include_str!("../queries/go-tags.scm"),
            Self::C => include_str!("../queries/c-tags.scm"),
            Self::Cpp => include_str!("../queries/cpp-tags.scm"),
        }
    }

    /// Compile the tags query against the pinned grammar. A query written for
    /// a different grammar generation fails here — this is the grammar-drift
    /// guard (§13), exercised for every language by the unit tests.
    pub(crate) fn compile_query(self) -> Result<Query, tree_sitter::QueryError> {
        Query::new(&self.grammar(), self.query())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_map_covers_the_plan_language_set() {
        let file = |ext: &str| Lang::from_path(Path::new(&format!("a.{ext}")));
        assert_eq!(file("rs"), Some(Lang::Rust));
        assert_eq!(file("py"), Some(Lang::Python));
        assert_eq!(file("js"), Some(Lang::Javascript));
        assert_eq!(file("ts"), Some(Lang::Typescript));
        assert_eq!(file("tsx"), Some(Lang::Tsx));
        assert_eq!(file("go"), Some(Lang::Go));
        assert_eq!(file("c"), Some(Lang::C));
        assert_eq!(file("h"), Some(Lang::C));
        assert_eq!(file("cpp"), Some(Lang::Cpp));
        assert_eq!(file("hpp"), Some(Lang::Cpp));
        assert_eq!(file("md"), None);
        assert_eq!(Lang::from_path(Path::new("noext")), None);
    }

    /// Grammar-drift guard: every embedded query must compile against its
    /// pinned grammar crate (DEVELOPMENT_PLAN.md §13).
    #[test]
    fn every_tags_query_compiles_against_its_grammar() {
        for lang in [
            Lang::Rust,
            Lang::Python,
            Lang::Javascript,
            Lang::Typescript,
            Lang::Tsx,
            Lang::Go,
            Lang::C,
            Lang::Cpp,
        ] {
            assert!(
                lang.compile_query().is_ok(),
                "tags query failed to compile for {lang:?}"
            );
        }
    }
}
