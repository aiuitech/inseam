//! The web connection plugin: the public web as a **fetch-only host**
//! (`design/connections.md`). It enumerates nothing and is never swept; it
//! exists so a link found in an indexed document — a markdown image, a URL
//! in a note — can become a content reference (`design/indexing.md`) that
//! the planner reads for byte-wanting transforms and that clients fetch
//! like any other address. Locators are the URLs themselves:
//! `inseam://web-…/https://example.com/logo.png`.
//!
//! Mounting this entry is the owner's consent for the node to contact
//! linked sites, so it is not in the base composition, and every request
//! goes through the guard in [`fetch`]: an optional allow list, a public-
//! address check on every hop, pinned resolution, a byte cap, a timeout.

pub(crate) mod fetch;

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use url::Url;

use inseam_kernel::address::{Address, ContentLength, Envelope, HostId, Locator, Timestamp};
use inseam_kernel::substrate::{parse_config, ApplyCx, Inject, Manifest, Plugin, PluginError};
use inseam_seams::connection::{
    derive_host_id, register_as_effect, Capabilities, Connection, EnumeratedSource,
    HostDescription, HostKind, Registration,
};
use inseam_seams::text::{check_line_range, slice_lines};
use inseam_seams::SeamError;

use fetch::{Fetcher, HostPattern};

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConnectionConfig {
    /// Hosts the node may contact: exact hosts (`example.com`,
    /// `127.0.0.1`) or domains with everything under them
    /// (`*.wikimedia.org`). Empty allows any host whose address is
    /// public. A host named here may also be private — a LAN image
    /// server — since the owner named it.
    pub allow_hosts: Vec<String>,
    /// Most bytes one resource may be: larger answers are refused before
    /// the body is read when the server declares a length, and the moment
    /// it passes the cap otherwise.
    pub content_bytes_max: u64,
    /// Whole-request timeout, headers and body.
    pub timeout_ms: u64,
    /// Redirect hops followed per fetch, each re-guarded; at most ten.
    pub redirects_max: u32,
    /// What the node calls itself to the sites it contacts.
    pub user_agent: String,
}

impl Default for WebConnectionConfig {
    fn default() -> Self {
        Self {
            allow_hosts: Vec::new(),
            content_bytes_max: 16 * 1024 * 1024,
            timeout_ms: 10_000,
            redirects_max: 3,
            user_agent: format!("inseam/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// The identity material of the one web host: there is exactly one public
/// web, so every node derives the same id for it and a reference minted
/// on one node names the same thing on another.
pub const WEB_HOST_PRINCIPAL: &str = "public";
/// The `source_type` every web resource's envelope carries.
pub const WEB_SOURCE_TYPE: &str = "web-resource";

pub fn web_host_kind() -> HostKind {
    HostKind::new("web").expect("literal host kind is valid")
}

pub fn web_host_id() -> HostId {
    derive_host_id(&web_host_kind(), WEB_HOST_PRINCIPAL)
}

/// The address of a URL on the web host: the URL is the locator, as
/// written, so the address round-trips to the exact resource.
pub fn web_address(url: &Url) -> Result<Address, SeamError> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(SeamError::Refused(format!("{url}: only http and https are addressable")));
    }
    let locator = Locator::new(url.as_str()).map_err(|e| SeamError::failed(e.to_string()))?;
    Ok(Address::new(web_host_id(), locator))
}

/// The URL a web address names. Refuses other hosts' addresses.
pub fn url_of(address: &Address) -> Result<Url, SeamError> {
    if address.host != web_host_id() {
        return Err(SeamError::failed(format!(
            "address {address} names host `{}`, not the web host",
            address.host
        )));
    }
    let url = Url::parse(address.locator.as_str())
        .map_err(|e| SeamError::failed(format!("{address}: locator is not a URL: {e}")))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(SeamError::Refused(format!("{url}: only http and https are fetched")));
    }
    Ok(url)
}

pub struct WebConnection {
    config: WebConnectionConfig,
}

pub struct WebConnectionFactory;

impl inseam_kernel::substrate::PluginFactory for WebConnectionFactory {
    fn name(&self) -> &str {
        "connection-web"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(WebConnection {
            config: parse_config(config)?,
        }))
    }
}

#[async_trait::async_trait]
impl Plugin for WebConnection {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("connections")];
        Manifest {
            name: "connection-web",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let host = WebHost::new(&self.config).map_err(PluginError)?;
        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                host: HostDescription {
                    id: web_host_id(),
                    kind: web_host_kind(),
                    display_name: "The public web".to_string(),
                },
                // Fetch-only, stated in full: never swept, no change feed,
                // nothing written.
                capabilities: Capabilities {
                    enumerates: false,
                    change_feed: false,
                    writable: false,
                },
                connection: Arc::new(host),
            },
        )
    }
}

