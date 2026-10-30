//! Podcast metadata for the personal-settings page, mirroring nextcloud-gpodder's
//! `PodcastDataReader` / `PodcastData`: RSS channel info (or ARD Audiothek API
//! data) plus the cover image inlined as a data URI.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{Client, Url, header, redirect};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const USER_AGENT: &str = "nextcloud-gpodder (+https://nextcloud.com; like iTMS)";
const FALLBACK_USER_AGENT: &str = "nextcloud-gpodder (+https://nextcloud.com)";
const MAX_FEED_BYTES: usize = 50 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PodcastData {
    pub title: Option<String>,
    pub author: Option<String>,
    pub link: Option<String>,
    pub description: Option<String>,
    pub image_url: Option<String>,
    pub image_blob: Option<String>,
    pub fetched_at_unix: i64,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How long fetched podcast data is reused before it's fetched again.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Upper bound on the memory used by cached podcast data (mostly inlined images).
const CACHE_MAX_BYTES: usize = 128 * 1024 * 1024;

pub struct PodcastDataReader {
    client: Client,
    allow_local: bool,
    cache: Mutex<Cache>,
}

/// Podcast data by feed URL, expiring after [`CACHE_TTL`] and evicting the oldest
/// entries once the total size exceeds [`CACHE_MAX_BYTES`].
#[derive(Default)]
struct Cache {
    entries: HashMap<String, CacheEntry>,
    total_bytes: usize,
}

struct CacheEntry {
    data: PodcastData,
    inserted: Instant,
    size: usize,
}

impl Cache {
    fn get(&mut self, url: &str) -> Option<PodcastData> {
        let entry = self.entries.get(url)?;
        if entry.inserted.elapsed() < CACHE_TTL {
            return Some(entry.data.clone());
        }
        self.remove(url);
        None
    }

    fn insert(&mut self, url: String, data: PodcastData) {
        let size = url.len() + data.approximate_size();
        if size > CACHE_MAX_BYTES {
            return;
        }
        self.remove(&url);
        self.entries.retain(|_, e| e.inserted.elapsed() < CACHE_TTL);
        self.total_bytes = self.entries.values().map(|e| e.size).sum();
        while self.total_bytes + size > CACHE_MAX_BYTES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.inserted)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.remove(&oldest);
        }
        self.total_bytes += size;
        self.entries.insert(
            url,
            CacheEntry {
                data,
                inserted: Instant::now(),
                size,
            },
        );
    }

    fn remove(&mut self, url: &str) {
        if let Some(entry) = self.entries.remove(url) {
            self.total_bytes -= entry.size;
        }
    }
}

impl PodcastData {
    fn approximate_size(&self) -> usize {
        [
            &self.title,
            &self.author,
            &self.link,
            &self.description,
            &self.image_url,
            &self.image_blob,
        ]
        .iter()
        .map(|s| s.as_ref().map_or(0, String::len))
        .sum()
    }
}

impl PodcastDataReader {
    pub fn new(allow_local_remote_servers: bool) -> Self {
        let allow_local = allow_local_remote_servers;
        let mut builder =
            Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(redirect::Policy::custom(move |attempt| {
                    if attempt.previous().len() >= 10 {
                        attempt.error("too many redirects")
                    } else if !allow_local && !is_allowed_url(attempt.url()) {
                        attempt.error("redirect to a local address is not allowed")
                    } else {
                        attempt.follow()
                    }
                }));
        if !allow_local {
            // A proxy would resolve hostnames itself, bypassing the resolver's
            // address check, so ignore HTTP(S)_PROXY from the environment.
            builder = builder
                .dns_resolver(Arc::new(PublicOnlyResolver))
                .no_proxy();
        }
        PodcastDataReader {
            client: builder.build().expect("failed to build HTTP client"),
            allow_local,
            cache: Mutex::new(Cache::default()),
        }
    }

    pub fn cached(&self, url: &str) -> Option<PodcastData> {
        self.cache.lock().unwrap().get(url)
    }

    /// Returns cached data or fetches it. The caller must already have checked
    /// that the user knows this podcast (upstream returns `null` otherwise).
    pub async fn get_cached_or_fetch(&self, url: &str) -> Result<PodcastData, BoxError> {
        if let Some(data) = self.cached(url) {
            return Ok(data);
        }
        let mut data = self.fetch_podcast_data(url).await?;
        data.image_blob = self.try_fetch_image_blob(&data).await;
        self.cache
            .lock()
            .unwrap()
            .insert(url.to_owned(), data.clone());
        Ok(data)
    }

