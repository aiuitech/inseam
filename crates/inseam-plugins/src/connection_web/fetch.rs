//! The web host's fetch path: one guarded HTTP request at a time, with the
//! decisions a node must make before it contacts anything on the open
//! web — is this host allowed, is its address public, how far may a
//! redirect carry us, how much may come back — kept as pure helpers here
//! so they can be tested without a socket.
//!
//! The guard is the node's SSRF posture (`design/connections.md`): a node
//! indexes personal data and, hosted, sits beside internal services, so a
//! link in an indexed document must never turn the node into a proxy for
//! reaching them. Every hop resolves its host first, refuses any address
//! that is not globally routable unless the owner allow-listed that host
//! by name, and pins the connection to the addresses it checked, so a DNS
//! answer cannot change between the check and the connect.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::redirect::Policy;
use reqwest::Method;
use url::Url;

use inseam_kernel::fragment::Mimetype;
use inseam_seams::SeamError;

/// Most redirect hops any single fetch may follow, whatever the config
/// asks: past this a chain is a loop or a trap.
pub(crate) const REDIRECTS_MAX_CEILING: u32 = 10;

/// A host the owner allowed by name: an exact host, or a domain and
/// everything under it (`*.example.com` matches `example.com` and
/// `img.example.com`). Comparison is case-insensitive and ignores a
/// trailing dot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostPattern {
    Exact(String),
    Domain(String),
}

impl HostPattern {
    pub(crate) fn parse(pattern: &str) -> Result<Self, String> {
        let pattern = normalize_host(pattern);
        if pattern.is_empty() {
            return Err("an allow_hosts entry may not be empty".to_string());
        }
        match pattern.strip_prefix("*.") {
            Some(domain) if domain.is_empty() || domain.contains('*') => {
                Err(format!("`{pattern}` is not a host or a `*.domain` pattern"))
            }
            Some(domain) => Ok(Self::Domain(domain.to_string())),
            None if pattern.contains('*') => {
                Err(format!("`{pattern}` is not a host or a `*.domain` pattern"))
            }
            None => Ok(Self::Exact(pattern)),
        }
    }

    pub(crate) fn matches(&self, host: &str) -> bool {
        let host = normalize_host(host);
        match self {
            Self::Exact(exact) => host == *exact,
            Self::Domain(domain) => {
                host == *domain
                    || host
                        .strip_suffix(domain.as_str())
                        .is_some_and(|prefix| prefix.ends_with('.'))
            }
        }
    }
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Whether an address is globally routable — the only kind a link in a
/// document may lead the node to unless its host was allow-listed by name.
/// Spelled out range by range rather than through the standard library's
/// unstable `is_global`, and covering the ranges that matter for a node:
/// loopback, private and carrier-grade NAT, link-local, the unspecified
/// and broadcast addresses, multicast, documentation, and everything
/// reserved above 240/4.
pub(crate) fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    let carrier_grade_nat = a == 100 && (64..=127).contains(&b);
    let protocol_assignments = a == 192 && b == 0 && ip.octets()[2] == 0;
    let benchmarking = a == 198 && (18..=19).contains(&b);
    let reserved = a >= 240;
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || carrier_grade_nat
        || protocol_assignments
        || benchmarking
        || reserved)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    let unique_local = (segments[0] & 0xfe00) == 0xfc00;
    let link_local = (segments[0] & 0xffc0) == 0xfe80;
    let documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
    !(ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || unique_local || link_local || documentation)
}

/// What one request answered: the content type and length the headers
/// declared, and the body when one was asked for.
#[derive(Debug, Clone)]
pub(crate) struct Fetched {
    pub(crate) content_type: Mimetype,
    pub(crate) content_length: Option<u64>,
    pub(crate) bytes: Vec<u8>,
}

/// The guarded fetcher: the config's dials, compiled once.
pub(crate) struct Fetcher {
    allow_hosts: Vec<HostPattern>,
    content_bytes_max: u64,
    timeout: Duration,
    redirects_max: u32,
    user_agent: String,
}

impl Fetcher {
    pub(crate) fn new(
        allow_hosts: Vec<HostPattern>,
        content_bytes_max: u64,
        timeout: Duration,
        redirects_max: u32,
        user_agent: String,
    ) -> Self {
        Self {
            allow_hosts,
            content_bytes_max,
            timeout,
            redirects_max: redirects_max.min(REDIRECTS_MAX_CEILING),
            user_agent,
        }
    }

    pub(crate) fn content_bytes_max(&self) -> u64 {
        self.content_bytes_max
    }

    /// The headers of a resource without its body: `HEAD`, falling back to
    /// a `GET` whose body is dropped unread when the server refuses `HEAD`.
    pub(crate) async fn head(&self, url: &Url) -> Result<Fetched, SeamError> {
        match self.request(url, Method::HEAD, BodyWant::None).await {
            Ok(fetched) => Ok(fetched),
            Err(_) => self.request(url, Method::GET, BodyWant::None).await,
        }
    }

