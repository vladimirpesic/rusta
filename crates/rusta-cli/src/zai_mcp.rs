//! Auto-injection of Z.AI's hosted MCP servers for the GLM provider: web
//! search, web reader and Zread, on the platform that issued the key.

use mcp_client::config::{McpDefaults, McpServerConfig};

use crate::endpoints::{Billing, Region};

/// A hosted Z.AI MCP server Rusta injects.
struct HostedServer {
    /// The name it is registered under.
    name: &'static str,
    /// Its path under the platform's MCP root.
    path: &'static str,
    /// Whether its tools are loaded on demand with `select_tools` rather than
    /// listed on every request.
    deferred: bool,
}

const HOSTED_SERVERS: [HostedServer; 3] = [
    HostedServer {
        name: "zai_web_search",
        path: "web_search_prime",
        deferred: false,
    },
    HostedServer {
        name: "zai_web_reader",
        path: "web_reader",
        deferred: false,
    },
    // Zread reads public GitHub repositories: their docs, structure and
    // files. Useful now and then, so its tools load when a task needs them.
    HostedServer {
        name: "zai_zread",
        path: "zread",
        deferred: true,
    },
];

/// The MCP root of the platform that issued the key: api.z.ai for global
/// keys, open.bigmodel.cn for mainland ones. Each refuses the other's keys.
#[must_use]
pub fn mcp_root(region: Region) -> &'static str {
    match region {
        Region::Global => "https://api.z.ai/api/mcp",
        Region::China => "https://open.bigmodel.cn/api/mcp",
    }
}