/// The connection to the web host.
pub struct WebHost {
    fetcher: Fetcher,
}

impl WebHost {
    pub fn new(config: &WebConnectionConfig) -> Result<Self, String> {
        let allow_hosts = config
            .allow_hosts
            .iter()
            .map(|p| HostPattern::parse(p))
            .collect::<Result<Vec<_>, _>>()?;
        if config.content_bytes_max == 0 {
            return Err("content_bytes_max must be at least 1".to_string());
        }
        if config.timeout_ms == 0 {
            return Err("timeout_ms must be at least 1".to_string());
        }
        Ok(Self {
            fetcher: Fetcher::new(
                allow_hosts,
                config.content_bytes_max,
                Duration::from_millis(config.timeout_ms),
                config.redirects_max,
                config.user_agent.clone(),
            ),
        })
    }
}

#[async_trait::async_trait]
impl Connection for WebHost {
    async fn enumerate(&self, _root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        Err(SeamError::Unavailable(
            "the web host serves fetches only and cannot be swept".to_string(),
        ))
    }

    fn locator_prefix(&self, _root: &str) -> Option<String> {
        None
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let bytes = self.read_bytes(address).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Lines of a web resource, streamed until line `end` is in hand and
    /// then dropped mid-body: the bytes before a line are the only way to
    /// find it, so a range request could not skip them, but nothing after
    /// it need cross the wire.
    async fn read_lines(&self, address: &Address, start: u64, end: u64) -> Result<String, SeamError> {
        check_line_range(start, end)?;
        let url = url_of(address)?;
        let fetched = self.fetcher.get_lines(&url, end).await?;
        let text = String::from_utf8_lossy(&fetched.bytes);
        slice_lines(&text, start, end)
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        let url = url_of(address)?;
        let fetched = self.fetcher.get(&url).await?;
        let size = u64::try_from(fetched.bytes.len()).unwrap_or(u64::MAX);
        assert!(size <= self.fetcher.content_bytes_max(), "the fetcher enforces its cap");
        Ok(fetched.bytes)
    }

    /// What the resource's headers say it is: the content type and the
    /// declared length, with the URL's last path segment as the hint.
    async fn describe(&self, address: &Address) -> Result<Envelope, SeamError> {
        let url = url_of(address)?;
        let fetched = self.fetcher.head(&url).await?;
        let length = fetched.content_length.unwrap_or(0);
        let hint = url
            .path_segments()
            .and_then(|mut segments| segments.next_back().map(str::to_string))
            .filter(|s| !s.is_empty());
        Ok(Envelope {
            source_type: WEB_SOURCE_TYPE.to_string(),
            content_type: fetched.content_type,
            length: ContentLength::Bytes(length),
            created: None,
            modified: None,
            observed: Timestamp::from(SystemTime::now()),
            properties: Vec::new(),
            hint,
            content_digest: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_addresses_carry_the_url_as_locator_and_roundtrip() {
        let url = Url::parse("https://example.com/a/logo.png?v=2").expect("url");
        let address = web_address(&url).expect("addressable");
        assert_eq!(address.host, web_host_id());
        assert!(address.host.as_str().starts_with("web-"));
        assert_eq!(address.locator.as_str(), "https://example.com/a/logo.png?v=2");
        let parsed: Address = address.to_string().parse().expect("address parses");
        assert_eq!(parsed, address);
        assert_eq!(url_of(&parsed).expect("url"), url);
    }

    #[test]
    fn only_http_urls_are_addressable() {
        assert!(web_address(&Url::parse("ftp://example.com/a").expect("url")).is_err());
        let foreign = Address::new(
            HostId::new("fs-test").expect("valid"),
            Locator::new("https://example.com/a").expect("valid"),
        );
        assert!(url_of(&foreign).is_err());
        let not_a_url = Address::new(web_host_id(), Locator::new("nope").expect("valid"));
        assert!(url_of(&not_a_url).is_err());
    }

    #[test]
    fn the_config_refuses_zero_caps_and_bad_patterns() {
        let mut config = WebConnectionConfig::default();
        assert!(WebHost::new(&config).is_ok());
        config.content_bytes_max = 0;
        assert!(WebHost::new(&config).is_err());
        config = WebConnectionConfig {
            allow_hosts: vec!["*".to_string()],
            ..WebConnectionConfig::default()
        };
        assert!(WebHost::new(&config).is_err());
    }

    #[test]
    fn every_node_derives_the_same_web_host() {
        assert_eq!(web_host_id(), derive_host_id(&web_host_kind(), "public"));
        assert_ne!(web_host_id(), derive_host_id(&web_host_kind(), "other"));
    }
}