    /// The resource, body included, under the byte cap.
    pub(crate) async fn get(&self, url: &Url) -> Result<Fetched, SeamError> {
        self.request(url, Method::GET, BodyWant::Whole).await
    }

    /// The resource's body through its first `lines` lines, under the byte
    /// cap; the connection is dropped once they are in hand. The body ends
    /// mid-line when it is cut, so callers slice by line, never by byte.
    pub(crate) async fn get_lines(&self, url: &Url, lines: u64) -> Result<Fetched, SeamError> {
        assert!(lines >= 1);
        self.request(url, Method::GET, BodyWant::Lines(lines)).await
    }

    /// Follow at most `redirects_max` hops, guarding every one, and read
    /// the final answer.
    async fn request(&self, url: &Url, method: Method, want: BodyWant) -> Result<Fetched, SeamError> {
        let hops_max = self.redirects_max + 1;
        let mut current = url.clone();
        for hop in 0..hops_max {
            assert!(hop <= REDIRECTS_MAX_CEILING);
            let response = self.send_once(&current, method.clone()).await?;
            if response.status().is_redirection() {
                current = redirect_target(&current, &response)?;
                continue;
            }
            if !response.status().is_success() {
                return Err(SeamError::failed(format!(
                    "{method} {current}: status {}",
                    response.status()
                )));
            }
            return self.finish(current, response, want).await;
        }
        Err(SeamError::failed(format!(
            "{method} {url}: more than {} redirects",
            self.redirects_max
        )))
    }

    /// One hop: the guard, then the request, with no automatic redirects
    /// so every hop comes back through the guard.
    async fn send_once(&self, url: &Url, method: Method) -> Result<reqwest::Response, SeamError> {
        let host = guard_url(url)?;
        let allow_listed = self.allow_hosts.iter().any(|p| p.matches(host));
        if !self.allow_hosts.is_empty() && !allow_listed {
            return Err(SeamError::Refused(format!(
                "host `{host}` is not in the web connection's allow_hosts"
            )));
        }
        let port = url.port_or_known_default().unwrap_or(80);
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| SeamError::failed(format!("resolving {host}: {e}")))?
            .collect();
        if addresses.is_empty() {
            return Err(SeamError::failed(format!("resolving {host}: no addresses")));
        }
        // An allow-listed host may be private (a LAN image server, a test
        // fixture); anything else must be globally routable.
        if !allow_listed {
            if let Some(private) = addresses.iter().find(|a| !is_public_ip(a.ip())) {
                return Err(SeamError::Refused(format!(
                    "host `{host}` resolves to {}, which is not a public address",
                    private.ip()
                )));
            }
        }
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(self.timeout)
            .user_agent(&self.user_agent)
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|e| SeamError::failed(format!("http client: {e}")))?;
        client
            .request(method.clone(), url.clone())
            .send()
            .await
            .map_err(|e| SeamError::failed(format!("{method} {url}: {e}")))
    }

    /// Read the headers, and the body under the cap when asked.
    async fn finish(&self, url: Url, response: reqwest::Response, want: BodyWant) -> Result<Fetched, SeamError> {
        let content_type = content_type_of(&response);
        // The declared header, not reqwest's body size hint: a `HEAD`
        // answer has no body, and its hint says zero.
        let content_length = declared_length_of(&response);
        // A line-bounded read stops early, so only what it actually reads
        // counts against the cap: the head of a huge log is scannable even
        // though the whole log is not fetchable.
        let refuse_by_declared_length = match want {
            BodyWant::None | BodyWant::Whole => true,
            BodyWant::Lines(_) => false,
        };
        if let Some(length) = content_length
            && length > self.content_bytes_max
            && refuse_by_declared_length
        {
            return Err(SeamError::Refused(format!(
                "{url} is {length} bytes; the web connection reads at most {}",
                self.content_bytes_max
            )));
        }
        let bytes = match want {
            BodyWant::None => Vec::new(),
            BodyWant::Whole => read_body(&url, response, self.content_bytes_max, None).await?,
            BodyWant::Lines(lines) => {
                read_body(&url, response, self.content_bytes_max, Some(lines)).await?
            }
        };
        Ok(Fetched {
            content_type,
            content_length,
            bytes,
        })
    }
}

/// The scheme and host checks that need no network: `http` or `https`,
/// with a host name present. Returns the host.
fn guard_url(url: &Url) -> Result<&str, SeamError> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(SeamError::Refused(format!(
            "{url}: only http and https are fetched"
        )));
    }
    url.host_str()
        .ok_or_else(|| SeamError::Refused(format!("{url} names no host")))
}

