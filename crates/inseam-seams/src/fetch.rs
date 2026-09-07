//! The node's guarded HTTP fetch: one request at a time, with the decisions
//! a node must make before it contacts anything on the open web — is this
//! host allowed, is its address public, how far may a redirect carry us,
//! how much may come back — kept as pure helpers so they can be tested
//! without a socket.
//!
//! The guard is the node's SSRF posture (`design/connections.md`): a node
//! indexes personal data and, hosted, sits beside internal services, so
//! neither a link in an indexed document nor a loaded plugin describing a
//! request must ever turn the node into a proxy for reaching them. Every
//! hop resolves its host first, refuses any address that is not globally
//! routable unless the owner allow-listed that host by name, and pins the
//! connection to the addresses it checked, so a DNS answer cannot change
//! between the check and the connect. It lives beside the seams because two
//! plugins speak it — the web connection, and the plugin-host bridge that
//! performs requests on a sandboxed component's behalf — and neither may
//! own the other's posture.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::Method;
use reqwest::redirect::Policy;
use url::Url;

use inseam_kernel::fragment::Mimetype;

use crate::SeamError;

/// Most redirect hops any single fetch may follow, whatever the config
/// asks: past this a chain is a loop or a trap.
pub const REDIRECTS_MAX_CEILING: u32 = 10;

/// A host the owner allowed by name: an exact host, or a domain and
/// everything under it (`*.example.com` matches `example.com` and
/// `img.example.com`). Comparison is case-insensitive and ignores a
/// trailing dot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPattern {
    Exact(String),
    Domain(String),
}

impl HostPattern {
    pub fn parse(pattern: &str) -> Result<Self, String> {
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

    pub fn matches(&self, host: &str) -> bool {
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
pub fn is_public_ip(ip: IpAddr) -> bool {
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
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || unique_local
        || link_local
        || documentation)
}

/// What one request answered: the content type and length the headers
/// declared, and the body when one was asked for.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub content_type: Mimetype,
    pub content_length: Option<u64>,
    pub bytes: Vec<u8>,
}

/// The methods a described request may use: the HTTP verbs a host API is
/// spoken with, and nothing exotic. Parsed once at the boundary so the
/// transport never sees an arbitrary string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMethod {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

impl FetchMethod {
    fn as_reqwest(self) -> Method {
        match self {
            Self::Get => Method::GET,
            Self::Head => Method::HEAD,
            Self::Post => Method::POST,
            Self::Put => Method::PUT,
            Self::Patch => Method::PATCH,
            Self::Delete => Method::DELETE,
        }
    }
}

impl TryFrom<&str> for FetchMethod {
    type Error = SeamError;

    fn try_from(method: &str) -> Result<Self, Self::Error> {
        match method.to_ascii_uppercase().as_str() {
            "GET" => Ok(Self::Get),
            "HEAD" => Ok(Self::Head),
            "POST" => Ok(Self::Post),
            "PUT" => Ok(Self::Put),
            "PATCH" => Ok(Self::Patch),
            "DELETE" => Ok(Self::Delete),
            other => Err(SeamError::Refused(format!(
                "method `{other}` is not one of GET, HEAD, POST, PUT, PATCH, DELETE"
            ))),
        }
    }
}

/// Request headers the transport owns; a caller naming one is describing
/// the wire, not the request, and is refused.
const HEADERS_TRANSPORT_OWNED: [&str; 4] =
    ["host", "content-length", "transfer-encoding", "connection"];

/// Most headers one described request may carry.
pub const REQUEST_HEADERS_MAX: usize = 32;

/// A request described by a caller — a loaded plugin, in the usual case —
/// for the node to perform on its behalf: the caller names the method,
/// URL, headers, and body; the node decides whether the host may be
/// contacted at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub method: FetchMethod,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl FetchRequest {
    /// Header hygiene: bounded, no transport-owned names, no control
    /// characters in names or values (a `\r\n` in a value is a request
    /// smuggled inside a request).
    pub fn validate(&self) -> Result<(), SeamError> {
        if self.headers.len() > REQUEST_HEADERS_MAX {
            return Err(SeamError::Refused(format!(
                "a request may carry at most {REQUEST_HEADERS_MAX} headers; this one has {}",
                self.headers.len()
            )));
        }
        for (name, value) in &self.headers {
            let name_ok = !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if !name_ok {
                return Err(SeamError::Refused(format!(
                    "header name `{name}` is malformed"
                )));
            }
            if HEADERS_TRANSPORT_OWNED.contains(&name.to_ascii_lowercase().as_str()) {
                return Err(SeamError::Refused(format!(
                    "header `{name}` belongs to the transport and may not be set by a request"
                )));
            }
            if value.bytes().any(|b| b < 0x20 || b == 0x7f) {
                return Err(SeamError::Refused(format!(
                    "header `{name}` carries a control character in its value"
                )));
            }
        }
        Ok(())
    }
}