/// Inject Z.AI's `web_search_prime`, `web_reader` and `zread` MCP servers
/// when the active provider is GLM/Z.AI on the Coding Plan and an API key is
/// available. Z.AI serves them to Coding Plan keys only.
///
/// Existing servers with the same names are left untouched so user config
/// always takes precedence. Their timeouts are the global `[mcp]` ones in
/// `defaults` where set, else the built-in ones.
pub fn inject_zai_mcp_servers(
    provider: &str,
    billing: Billing,
    api_key: Option<&str>,
    region: Region,
    servers: &mut Vec<McpServerConfig>,
    defaults: &McpDefaults,
) {
    if provider != "glm" && provider != "zai" {
        return;
    }
    if billing != Billing::Subscription {
        return;
    }

    let Some(api_key) = api_key else {
        return;
    };

    for hosted in &HOSTED_SERVERS {
        if servers.iter().any(|s| s.name == hosted.name) {
            continue;
        }
        servers.push(McpServerConfig {
            name: hosted.name.to_string(),
            command: None,
            args: Vec::new(),
            url: Some(format!("{}/{}/mcp", mcp_root(region), hosted.path)),
            token: Some(api_key.to_string()),
            startup_timeout_ms: defaults.startup_timeout_ms,
            tool_timeout_ms: defaults.tool_timeout_ms,
            disabled: false,
            oauth: None,
            deferred: hosted.deferred.then_some(true),
            // Searching the web, reading a page and reading a public
            // repository change nothing, but Z.AI does not declare it: the
            // read-only sub-agents would go without them.
            read_only: true,
            env: Default::default(),
            cwd: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Z.AI serves its MCP servers to Coding Plan keys only.
    #[test]
    fn a_pay_per_token_key_gets_no_hosted_servers() {
        let mut servers = Vec::new();
        inject_zai_mcp_servers(
            "glm",
            Billing::Api,
            Some("secret"),
            Region::Global,
            &mut servers,
            &McpDefaults::default(),
        );
        assert!(servers.is_empty());
    }

    fn url_of<'a>(servers: &'a [McpServerConfig], name: &str) -> &'a str {
        servers
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.url.as_deref())
            .unwrap_or_else(|| panic!("{name} is injected"))
    }

    #[test]
    fn test_injects_servers_for_glm() {
        let mut servers = Vec::new();
        inject_zai_mcp_servers(
            "glm",
            Billing::Subscription,
            Some("secret"),
            Region::Global,
            &mut servers,
            &McpDefaults::default(),
        );
        assert_eq!(servers.len(), 3);
        assert_eq!(
            url_of(&servers, "zai_web_search"),
            "https://api.z.ai/api/mcp/web_search_prime/mcp"
        );
        assert_eq!(
            url_of(&servers, "zai_web_reader"),
            "https://api.z.ai/api/mcp/web_reader/mcp"
        );
        assert_eq!(
            url_of(&servers, "zai_zread"),
            "https://api.z.ai/api/mcp/zread/mcp"
        );
        assert!(servers.iter().all(|s| s.token.as_deref() == Some("secret")));
    }

    /// Zread's tools load on demand; search and reader stay listed.
    #[test]
    fn zread_is_deferred() {
        let mut servers = Vec::new();
        inject_zai_mcp_servers(
            "glm",
            Billing::Subscription,
            Some("secret"),
            Region::Global,
            &mut servers,
            &McpDefaults::default(),
        );
        for server in &servers {
            let expected = (server.name == "zai_zread").then_some(true);
            assert_eq!(server.deferred, expected, "{}", server.name);
        }
    }

    /// A mainland key goes to open.bigmodel.cn; api.z.ai refuses it.
    #[test]
    fn a_mainland_key_uses_the_bigmodel_endpoints() {
        let mut servers = Vec::new();
        inject_zai_mcp_servers(
            "zai",
            Billing::Subscription,
            Some("secret"),
            Region::China,
            &mut servers,
            &McpDefaults::default(),
        );
        assert_eq!(
            url_of(&servers, "zai_web_search"),
            "https://open.bigmodel.cn/api/mcp/web_search_prime/mcp"
        );
        assert_eq!(
            url_of(&servers, "zai_web_reader"),
            "https://open.bigmodel.cn/api/mcp/web_reader/mcp"
        );
        assert_eq!(
            url_of(&servers, "zai_zread"),
            "https://open.bigmodel.cn/api/mcp/zread/mcp"
        );
    }

    #[test]
    fn test_skips_injection_for_kimi() {
        let mut servers = Vec::new();
        inject_zai_mcp_servers(
            "kimi",
            Billing::Subscription,
            Some("secret"),
            Region::Global,
            &mut servers,
            &McpDefaults::default(),
        );
        assert!(servers.is_empty());
    }

    #[test]
    fn test_skips_injection_without_api_key() {
        let mut servers = Vec::new();
        inject_zai_mcp_servers(
            "glm",
            Billing::Subscription,
            None,
            Region::Global,
            &mut servers,
            &McpDefaults::default(),
        );
        assert!(servers.is_empty());
    }

    #[test]
    fn test_preserves_existing_servers() {
        let mut servers = vec![McpServerConfig {
            name: "zai_web_search".to_string(),
            command: None,
            args: Vec::new(),
            url: Some("https://example.com".to_string()),
            token: Some("old".to_string()),
            startup_timeout_ms: Some(30_000),
            tool_timeout_ms: Some(60_000),
            disabled: false,
            oauth: None,
            deferred: None,
            read_only: false,
            env: Default::default(),
            cwd: None,
        }];
        inject_zai_mcp_servers(
            "glm",
            Billing::Subscription,
            Some("secret"),
            Region::Global,
            &mut servers,
            &McpDefaults::default(),
        );
        assert_eq!(servers.len(), 3);
        let existing = servers.iter().find(|s| s.name == "zai_web_search").unwrap();
        assert_eq!(existing.url.as_deref(), Some("https://example.com"));
    }

    /// The hosted servers only read, which their tools do not declare.
    #[test]
    fn the_hosted_servers_are_read_only() {
        let mut servers = Vec::new();
        inject_zai_mcp_servers(
            "glm",
            Billing::Subscription,
            Some("secret"),
            Region::Global,
            &mut servers,
            &McpDefaults::default(),
        );
        assert_eq!(servers.len(), 3);
        assert!(servers.iter().all(|server| server.read_only));
    }

    /// The global `[mcp]` timeouts reach the hosted servers as they do a
    /// configured one; unset, the built-in ones apply.
    #[test]
    fn the_global_timeouts_reach_the_hosted_servers() {
        let global = McpDefaults {
            startup_timeout_ms: Some(5_000),
            tool_timeout_ms: Some(600_000),
            ..McpDefaults::default()
        };
        for (defaults, startup, tool) in [
            (global, Some(5_000), Some(600_000)),
            (McpDefaults::default(), None, None),
        ] {
            let mut servers = Vec::new();
            inject_zai_mcp_servers(
                "glm",
                Billing::Subscription,
                Some("secret"),
                Region::Global,
                &mut servers,
                &defaults,
            );
            assert_eq!(servers.len(), 3);
            for server in &servers {
                assert_eq!(server.startup_timeout_ms, startup, "{}", server.name);
                assert_eq!(server.tool_timeout_ms, tool, "{}", server.name);
            }
        }
    }
}