/// Where a redirect points, resolved against the hop that answered it.
fn redirect_target(from: &Url, response: &reqwest::Response) -> Result<Url, SeamError> {
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| SeamError::failed(format!("{from}: redirect without a location")))?;
    from.join(location)
        .map_err(|e| SeamError::failed(format!("{from}: redirect to `{location}`: {e}")))
}

/// The declared content type's essence, or `application/octet-stream`
/// when the server declared none or nonsense. Parameters (`charset`) are
/// dropped: the essence is what claims dispatch on.
fn content_type_of(response: &reqwest::Response) -> Mimetype {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .and_then(|essence| Mimetype::parse(essence.trim()).ok())
        .unwrap_or_else(|| Mimetype::parse("application/octet-stream").expect("literal mimetype is valid"))
}

/// The `Content-Length` the server declared, when it declared one that
/// parses.
fn declared_length_of(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Read a body chunk by chunk, refusing the moment it passes the cap, so
/// an undeclared or lying length never fills memory.
/// How much of a response body a request wants: none (a `HEAD`), all of
/// it, or only through its first n lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyWant {
    None,
    Whole,
    Lines(u64),
}

/// The body under the cap. With `lines_wanted`, reading stops at the chunk
/// that completes that many lines — the count of `\n` seen — and the rest
/// of the body never arrives.
async fn read_body(
    url: &Url,
    mut response: reqwest::Response,
    cap: u64,
    lines_wanted: Option<u64>,
) -> Result<Vec<u8>, SeamError> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut newlines: u64 = 0;
    // Every chunk carries at least one byte, so the loop is bounded by the
    // cap plus one refused chunk.
    let mut chunks: u64 = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| SeamError::failed(format!("reading {url}: {e}")))?
    {
        chunks += 1;
        assert!(chunks <= cap.saturating_add(1), "each chunk carries at least one byte");
        bytes.extend_from_slice(&chunk);
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > cap {
            return Err(SeamError::Refused(format!(
                "{url} exceeds the web connection's {cap} byte cap"
            )));
        }
        newlines += u64::try_from(chunk.iter().filter(|b| **b == b'\n').count()).unwrap_or(u64::MAX);
        if lines_wanted.is_some_and(|wanted| newlines >= wanted) {
            break;
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_patterns_match_exact_hosts_and_domains() {
        let exact = HostPattern::parse("Example.com.").expect("parses");
        assert_eq!(exact, HostPattern::Exact("example.com".into()));
        assert!(exact.matches("EXAMPLE.com"));
        assert!(!exact.matches("img.example.com"));

        let domain = HostPattern::parse("*.example.com").expect("parses");
        assert!(domain.matches("example.com"));
        assert!(domain.matches("img.example.com"));
        assert!(domain.matches("a.b.example.com"));
        assert!(!domain.matches("notexample.com"));
        assert!(!domain.matches("example.com.evil"));
    }

    #[test]
    fn host_patterns_reject_shapeless_entries() {
        for bad in ["", "*", "*.", "a*b.com", "*.*.com"] {
            assert!(HostPattern::parse(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn public_addresses_exclude_every_local_range() {
        let private = [
            "127.0.0.1", "10.1.2.3", "172.16.0.9", "192.168.1.1", "169.254.169.254",
            "0.0.0.0", "255.255.255.255", "224.0.0.1", "100.64.0.1", "100.127.255.254",
            "192.0.0.8", "192.0.2.1", "198.18.0.1", "240.0.0.1", "::1", "::", "fc00::1",
            "fd12::1", "fe80::1", "ff02::1", "2001:db8::1", "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
        ];
        for ip in private {
            let ip: IpAddr = ip.parse().expect("valid ip");
            assert!(!is_public_ip(ip), "{ip} is not public");
        }
        let public = ["93.184.216.34", "8.8.8.8", "100.128.0.1", "2606:4700::1111", "::ffff:8.8.8.8"];
        for ip in public {
            let ip: IpAddr = ip.parse().expect("valid ip");
            assert!(is_public_ip(ip), "{ip} is public");
        }
    }

    #[test]
    fn only_http_schemes_pass_the_url_guard() {
        assert_eq!(guard_url(&Url::parse("https://example.com/a.png").expect("url")).expect("host"), "example.com");
        assert!(guard_url(&Url::parse("ftp://example.com/a").expect("url")).is_err());
        assert!(guard_url(&Url::parse("file:///etc/passwd").expect("url")).is_err());
        assert!(guard_url(&Url::parse("data:image/png;base64,AAAA").expect("url")).is_err());
    }

    #[test]
    fn the_redirect_ceiling_bounds_the_config() {
        let fetcher = Fetcher::new(Vec::new(), 1, Duration::from_secs(1), 500, "ua".into());
        assert_eq!(fetcher.redirects_max, REDIRECTS_MAX_CEILING);
    }
}