/// What a described request answered, status included: a plugin speaking
/// a host API needs to read a 404 or a 429 for itself, so a non-success
/// status is an answer here, never an error. The body is under the cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The guarded fetcher: the config's dials, compiled once.
pub struct Fetcher {
    allow_hosts: Vec<HostPattern>,
    /// Whether an empty allow list means "any public host" (the web
    /// connection, where mounting the entry is the consent) or "no host at
    /// all" (a loaded plugin, whose manifest must name every host).
    empty_allows_public: bool,
    content_bytes_max: u64,
    timeout: Duration,
    redirects_max: u32,
    user_agent: String,
}

impl Fetcher {
    /// The web connection's posture: an empty allow list admits any host
    /// whose address is public.
    pub fn new(
        allow_hosts: Vec<HostPattern>,
        content_bytes_max: u64,
        timeout: Duration,
        redirects_max: u32,
        user_agent: String,
    ) -> Self {
        Self {
            allow_hosts,
            empty_allows_public: true,
            content_bytes_max,
            timeout,
            redirects_max: redirects_max.min(REDIRECTS_MAX_CEILING),
            user_agent,
        }
    }

    /// A plugin's posture: only the hosts its reviewed manifest names,
    /// and none when it names none.
    pub fn for_allowed_hosts_only(
        allow_hosts: Vec<HostPattern>,
        content_bytes_max: u64,
        timeout: Duration,
        redirects_max: u32,
        user_agent: String,
    ) -> Self {
        Self {
            empty_allows_public: false,
            ..Self::new(
                allow_hosts,
                content_bytes_max,
                timeout,
                redirects_max,
                user_agent,
            )
        }
    }

    pub fn content_bytes_max(&self) -> u64 {
        self.content_bytes_max
    }

    /// Whether the guard would let a request reach `host` by name, before
    /// any resolution — what an authoring surface can answer offline.
    pub fn allows_host(&self, host: &str) -> bool {
        let allow_listed = self.allow_hosts.iter().any(|p| p.matches(host));
        allow_listed || (self.allow_hosts.is_empty() && self.empty_allows_public)
    }

    /// The headers of a resource without its body: `HEAD`, falling back to
    /// a `GET` whose body is dropped unread when the server refuses `HEAD`.
    pub async fn head(&self, url: &Url) -> Result<Fetched, SeamError> {
        match self.request(url, Method::HEAD, BodyWant::None).await {
            Ok(fetched) => Ok(fetched),
            Err(_) => self.request(url, Method::GET, BodyWant::None).await,
        }
    }

    /// The resource, body included, under the byte cap.
    pub async fn get(&self, url: &Url) -> Result<Fetched, SeamError> {
        self.request(url, Method::GET, BodyWant::Whole).await
    }

    /// The resource's body through its first `lines` lines, under the byte
    /// cap; the connection is dropped once they are in hand. The body ends
    /// mid-line when it is cut, so callers slice by line, never by byte.
    pub async fn get_lines(&self, url: &Url, lines: u64) -> Result<Fetched, SeamError> {
        assert!(lines >= 1);
        self.request(url, Method::GET, BodyWant::Lines(lines)).await
    }