    async fn fetch_podcast_data(&self, url: &str) -> Result<PodcastData, BoxError> {
        if let Some(program_id) = extract_ard_program_id(url) {
            match self.fetch_ard_audiothek_data(&program_id, url).await {
                Ok(data) => return Ok(data),
                Err(err) => tracing::warn!(
                    "failed to resolve ARD Audiothek metadata for {url}, falling back to RSS parsing: {err}"
                ),
            }
        }
        let (_, body) = self.fetch_url(url, MAX_FEED_BYTES).await?;
        parse_rss_xml(&String::from_utf8_lossy(&body), crate::db::now())
    }

    async fn fetch_ard_audiothek_data(
        &self,
        program_id: &str,
        original_url: &str,
    ) -> Result<PodcastData, BoxError> {
        let api_url = format!("https://api.ardaudiothek.de/programsets/{program_id}");
        let (_, body) = self.fetch_url(&api_url, MAX_FEED_BYTES).await?;
        let decoded: Value = serde_json::from_slice(&body)?;
        let program_set = decoded
            .pointer("/data/programSet")
            .filter(|v| v.is_object())
            .ok_or("programSet missing in ARD Audiothek response")?;
        let image_url = match program_set.get("image") {
            Some(image @ Value::Object(_)) => {
                opt_str(image.get("url"))?.or(opt_str(image.get("url1X1"))?)
            }
            _ => None,
        };
        Ok(PodcastData {
            title: opt_str(program_set.get("title"))?,
            author: opt_str(program_set.pointer("/publicationService/title"))?,
            link: Some(
                opt_str(program_set.get("sharingUrl"))?.unwrap_or_else(|| original_url.to_owned()),
            ),
            description: opt_str(program_set.get("synopsis"))?
                .or(opt_str(program_set.get("description"))?),
            image_url,
            image_blob: None,
            fetched_at_unix: crate::db::now(),
        })
    }

    async fn try_fetch_image_blob(&self, data: &PodcastData) -> Option<String> {
        let image_url = data.image_url.as_deref().filter(|u| !u.is_empty())?;
        let (content_type, body) = self.fetch_url(image_url, MAX_IMAGE_BYTES).await.ok()?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(body);
        Some(format!("data:{content_type};base64,{encoded}"))
    }

    /// GETs `url`, retrying once with a different User-Agent on a non-2xx status.
    async fn fetch_url(&self, url: &str, max_bytes: usize) -> Result<(String, Vec<u8>), BoxError> {
        let parsed = Url::parse(url)?;
        if !self.allow_local && !is_allowed_url(&parsed) {
            return Err("fetching local addresses is not allowed".into());
        }
        let mut resp = self.get(parsed.clone(), USER_AGENT).await?;
        if !resp.status().is_success() {
            resp = self.get(parsed, FALLBACK_USER_AGENT).await?;
            if !resp.status().is_success() {
                return Err(format!(
                    "Web request returned non-2xx status code: {}",
                    resp.status().as_u16()
                )
                .into());
            }
        }
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            if body.len() + chunk.len() > max_bytes {
                return Err("response body too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok((content_type, body))
    }

    async fn get(&self, url: Url, user_agent: &str) -> reqwest::Result<reqwest::Response> {
        self.client
            .get(url)
            .header(header::USER_AGENT, user_agent)
            .send()
            .await
    }
}

/// Like PHP's `$array['key'] ?? null` assigned to a `?string`: a non-string value is an error.
fn opt_str(value: Option<&Value>) -> Result<Option<String>, BoxError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("expected a string, got {other}").into()),
    }
}

