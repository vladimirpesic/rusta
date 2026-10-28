//! The names MCP tools are offered to the model under:
//! `mcp__<server>__<tool>`, made only of characters every provider accepts.
//!
//! DeepSeek takes function names of `[a-zA-Z0-9_-]` up to 128 characters,
//! and OpenAI up to 64. A dot or a space in a server's or a tool's name, or a
//! name past 64 characters, would fail every request with a 400. Calls are
//! still dispatched by the server's and the tool's own names, and names that
//! would collide are tagged rather than dropped.

use std::collections::HashSet;

/// The longest function name every provider accepts.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// The longest server part. A cut to [`MAX_TOOL_NAME_LEN`] then always falls
/// in the tool part, past the `__` that ends the server part, so the server
/// can still be read off a qualified name.
const MAX_SERVER_PART_LEN: usize = 32;

/// `part` with every character a provider refuses replaced by `_`.
fn wire_safe(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A stable eight-hex-digit tag of `text`: its 32-bit FNV-1a hash. Stable
/// across runs and versions, so a tagged name a permission rule or a session
/// refers to keeps meaning the same tool. Tags hash the original name, so
/// two names that differ only in refused characters get different tags.
fn tag(text: &str) -> String {
    let hash = text.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    format!("{hash:08x}")
}

/// `name` itself when it fits in `max` characters; otherwise its first
/// characters, then `_` and a tag of `original`. `name` is ASCII.
fn fit(name: String, original: &str, max: usize) -> String {
    if name.len() <= max {
        return name;
    }
    let tag = tag(original);
    format!("{}_{tag}", &name[..max - tag.len() - 1])
}

/// The part a server's tools are qualified with: its name made wire-safe,
/// with runs of `_` collapsed so it holds no `__` (which ends the server part
/// of a qualified name), and cut to 32 characters with a tag when longer.
#[must_use]
pub fn server_part(server: &str) -> String {
    let mut part = String::with_capacity(server.len());
    for c in wire_safe(server).chars() {
        if !(c == '_' && part.ends_with('_')) {
            part.push(c);
        }
    }
    fit(part, server, MAX_SERVER_PART_LEN)
}

/// The server parts of the servers `names`, in order ([`server_part`]). A
/// part another server already has is tagged with its own server's name, so
/// no two servers' tools can share a name.
#[must_use]
pub fn server_parts(names: &[&str]) -> Vec<String> {
    let mut taken = HashSet::new();
    names
        .iter()
        .map(|name| {
            let part = unique(server_part(name), name, &taken, MAX_SERVER_PART_LEN);
            taken.insert(part.clone());
            part
        })
        .collect()
}

/// The name tool `tool` of the server whose part is `server_part` is
/// offered under: `mcp__<server>__<tool>`, the tool's name made wire-safe,
/// cut to 64 characters with a tag of the full name when longer.
#[must_use]
pub fn qualified_tool_name(server_part: &str, tool: &str) -> String {
    let name = format!("mcp__{server_part}__{}", wire_safe(tool));
    fit(
        name,
        &format!("mcp__{server_part}__{tool}"),
        MAX_TOOL_NAME_LEN,
    )
}

/// [`qualified_tool_name`], unique among `taken`: a name another of the
/// server's tools already has is tagged with this tool's own name.
#[must_use]
pub fn unique_tool_name(server_part: &str, tool: &str, taken: &HashSet<String>) -> String {
    unique(
        qualified_tool_name(server_part, tool),
        &format!("mcp__{server_part}__{tool}"),
        taken,
        MAX_TOOL_NAME_LEN,
    )
}

/// `name` when `taken` does not hold it; otherwise `name` cut to leave room,
/// then `_` and a tag of `original`, within `max` characters.
fn unique(name: String, original: &str, taken: &HashSet<String>, max: usize) -> String {
    if !taken.contains(&name) {
        return name;
    }
    let tag = tag(original);
    let keep = name.len().min(max - tag.len() - 1);
    format!("{}_{tag}", &name[..keep])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_valid(name: &str) -> bool {
        name.len() <= MAX_TOOL_NAME_LEN
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    /// Valid names are offered exactly as before.
    #[test]
    fn valid_names_are_unchanged() {
        assert_eq!(server_part("devscriptor"), "devscriptor");
        assert_eq!(server_part("zai-web_search"), "zai-web_search");
        assert_eq!(
            qualified_tool_name("devscriptor", "semantic_search"),
            "mcp__devscriptor__semantic_search"
        );
        assert_eq!(
            qualified_tool_name("docs", "get__page"),
            "mcp__docs__get__page"
        );
    }

    /// Dots, spaces and other characters a provider refuses become `_`, and a
    /// server part holds no `__`.
    #[test]
    fn refused_characters_are_replaced() {
        assert_eq!(server_part("my.server"), "my_server");
        assert_eq!(server_part("My Server (local)"), "My_Server_local_");
        assert_eq!(server_part("a__b"), "a_b");
        let name = qualified_tool_name(&server_part("my.server"), "files/read v2");
        assert_eq!(name, "mcp__my_server__files_read_v2");
        assert!(wire_valid(&name));
    }

    /// A name past 64 characters is cut and tagged with a stable hash of the
    /// full one, so two long names that share a prefix stay apart.
    #[test]
    fn long_names_are_cut_and_tagged() {
        let tool_a = format!("{}_alpha", "x".repeat(80));
        let tool_b = format!("{}_beta", "x".repeat(80));
        let a = qualified_tool_name("server", &tool_a);
        let b = qualified_tool_name("server", &tool_b);
        assert!(wire_valid(&a) && wire_valid(&b), "{a} {b}");
        assert_eq!(a.len(), MAX_TOOL_NAME_LEN);
        assert_ne!(a, b);
        assert_eq!(
            a,
            qualified_tool_name("server", &tool_a),
            "the tag is stable"
        );
        assert!(a.starts_with("mcp__server__xxx"), "{a}");

        // A long server name is cut too, short enough that the server part
        // still ends with `__` inside the qualified name.
        let part = server_part(&"s".repeat(100));
        assert_eq!(part.len(), 32);
        let name = qualified_tool_name(&part, &"t".repeat(100));
        assert!(wire_valid(&name));
        assert!(name.starts_with(&format!("mcp__{part}__")), "{name}");
    }

    /// Two servers whose names are made the same are told apart, and so are
    /// two tools of one server.
    #[test]
    fn colliding_names_are_tagged() {
        let parts = server_parts(&["my.server", "my server", "other"]);
        assert_eq!(parts[0], "my_server");
        assert_ne!(parts[1], parts[0]);
        assert!(parts[1].starts_with("my_server_"), "{}", parts[1]);
        assert_eq!(parts[2], "other");

        let mut taken = HashSet::new();
        let first = unique_tool_name("s", "read.file", &taken);
        taken.insert(first.clone());
        let second = unique_tool_name("s", "read file", &taken);
        assert_eq!(first, "mcp__s__read_file");
        assert_ne!(second, first);
        assert!(wire_valid(&second), "{second}");
    }

    /// The tag is the standard 32-bit FNV-1a.
    #[test]
    fn the_tag_is_fnv_1a() {
        assert_eq!(tag(""), "811c9dc5");
        assert_eq!(tag("a"), "e40c292c");
    }
}