    /// Perform a described request. Every hop passes the guard; the
    /// caller's headers and body ride only on hops to the host the caller
    /// named, so a redirect elsewhere can never carry a credential with it
    /// (`extra` is the node's own contribution — a granted bearer token —
    /// and is bound by the same rule). The final status is returned as an
    /// answer; only the guard, the network, and the caps produce errors.
    pub async fn send(
        &self,
        request: &FetchRequest,
        extra: &[(String, String)],
    ) -> Result<FetchResponse, SeamError> {
        request.validate()?;
        let origin_host = guard_url(&request.url)?.to_string();
        let hops_max = self.redirects_max + 1;
        let mut current = request.url.clone();
        for hop in 0..hops_max {
            assert!(hop <= REDIRECTS_MAX_CEILING);
            let same_host = current
                .host_str()
                .is_some_and(|h| h.eq_ignore_ascii_case(&origin_host));
            let (headers, body): (Vec<(String, String)>, Option<Vec<u8>>) = if same_host {
                (
                    request.headers.iter().chain(extra).cloned().collect(),
                    request.body.clone(),
                )
            } else {
                (Vec::new(), None)
            };
            let response = self
                .send_once(&current, request.method.as_reqwest(), &headers, body)
                .await?;
            if response.status().is_redirection() {
                current = redirect_target(&current, &response)?;
                continue;
            }
            let status = response.status().as_u16();
            let response_headers: Vec<(String, String)> = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|v| (name.as_str().to_string(), v.to_string()))
                })
                .collect();
            let fetched = self.finish(current, response, BodyWant::Whole).await?;
            return Ok(FetchResponse {
                status,
                headers: response_headers,
                body: fetched.bytes,
            });
        }
        Err(SeamError::failed(format!(
            "{:?} {}: more than {} redirects",
            request.method, request.url, self.redirects_max
        )))
    }

    /// Follow at most `redirects_max` hops, guarding every one, and read
    /// the final answer.
    async fn request(
        &self,
        url: &Url,
        method: Method,
        want: BodyWant,
    ) -> Result<Fetched, SeamError> {
        let hops_max = self.redirects_max + 1;
        let mut current = url.clone();
        for hop in 0..hops_max {
            assert!(hop <= REDIRECTS_MAX_CEILING);
            let response = self.send_once(&current, method.clone(), &[], None).await?;
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
    async fn send_once(
        &self,
        url: &Url,
        method: Method,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<reqwest::Response, SeamError> {
        let host = guard_url(url)?;
        let allow_listed = self.allow_hosts.iter().any(|p| p.matches(host));
        if !self.allows_host(host) {
            return Err(SeamError::Refused(format!(
                "host `{host}` is not in the allowed hosts"
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
        if !allow_listed && let Some(private) = addresses.iter().find(|a| !is_public_ip(a.ip())) {
            return Err(SeamError::Refused(format!(
                "host `{host}` resolves to {}, which is not a public address",
                private.ip()
            )));
        }
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(self.timeout)
            .user_agent(&self.user_agent)
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|e| SeamError::failed(format!("http client: {e}")))?;
        let mut builder = client.request(method.clone(), url.clone());
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = body {
            builder = builder.body(body);
        }
        builder
            .send()
            .await
            .map_err(|e| SeamError::failed(format!("{method} {url}: {e}")))
    }

    /// Read the headers, and the body under the cap when asked.
    async fn finish(
        &self,
        url: Url,
        response: reqwest::Response,
        want: BodyWant,
    ) -> Result<Fetched, SeamError> {
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
        .unwrap_or_else(|| {
            Mimetype::parse("application/octet-stream").expect("literal mimetype is valid")
        })
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
/// of the body never arrives; what one chunk carried past the wanted line
/// is cut before the cap is judged, so the head of a huge log is readable
/// even when the server hands it over in one piece.
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
        assert!(
            chunks <= cap.saturating_add(1),
            "each chunk carries at least one byte"
        );
        let before = bytes.len();
        bytes.extend_from_slice(&chunk);
        if let Some(wanted) = lines_wanted
            && let Some(end) = nth_line_end(&bytes[before..], wanted - newlines)
        {
            bytes.truncate(before + end);
            newlines = wanted;
        } else {
            newlines +=
                u64::try_from(chunk.iter().filter(|b| **b == b'\n').count()).unwrap_or(u64::MAX);
        }
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > cap {
            return Err(SeamError::Refused(format!(
                "{url} exceeds the web connection's {cap} byte cap"
            )));
        }
        if lines_wanted.is_some_and(|wanted| newlines >= wanted) {
            break;
        }
    }
    Ok(bytes)
}

/// The byte length through the `n`th newline of `chunk`, when it holds
/// that many; `n` is at least one.
fn nth_line_end(chunk: &[u8], n: u64) -> Option<usize> {
    assert!(n >= 1);
    chunk
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .nth(usize::try_from(n - 1).ok()?)
        .map(|(i, _)| i + 1)
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
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.9",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "100.64.0.1",
            "100.127.255.254",
            "192.0.0.8",
            "192.0.2.1",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
        ];
        for ip in private {
            let ip: IpAddr = ip.parse().expect("valid ip");
            assert!(!is_public_ip(ip), "{ip} is not public");
        }
        let public = [
            "93.184.216.34",
            "8.8.8.8",
            "100.128.0.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ];
        for ip in public {
            let ip: IpAddr = ip.parse().expect("valid ip");
            assert!(is_public_ip(ip), "{ip} is public");
        }
    }

    #[test]
    fn only_http_schemes_pass_the_url_guard() {
        assert_eq!(
            guard_url(&Url::parse("https://example.com/a.png").expect("url")).expect("host"),
            "example.com"
        );
        assert!(guard_url(&Url::parse("ftp://example.com/a").expect("url")).is_err());
        assert!(guard_url(&Url::parse("file:///etc/passwd").expect("url")).is_err());
        assert!(guard_url(&Url::parse("data:image/png;base64,AAAA").expect("url")).is_err());
    }

    #[test]
    fn described_requests_refuse_transport_headers_and_control_characters() {
        let url = Url::parse("https://example.com/api").expect("url");
        let fine = FetchRequest {
            method: FetchMethod::Get,
            url: url.clone(),
            headers: vec![("Accept".into(), "application/json".into())],
            body: None,
        };
        assert!(fine.validate().is_ok());
        for (name, value) in [
            ("Host", "evil.example"),
            ("content-length", "0"),
            ("X-Injected", "a\r\nX-Other: b"),
            ("bad name", "x"),
            ("", "x"),
        ] {
            let bad = FetchRequest {
                headers: vec![(name.into(), value.into())],
                ..fine.clone()
            };
            assert!(bad.validate().is_err(), "{name:?} must be refused");
        }
        let many = FetchRequest {
            headers: (0..=REQUEST_HEADERS_MAX)
                .map(|i| (format!("X-{i}"), "v".to_string()))
                .collect(),
            ..fine
        };
        assert!(many.validate().is_err());
    }

    #[test]
    fn methods_parse_case_insensitively_and_refuse_the_exotic() {
        assert_eq!(
            FetchMethod::try_from("get").expect("parses"),
            FetchMethod::Get
        );
        assert_eq!(
            FetchMethod::try_from("DELETE").expect("parses"),
            FetchMethod::Delete
        );
        assert!(FetchMethod::try_from("TRACE").is_err());
        assert!(FetchMethod::try_from("CONNECT").is_err());
    }

    #[test]
    fn an_empty_allow_list_means_public_for_the_web_and_nothing_for_a_plugin() {
        let web = Fetcher::new(Vec::new(), 1, Duration::from_secs(1), 1, "ua".into());
        assert!(web.allows_host("example.com"));
        let plugin =
            Fetcher::for_allowed_hosts_only(Vec::new(), 1, Duration::from_secs(1), 1, "ua".into());
        assert!(!plugin.allows_host("example.com"));
        let listed = Fetcher::for_allowed_hosts_only(
            vec![HostPattern::parse("*.example.com").expect("parses")],
            1,
            Duration::from_secs(1),
            1,
            "ua".into(),
        );
        assert!(listed.allows_host("api.example.com"));
        assert!(!listed.allows_host("example.org"));
    }

    #[test]
    fn line_ends_are_found_by_count_within_a_chunk() {
        assert_eq!(nth_line_end(b"a\nb\nc", 1), Some(2));
        assert_eq!(nth_line_end(b"a\nb\nc", 2), Some(4));
        assert_eq!(nth_line_end(b"a\nb\nc", 3), None);
        assert_eq!(nth_line_end(b"", 1), None);
    }

    #[test]
    fn the_redirect_ceiling_bounds_the_config() {
        let fetcher = Fetcher::new(Vec::new(), 1, Duration::from_secs(1), 500, "ua".into());
        assert_eq!(fetcher.redirects_max, REDIRECTS_MAX_CEILING);
    }
}