/// Matches `#https?://api\.ardaudiothek\.de/programsets/(?P<id>[^/?]+)#i`, or
/// an ardaudiothek.de `/sendung/...` or `/podcast/...` page ending in a numeric id.
pub fn extract_ard_program_id(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    const NEEDLE: &str = "://api.ardaudiothek.de/programsets/";
    let mut search_from = 0;
    while let Some(pos) = lower[search_from..].find(NEEDLE).map(|p| p + search_from) {
        if lower[..pos].ends_with("http") || lower[..pos].ends_with("https") {
            let rest = &url[pos + NEEDLE.len()..];
            let id: String = rest
                .chars()
                .take_while(|c| *c != '/' && *c != '?')
                .collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
        search_from = pos + 1;
    }

    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host != "ardaudiothek.de" && host != "www.ardaudiothek.de" {
        return None;
    }
    let segments: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 3 {
        return None;
    }
    let section = segments[0].to_ascii_lowercase();
    if section != "sendung" && section != "podcast" {
        return None;
    }
    let possible_id = segments[segments.len() - 1];
    (!possible_id.is_empty() && possible_id.bytes().all(|b| b.is_ascii_digit()))
        .then(|| possible_id.to_owned())
}

struct Frame {
    name: String,
    text: String,
    has_attributes: bool,
    has_children: bool,
    href: Option<String>,
}

/// Extracts channel info the same way upstream's `PodcastData::parseRssXml` does
/// with SimpleXML: `title`/`link`/`description` from the root's first `channel`
/// (null when the element is missing or empty), `itunes:author` and `image/url`
/// via `/rss/channel/...` paths, falling back to `itunes:image/@href`.
pub fn parse_rss_xml(xml: &str, fetched_at_unix: i64) -> Result<PodcastData, BoxError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;

    let mut stack: Vec<Frame> = Vec::new();
    let mut root_name: Option<String> = None;
    let mut channels_seen = 0;
    let (mut title, mut link, mut description) = (None, None, None);
    let (mut seen_title, mut seen_link, mut seen_description) = (false, false, false);
    let (mut author, mut image_url, mut itunes_href) = (None, None, None);

    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                if let Some(parent) = stack.last_mut() {
                    parent.has_children = true;
                } else if root_name.is_some() {
                    return Err("multiple root elements".into());
                }
                let name = AsRef::<str>::as_ref(&e.name()).to_owned();
                let mut has_attributes = false;
                let mut href = None;
                for attr in e.attributes() {
                    let attr = attr?;
                    has_attributes = true;
                    if AsRef::<str>::as_ref(&attr.key) == "href" {
                        href = Some(attr.normalized_value(XmlVersion::Implicit1_0)?.into_owned());
                    }
                }
                if stack.is_empty() {
                    root_name = Some(name.clone());
                } else if stack.len() == 1 && name == "channel" {
                    channels_seen += 1;
                }
                stack.push(Frame {
                    name,
                    text: String::new(),
                    has_attributes,
                    has_children: false,
                    href,
                });
            }
            Event::Text(t) => {
                if let Some(frame) = stack.last_mut() {
                    frame.text.push_str(&t.xml10_content());
                }
            }
            Event::CData(t) => {
                if let Some(frame) = stack.last_mut() {
                    frame.text.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(r) => {
                let c = match r.resolve_char_ref()? {
                    Some(c) => c,
                    None => match &*r {
                        "lt" => '<',
                        "gt" => '>',
                        "amp" => '&',
                        "apos" => '\'',
                        "quot" => '"',
                        other => return Err(format!("undefined entity &{other};").into()),
                    },
                };
                if let Some(frame) = stack.last_mut() {
                    frame.text.push(c);
                }
            }
            Event::End(_) => {
                let frame = stack.pop().ok_or("unbalanced end tag")?;
                let path: Vec<&str> = stack.iter().map(|f| f.name.as_str()).collect();
                let is_rss = root_name.as_deref() == Some("rss");

                if path.len() == 2 && path[1] == "channel" {
                    let truthy =
                        !frame.text.is_empty() || frame.has_attributes || frame.has_children;
                    let value = truthy.then(|| frame.text.clone());
                    if channels_seen == 1 {
                        match frame.name.as_str() {
                            "title" if !seen_title => (title, seen_title) = (value, true),
                            "link" if !seen_link => (link, seen_link) = (value, true),
                            "description" if !seen_description => {
                                (description, seen_description) = (value, true)
                            }
                            _ => {}
                        }
                    }
                    if is_rss && frame.name == "itunes:author" && author.is_none() {
                        author = Some(frame.text.clone());
                    }
                    if is_rss && frame.name == "itunes:image" && itunes_href.is_none() {
                        itunes_href = frame.href.clone();
                    }
                }
                if is_rss
                    && path == ["rss", "channel", "image"]
                    && frame.name == "url"
                    && image_url.is_none()
                {
                    image_url = Some(frame.text);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if root_name.is_none() {
        return Err("document has no root element".into());
    }
    if !stack.is_empty() {
        return Err("unexpected end of document".into());
    }
    Ok(PodcastData {
        title,
        author,
        link,
        description,
        image_url: image_url.or(itunes_href),
        image_blob: None,
        fetched_at_unix,
    })
}

/// Rejects URLs whose host is a literal non-public IP address. Hostnames are
/// checked after resolution by [`PublicOnlyResolver`].
fn is_allowed_url(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => is_public_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => is_public_ip(IpAddr::V6(ip)),
        Some(url::Host::Domain(d)) => !d.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || a >= 240)
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    let embedded_v4 = |hi: u16, lo: u16| Ipv4Addr::from(((hi as u32) << 16) | lo as u32);
    // IPv4-mapped (::ffff:a.b.c.d) and IPv4-compatible (::a.b.c.d) addresses.
    if let Some(v4) = ip.to_ipv4() {
        return is_public_ipv4(v4);
    }
    // 6to4 (2002::/16) embeds an IPv4 address in bits 16..48.
    if seg[0] == 0x2002 {
        return is_public_ipv4(embedded_v4(seg[1], seg[2]));
    }
    // NAT64 (64:ff9b::/96) embeds an IPv4 address in the last 32 bits.
    if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return is_public_ipv4(embedded_v4(seg[6], seg[7]));
    }
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (seg[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
        || (seg[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        || (seg[0] & 0xffc0) == 0xfec0 // deprecated site-local fec0::/10
        || (seg[0] == 0x64 && seg[1] == 0xff9b && seg[2] == 1) // local-use NAT64 64:ff9b:1::/48
        || (seg[0] == 0x2001 && seg[1] == 0) // Teredo 2001::/32
        || (seg[0] == 0x2001 && seg[1] == 0xdb8) // documentation 2001:db8::/32
        || seg[0] == 0x100 && seg[1..4] == [0, 0, 0]) // discard-only 100::/64
}

/// DNS resolver that drops loopback/private addresses, like Nextcloud's HTTP
/// client does unless `allow_local_remote_servers` is set.
struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| is_public_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} does not resolve to a public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rss_channel() {
        let xml = r#"<?xml version="1.0"?>
<rss xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd" version="2.0">
  <channel>
    <title>My &amp; Pod</title>
    <link>https://example.com</link>
    <description><![CDATA[<b>Desc</b>]]></description>
    <itunes:author>Jane</itunes:author>
    <itunes:image href="https://example.com/i.jpg"/>
    <item><title>Episode</title></item>
  </channel>
</rss>"#;
        let data = parse_rss_xml(xml, 7).unwrap();
        assert_eq!(data.title.as_deref(), Some("My & Pod"));
        assert_eq!(data.link.as_deref(), Some("https://example.com"));
        assert_eq!(data.description.as_deref(), Some("<b>Desc</b>"));
        assert_eq!(data.author.as_deref(), Some("Jane"));
        assert_eq!(data.image_url.as_deref(), Some("https://example.com/i.jpg"));
        assert_eq!(data.fetched_at_unix, 7);
    }

    #[test]
    fn image_url_element_wins_and_empty_title_is_null() {
        let xml = "<rss><channel><title></title><image><url>https://e/x.png</url></image>\
                   <itunes:image href=\"https://e/y.png\"/></channel></rss>";
        let data = parse_rss_xml(xml, 0).unwrap();
        assert_eq!(data.title, None);
        assert_eq!(data.image_url.as_deref(), Some("https://e/x.png"));
    }

    #[test]
    fn rejects_malformed_xml() {
        assert!(parse_rss_xml("<rss><channel></rss>", 0).is_err());
        assert!(parse_rss_xml("not xml", 0).is_err());
    }

    #[test]
    fn extracts_ard_ids() {
        assert_eq!(
            extract_ard_program_id("https://api.ardaudiothek.de/programsets/123?x"),
            Some("123".into())
        );
        assert_eq!(
            extract_ard_program_id("https://www.ardaudiothek.de/sendung/foo/12345/"),
            Some("12345".into())
        );
        assert_eq!(
            extract_ard_program_id("https://www.ardaudiothek.de/sendung/foo"),
            None
        );
        assert_eq!(extract_ard_program_id("https://example.com/feed"), None);
    }

    #[test]
    fn cache_expires_and_is_bounded() {
        let data = |blob_len: usize| PodcastData {
            title: None,
            author: None,
            link: None,
            description: None,
            image_url: None,
            image_blob: Some("x".repeat(blob_len)),
            fetched_at_unix: 0,
        };
        let mut cache = Cache::default();
        let big = CACHE_MAX_BYTES / 3;
        for i in 0..5 {
            cache.insert(format!("u{i}"), data(big));
        }
        assert!(cache.total_bytes <= CACHE_MAX_BYTES);
        assert!(cache.get("u0").is_none(), "oldest entries are evicted");
        assert!(cache.get("u4").is_some());
        cache.insert("too-big".into(), data(CACHE_MAX_BYTES + 1));
        assert!(cache.get("too-big").is_none());

        cache.entries.get_mut("u4").unwrap().inserted -= CACHE_TTL;
        assert!(cache.get("u4").is_none(), "expired entries are dropped");
    }

    #[test]
    fn classifies_ips() {
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("10.1.2.3".parse().unwrap()));
        assert!(!is_public_ip("::1".parse().unwrap()));
        assert!(!is_public_ip("::ffff:192.168.0.1".parse().unwrap()));
        assert!(!is_public_ip("::127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("2002:c0a8:0001::1".parse().unwrap()));
        assert!(!is_public_ip("64:ff9b::10.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("fec0::1".parse().unwrap()));
        assert!(!is_public_ip("2001:db8::1".parse().unwrap()));
        assert!(is_public_ip("1.1.1.1".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
        assert!(is_public_ip("2002:0101:0101::1".parse().unwrap()));
        assert!(is_public_ip("64:ff9b::1.1.1.1".parse().unwrap()));
    }
}
