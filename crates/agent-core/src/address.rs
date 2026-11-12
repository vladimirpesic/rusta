//! Which network an address belongs to, for requests to URLs someone else chose.
//!
//! The `fetch` tool requests URLs a model chose, and an MCP server's sign-in
//! requests URLs the server's metadata names. Neither may reach a service that
//! only the user's machine or network can: a local daemon, an intranet host, or
//! a cloud metadata endpoint. Their hosts are resolved, every address is
//! classified here, and the connection is made only to addresses the caller
//! accepted.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The network an address belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressClass {
    /// Reachable from the internet.
    Public,
    /// This machine: 127.0.0.0/8 and `::1`.
    Loopback,
    /// A private network: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, the
    /// carrier-grade NAT range 100.64.0.0/10, and unique local `fc00::/7`.
    Private,
    /// The local link, where cloud metadata services answer: 169.254.0.0/16
    /// and `fe80::/10`.
    LinkLocal,
    /// No host: 0.0.0.0/8, `::` and 255.255.255.255.
    Unroutable,
}

impl AddressClass {
    /// The class of `ip`. An IPv4-mapped IPv6 address (`::ffff:127.0.0.1`)
    /// reaches the same host as its IPv4 address, so it is judged as that.
    #[must_use]
    pub fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => Self::of_v4(v4),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => Self::of_v4(v4),
                None => Self::of_v6(v6),
            },
        }
    }

    fn of_v4(ip: Ipv4Addr) -> Self {
        let [a, b, ..] = ip.octets();
        if ip.is_loopback() {
            Self::Loopback
        } else if ip.is_private() || (a == 100 && (64..128).contains(&b)) {
            Self::Private
        } else if ip.is_link_local() {
            Self::LinkLocal
        } else if a == 0 || ip.is_broadcast() {
            Self::Unroutable
        } else {
            Self::Public
        }
    }

    fn of_v6(ip: Ipv6Addr) -> Self {
        let first = ip.segments()[0];
        if ip.is_loopback() {
            Self::Loopback
        } else if (first & 0xfe00) == 0xfc00 {
            Self::Private
        } else if (first & 0xffc0) == 0xfe80 {
            Self::LinkLocal
        } else if ip.is_unspecified() {
            Self::Unroutable
        } else {
            Self::Public
        }
    }

    /// Whether the address is anything but public.
    #[must_use]
    pub fn is_internal(self) -> bool {
        self != Self::Public
    }
}

impl fmt::Display for AddressClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Public => "public",
            Self::Loopback => "loopback",
            Self::Private => "private",
            Self::LinkLocal => "link-local",
            Self::Unroutable => "unroutable",
        })
    }
}

/// Why [`resolve_allowed`] gave no addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressError {
    /// The URL has no host or port, or its host did not resolve.
    Unresolved(String),
    /// The host resolves to an address of a class the caller refused.
    Refused {
        host: String,
        ip: IpAddr,
        class: AddressClass,
    },
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolved(reason) => f.write_str(reason),
            Self::Refused { host, ip, class } => {
                write!(f, "{host} resolves to the {class} address {ip}")
            }
        }
    }
}

impl std::error::Error for AddressError {}

/// Resolve the host of `url` and return its addresses, if `allow` accepts the
/// class of every one of them. The caller connects only to these addresses
/// (`reqwest::ClientBuilder::resolve_to_addrs`), so a DNS answer that changes
/// between this check and the connection cannot slip past it.
pub async fn resolve_allowed(
    url: &url::Url,
    allow: impl Fn(AddressClass) -> bool,
) -> Result<Vec<SocketAddr>, AddressError> {
    let host = url
        .host_str()
        .ok_or_else(|| AddressError::Unresolved(format!("{url} has no host")))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| AddressError::Unresolved(format!("{url} has no port")))?;
    // `host_str` keeps an IPv6 literal's brackets, which a lookup rejects.
    let lookup = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((lookup, port))
        .await
        .map_err(|e| AddressError::Unresolved(format!("could not resolve {host}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(AddressError::Unresolved(format!(
            "could not resolve {host}"
        )));
    }
    for addr in &addrs {
        let class = AddressClass::of(addr.ip());
        if !allow(class) {
            return Err(AddressError::Refused {
                host: host.to_string(),
                ip: addr.ip(),
                class,
            });
        }
    }
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(ip: &str) -> AddressClass {
        AddressClass::of(ip.parse().unwrap())
    }

    #[test]
    fn addresses_fall_into_their_networks() {
        for (ip, expected) in [
            ("127.0.0.1", AddressClass::Loopback),
            ("127.8.9.10", AddressClass::Loopback),
            ("::1", AddressClass::Loopback),
            ("::ffff:127.0.0.1", AddressClass::Loopback),
            ("10.1.2.3", AddressClass::Private),
            ("172.16.5.4", AddressClass::Private),
            ("192.168.0.1", AddressClass::Private),
            ("100.64.1.1", AddressClass::Private),
            ("fc00::1", AddressClass::Private),
            ("fd12:3456::1", AddressClass::Private),
            ("169.254.169.254", AddressClass::LinkLocal),
            ("::ffff:169.254.169.254", AddressClass::LinkLocal),
            ("fe80::1", AddressClass::LinkLocal),
            ("0.0.0.0", AddressClass::Unroutable),
            ("0.1.2.3", AddressClass::Unroutable),
            ("255.255.255.255", AddressClass::Unroutable),
            ("::", AddressClass::Unroutable),
            ("8.8.8.8", AddressClass::Public),
            ("93.184.216.34", AddressClass::Public),
            ("2606:4700:4700::1111", AddressClass::Public),
        ] {
            assert_eq!(class(ip), expected, "{ip}");
            assert_eq!(
                class(ip).is_internal(),
                expected != AddressClass::Public,
                "{ip}"
            );
        }
    }

    /// Carrier-grade NAT is exactly 100.64.0.0/10: the 100.x addresses on
    /// either side of it are public.
    #[test]
    fn the_cgnat_range_is_exact() {
        assert_eq!(class("100.64.0.0"), AddressClass::Private);
        assert_eq!(class("100.127.255.255"), AddressClass::Private);
        assert_eq!(class("100.63.255.255"), AddressClass::Public);
        assert_eq!(class("100.128.0.0"), AddressClass::Public);
    }

    #[tokio::test]
    async fn a_refused_class_names_the_address() {
        let url = url::Url::parse("http://127.0.0.1:9/").unwrap();
        let err = resolve_allowed(&url, |c| !c.is_internal())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "127.0.0.1 resolves to the loopback address 127.0.0.1"
        );
        assert!(matches!(
            err,
            AddressError::Refused {
                class: AddressClass::Loopback,
                ..
            }
        ));
        let addrs = resolve_allowed(&url, |_| true).await.unwrap();
        assert_eq!(addrs, vec!["127.0.0.1:9".parse::<SocketAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn an_ipv6_literal_is_resolved_without_its_brackets() {
        let url = url::Url::parse("http://[::1]:9/").unwrap();
        let err = resolve_allowed(&url, |c| !c.is_internal())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "[::1] resolves to the loopback address ::1"
        );
    }
}
