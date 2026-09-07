//! The transport's endpoint vocabulary (`design/roster.md`): what a node
//! record's `endpoints` carry for this transport, rendered from iroh's own
//! address and parsed back into it. `relay:<url>` names the relay a peer is
//! reachable through, `ip:<socketaddr>` a direct address. Unknown kinds are
//! skipped with a warning, never fatal: a newer transport may publish hints
//! an older one cannot read, and the readable ones still dial.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};

use iroh::{EndpointAddr, RelayUrl, TransportAddr};

use inseam_kernel::network::{ENDPOINTS_MAX, Endpoint};

/// The kind prefix of a relay hint: `relay:https://relay.example`.
pub const RELAY_KIND: &str = "relay";
/// The kind prefix of a direct-address hint: `ip:203.0.113.5:7000`.
pub const IP_KIND: &str = "ip";

/// Render iroh's view of this node into roster endpoints: relays first
/// (the reliable path), then direct addresses. Loopback and link-local
/// addresses are kept only when they are all there is — two nodes on one
/// machine, or a LAN with no route out, can still find each other — and
/// never beside routable ones, where they would only be noise replicated
/// to every node. Bounded by the roster's [`ENDPOINTS_MAX`].
pub fn render(addr: &EndpointAddr) -> Vec<Endpoint> {
    let relays = addr
        .relay_urls()
        .filter_map(|url| endpoint_of(format!("{RELAY_KIND}:{url}")));
    let (routable, local): (Vec<SocketAddr>, Vec<SocketAddr>) = addr
        .ip_addrs()
        .copied()
        .partition(|socket| is_routable(socket.ip()));
    let direct = if routable.is_empty() { local } else { routable };
    let rendered: Vec<Endpoint> = relays
        .chain(
            direct
                .into_iter()
                .filter_map(|socket| endpoint_of(format!("{IP_KIND}:{socket}"))),
        )
        .take(ENDPOINTS_MAX)
        .collect();
    assert!(rendered.len() <= ENDPOINTS_MAX, "the roster bound holds");
    rendered
}

/// Parse a peer's roster endpoints into dialing hints. Unreadable ones are
/// skipped with a warning; the caller decides what an empty set means.
pub fn parse(endpoints: &[Endpoint]) -> BTreeSet<TransportAddr> {
    let mut addrs = BTreeSet::new();
    // A record never carries more than the bound, but a caller-built
    // address might; the tail past it is ignored rather than trusted.
    for endpoint in endpoints.iter().take(ENDPOINTS_MAX) {
        match parse_one(endpoint.as_str()) {
            Ok(addr) => {
                addrs.insert(addr);
            }
            Err(reason) => {
                tracing::warn!(endpoint = %endpoint, "skipping endpoint: {reason}");
            }
        }
    }
    assert!(addrs.len() <= ENDPOINTS_MAX);
    addrs
}

fn parse_one(text: &str) -> Result<TransportAddr, String> {
    let (kind, value) = text
        .split_once(':')
        .ok_or_else(|| "no `<kind>:` prefix".to_string())?;
    match kind {
        RELAY_KIND => value
            .parse::<RelayUrl>()
            .map(TransportAddr::Relay)
            .map_err(|e| format!("relay url `{value}`: {e}")),
        IP_KIND => value
            .parse::<SocketAddr>()
            .map(TransportAddr::Ip)
            .map_err(|e| format!("socket address `{value}`: {e}")),
        other => Err(format!("unknown endpoint kind `{other}`")),
    }
}

fn endpoint_of(text: String) -> Option<Endpoint> {
    match Endpoint::new(text) {
        Ok(endpoint) => Some(endpoint),
        Err(e) => {
            tracing::warn!("not publishing an endpoint: {e}");
            None
        }
    }
}

/// Whether an address means anything past this machine and its link.
fn is_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unicast_link_local() && !v6.is_unspecified(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::PublicKey;

    fn id() -> PublicKey {
        iroh::SecretKey::from_bytes(&[9; 32]).public()
    }

    fn socket(text: &str) -> SocketAddr {
        text.parse().expect("socket address")
    }

    fn relay(text: &str) -> RelayUrl {
        text.parse().expect("relay url")
    }

    #[test]
    fn rendering_round_trips_through_the_parser() {
        let addr = EndpointAddr::from_parts(
            id(),
            [
                TransportAddr::Ip(socket("203.0.113.5:7000")),
                TransportAddr::Relay(relay("https://relay.example/")),
                TransportAddr::Ip(socket("[2001:db8::1]:7000")),
            ],
        );
        let rendered = render(&addr);
        let texts: Vec<&str> = rendered.iter().map(Endpoint::as_str).collect();
        assert_eq!(
            texts,
            vec![
                "relay:https://relay.example/",
                "ip:203.0.113.5:7000",
                "ip:[2001:db8::1]:7000"
            ],
            "relays first, then direct addresses"
        );
        assert_eq!(parse(&rendered), addr.addrs);
    }

    #[test]
    fn local_addresses_are_dropped_beside_routable_ones_and_kept_alone() {
        let mixed = EndpointAddr::from_parts(
            id(),
            [
                TransportAddr::Ip(socket("127.0.0.1:1")),
                TransportAddr::Ip(socket("169.254.1.2:1")),
                TransportAddr::Ip(socket("[fe80::1]:1")),
                TransportAddr::Ip(socket("[::1]:1")),
                TransportAddr::Ip(socket("192.168.1.9:1")),
            ],
        );
        let texts: Vec<String> = render(&mixed).iter().map(ToString::to_string).collect();
        assert_eq!(texts, vec!["ip:192.168.1.9:1"]);

        let alone = EndpointAddr::from_parts(
            id(),
            [
                TransportAddr::Ip(socket("127.0.0.1:1")),
                TransportAddr::Ip(socket("[::1]:1")),
            ],
        );
        let texts: Vec<String> = render(&alone).iter().map(ToString::to_string).collect();
        assert_eq!(texts, vec!["ip:127.0.0.1:1", "ip:[::1]:1"]);
    }

    #[test]
    fn rendering_is_bounded() {
        let many = EndpointAddr::from_parts(
            id(),
            (0..40u16).map(|port| TransportAddr::Ip(socket(&format!("10.0.0.1:{}", port + 1)))),
        );
        assert_eq!(render(&many).len(), ENDPOINTS_MAX);
    }

    #[test]
    fn unknown_kinds_and_malformed_values_are_skipped() {
        let endpoints = vec![
            Endpoint::new("ip:203.0.113.5:7000").expect("valid"),
            Endpoint::new("tor:abc").expect("valid"),
            Endpoint::new("ip:not-an-address").expect("valid"),
            Endpoint::new("relay:::nope").expect("valid"),
            Endpoint::new("no-prefix").expect("valid"),
        ];
        let parsed = parse(&endpoints);
        assert_eq!(parsed.len(), 1);
        assert!(parsed.contains(&TransportAddr::Ip(socket("203.0.113.5:7000"))));
        assert!(parse(&[]).is_empty());
    }
}
