//! Bun/BoringSSL-shaped HTTP/1.1 transport, used only by the OpenCode free
//! lane.
//!
//! The anonymous Zen free tier is fingerprint-sensitive: from 2026-10-26 it
//! refuses every client that does not look like the official OpenCode CLI,
//! which runs on Bun. Two layers are checked, and both live here:
//!
//! * **The TLS ClientHello** must be Bun's own: BoringSSL with Bun's cipher
//!   list, curve list and signature algorithms, no GREASE and `http/1.1` as
//!   the only ALPN protocol. Measured with `tls.peet.ws` this yields
//!   `JA4 t13d1713h1_5b57614c22b0_7f527383e249` and `JA3 hash
//!   1523504b38f0fae0d881d4b6554aac1b` — the exact values a Bun 1.4.4 `fetch`
//!   produces. `rustls`/`reqwest` send a different hello and are always
//!   refused, which is why this transport exists instead of a reqwest client.
//! * **The HTTP/1.1 request layout** must follow Bun's `fetch` writer: the
//!   WebKit common headers in their fixed spelling/order, the application
//!   headers lowercased and sorted, then the framing headers (`Connection`,
//!   `User-Agent`, `Accept`, `Host`, `Accept-Encoding`, `Content-Length`) in
//!   the order Bun appends them. See [`render_request_head`].
//!
//! The transport is intentionally small: plain HTTP/1.1, one connection per
//! request. Its dialling is routed through [`Egress`]: a direct dial, an HTTP
//! CONNECT tunnel (`http://…`, Basic proxy auth from the URL userinfo) or a
//! SOCKS5 tunnel (`socks5://` = proxy-side name resolution, `socks5h://` =
//! local resolution, RFC 1929 user/password auth). The upstream sees the same
//! Bun ClientHello through a tunnel, so a proxy changes the egress IP (and
//! therefore the free-tier quota bucket) without changing the fingerprint. The
//! free lane is a chat lane (streaming SSE), so
//! connection pooling would buy little and complicate cancellation. Reads
//! happen on a blocking worker ([`tokio::task::spawn_blocking`]) and are handed
//! to the async side through a bounded channel; a 200 ms read timeout keeps the
//! worker responsive to the request deadline and to a dropped receiver, so a
//! client disconnect or a cancelled request tears the socket down instead of
//! leaking a thread.
//!
//! This module deliberately does not use `reqwest`: its HTTP/1.1 writer
//! canonicalizes and orders headers itself, which is part of the fingerprint.

use base64::Engine as _;
use boring::ssl::{SslConnector, SslMethod, StatusType};
use bytes::Bytes;
use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::io::{self, BufReader, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// JA4 of the ClientHello this module produces, as measured against
/// `tls.peet.ws` (and equal to a Bun 1.4.4 `fetch` hello).
pub const BUN_JA4: &str = "t13d1713h1_5b57614c22b0_7f527383e249";
/// JA3 hash of the same hello.
pub const BUN_JA3_HASH: &str = "1523504b38f0fae0d881d4b6554aac1b";

/// The exact signature-algorithm list Bun's BoringSSL sends, in wire order.
/// BoringSSL's own default list differs (it omits `ed25519` and
/// `ecdsa_secp521r1_sha512`), which changes the JA4 `_c` segment.
const BUN_SIGNATURE_ALGORITHMS: &str = "ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:rsa_pkcs1_sha256:ecdsa_secp384r1_sha384:rsa_pss_rsae_sha384:rsa_pkcs1_sha384:rsa_pss_rsae_sha512:rsa_pkcs1_sha512:ed25519:ecdsa_secp521r1_sha512:rsa_pkcs1_sha1";
/// Bun's supported-group list, in wire order (X25519MLKEM768 is the first key
/// share). BoringSSL's default list carries the older P-256/Kyber hybrid too,
/// which Bun does not offer.
const BUN_CURVE_LIST: &str = "X25519MLKEM768:X25519:P-256:P-384";
/// `fetch`'s User-Agent when the caller sets none (the gateway always sets the
/// OpenCode CLI UA, so this is only a fallback).
const FALLBACK_USER_AGENT: &str = "Bun/1.4.4";

/// Headers WebKit/Bun know by name, in the order Bun's `fetch` writes them.
const BUN_COMMON_HEADERS: [&str; 27] = [
    "Accept",
    "Accept-Charset",
    "Accept-Encoding",
    "Accept-Language",
    "Authorization",
    "Cache-Control",
    "Connection",
    "Content-Encoding",
    "Content-Language",
    "Content-Length",
    "Content-Type",
    "Cookie",
    "DNT",
    "Expect",
    "Host",
    "If-None-Match",
    "Keep-Alive",
    "Last-Event-Id",
    "Origin",
    "Pragma",
    "Proxy-Authorization",
    "Range",
    "Referer",
    "Sec-Fetch-Mode",
    "TE",
    "Upgrade-Insecure-Requests",
    "User-Agent",
];

/// Cap on a response head (status line + headers). A server that sends more is
/// not a server this lane should talk to.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Cap on a proxy's CONNECT / SOCKS reply head.
const MAX_PROXY_HEAD_BYTES: usize = 16 * 1024;
/// Cap on a compressed body that must be buffered to be decoded.
const MAX_BUFFERED_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Poll interval of the blocking reader; the worker re-checks the deadline and
/// the cancellation flag at this cadence.
const READ_POLL: Duration = Duration::from_millis(200);
/// Bound on the TCP connect + TLS handshake so a cancelled request cannot sit
/// in the blocking pool for the whole request timeout.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Body chunks buffered between the blocking reader and the async consumer.
const BODY_CHANNEL_CAPACITY: usize = 32;
/// Size of the read buffer used while copying an uncompressed body.
const BODY_CHUNK: usize = 16 * 1024;

/// One response whose body is streamed off the socket: status, headers and a
/// stream of body chunks (already de-chunked and decompressed when needed).
pub struct RawResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: BoxStream<'static, io::Result<Bytes>>,
}

/// How a free-lane request leaves this host.
///
/// The free tier's budget is counted per **egress IP**: measured on
/// 2026-10-29, a direct dial and a Cloudflare WARP exit each had their own
/// anonymous quota, and a second WARP `/32` was still exhausted from someone
/// else's traffic. A proxy is therefore not just a network hop but an
/// accounting dimension — [`crate::opencode_free_budget`] keys its buckets by
/// the egress label this type produces. The transport dials directly or
/// tunnels through an HTTP CONNECT / SOCKS5 proxy; the TLS handshake to the
/// upstream still uses Bun's ClientHello, so the fingerprint gate keeps
/// working through a tunnel.
///
/// `Debug` is implemented by hand: the raw URL may carry `user:pass` and must
/// never reach a log line.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum Egress {
    /// Dial the upstream host directly from this host's own address.
    #[default]
    Direct,
    /// Tunnel through an HTTP CONNECT proxy (`http://user:pass@host:port`).
    HttpConnect { url: String },
    /// Tunnel through a SOCKS5 proxy that resolves the target hostname itself
    /// (`socks5://user:pass@host:port`), per the configured scheme contract.
    Socks5 { url: String },
    /// Tunnel through a SOCKS5 proxy with local name resolution
    /// (`socks5h://...`), per the configured scheme contract.
    Socks5h { url: String },
}

impl std::fmt::Debug for Egress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct => formatter.write_str("Egress::Direct"),
            Self::HttpConnect { url } => {
                write!(formatter, "Egress::HttpConnect({})", mask_proxy_url(url))
            }
            Self::Socks5 { url } => write!(formatter, "Egress::Socks5({})", mask_proxy_url(url)),
            Self::Socks5h { url } => write!(formatter, "Egress::Socks5h({})", mask_proxy_url(url)),
        }
    }
}

impl Egress {
    /// Resolve a provider connection's optional `proxy_url` into an egress.
    ///
    /// The URL scheme selects the tunnel: `http`/`https` → HTTP CONNECT,
    /// `socks5` → proxy-side resolution, `socks5h` → local resolution. An
    /// unsupported scheme is kept in the HTTP variant so the dial reports it
    /// loudly instead of silently using the direct egress.
    pub fn from_proxy_url(proxy_url: Option<&str>) -> Self {
        let Some(url) = proxy_url.map(str::trim).filter(|url| !url.is_empty()) else {
            return Self::Direct;
        };
        let scheme = reqwest::Url::parse(url)
            .ok()
            .map(|parsed| parsed.scheme().to_ascii_lowercase());
        match scheme.as_deref() {
            Some("socks5") => Self::Socks5 { url: url.to_string() },
            Some("socks5h") => Self::Socks5h { url: url.to_string() },
            _ => Self::HttpConnect { url: url.to_string() },
        }
    }

    pub fn is_direct(&self) -> bool {
        matches!(self, Self::Direct)
    }

    /// Transport mode, stable for logs and the status endpoint.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::HttpConnect { .. } => "http_connect",
            Self::Socks5 { .. } => "socks5",
            Self::Socks5h { .. } => "socks5h",
        }
    }

    /// Accounting identity of this egress: `scheme://host:port` with the
    /// userinfo stripped, or `None` for a direct dial (which keeps the
    /// pre-proxy bucket keys unchanged). This string is the second half of a
    /// budget bucket key, so it must be stable and secret-free.
    pub fn label(&self) -> Option<String> {
        match self {
            Self::Direct => None,
            Self::HttpConnect { url } | Self::Socks5 { url } | Self::Socks5h { url } => {
                Some(ProxyUrl::parse(url).map_or_else(
                    |_| "invalid://proxy".to_string(),
                    |proxy| format!("{}://{}", proxy.scheme.as_str(), proxy.authority()),
                ))
            }
        }
    }

    /// Operator-facing label: mode, masked host and port, never the userinfo.
    pub fn masked_label(&self) -> String {
        match self {
            Self::Direct => "direct".to_string(),
            Self::HttpConnect { url } | Self::Socks5 { url } | Self::Socks5h { url } => {
                mask_proxy_url(url)
            }
        }
    }
}

/// Redact the credentials of a proxy URL and mask most of its host, keeping
/// enough of it to tell two exits apart in a log or the status endpoint.
pub fn mask_proxy_url(url: &str) -> String {
    let Ok(proxy) = ProxyUrl::parse(url) else {
        // Never echo the raw string: an unparsable proxy URL may still contain
        // the password the operator typed.
        return "<unparsable-proxy>".to_string();
    };
    format!("{}://{}", proxy.scheme.as_str(), mask_authority(&proxy.authority()))
}

/// Mask an `egress_label` produced by [`Egress::label`] for display.
pub fn mask_egress_label(label: &str) -> String {
    match label.split_once("://") {
        Some((scheme, authority)) => format!("{scheme}://{}", mask_authority(authority)),
        None => mask_authority(label),
    }
}

/// Mask the host part of a `host:port` authority. IPv4 keeps its first two
/// octets, an IPv6 literal keeps its prefix, a hostname keeps its first
/// character and its domain suffix.
fn mask_authority(authority: &str) -> String {
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') || host.starts_with('[') => (host, Some(port)),
        _ => (authority, None),
    };
    let masked = if let Ok(address) = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        match address {
            IpAddr::V4(v4) => {
                let octets = v4.octets();
                format!("{}.{}.*.*", octets[0], octets[1])
            }
            IpAddr::V6(v6) => {
                let segments = v6.segments();
                format!("{:x}:{:x}:{:x}:…", segments[0], segments[1], segments[2])
            }
        }
    } else if let Some((first, suffix)) = host.split_once('.') {
        let initial: String = first.chars().take(1).collect();
        format!("{initial}***.{suffix}")
    } else {
        let prefix: String = host.chars().take(3).collect();
        format!("{prefix}***")
    };
    match port {
        Some(port) => format!("{masked}:{port}"),
        None => masked,
    }
}

/// Validate a proxy URL before it is accepted into the anonymous pool.
pub fn validate_proxy_url(url: &str) -> Result<(), String> {
    ProxyUrl::parse(url).map(|_| ())
}

/// One parsed proxy endpoint. Kept private: the public vocabulary is [`Egress`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProxyUrl {
    scheme: ProxyScheme,
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyScheme {
    Http,
    Https,
    Socks5,
    Socks5h,
}

impl ProxyScheme {
    fn parse(scheme: &str) -> Option<Self> {
        match scheme.trim().to_ascii_lowercase().as_str() {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            "socks5" => Some(Self::Socks5),
            "socks5h" => Some(Self::Socks5h),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks5 => "socks5",
            Self::Socks5h => "socks5h",
        }
    }

    fn default_port(self) -> Option<u16> {
        match self {
            Self::Http => Some(80),
            Self::Https => Some(443),
            Self::Socks5 | Self::Socks5h => None,
        }
    }
}

impl ProxyUrl {
    fn parse(raw: &str) -> Result<Self, String> {
        let parsed = reqwest::Url::parse(raw.trim())
            .map_err(|error| format!("invalid proxy URL: {error}"))?;
        let scheme = ProxyScheme::parse(parsed.scheme())
            .ok_or_else(|| format!("unsupported proxy scheme '{}' (use http, socks5 or socks5h)", parsed.scheme()))?;
        // `url::Url::host_str` returns an IPv6 literal bracketed; the proxy
        // code re-brackets only where the wire format needs it.
        let host = parsed
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| "proxy URL has no host".to_string())?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = parsed
            .port()
            .or_else(|| parsed.port_or_known_default())
            .or_else(|| scheme.default_port())
            .ok_or_else(|| "proxy URL has no port (socks5 needs an explicit one)".to_string())?;
        let username = percent_decode(parsed.username())
            .map_err(|error| format!("invalid proxy username: {error}"))?
            .filter(|username| !username.is_empty());
        let password = match parsed.password() {
            Some(password) => percent_decode(password)
                .map_err(|error| format!("invalid proxy password: {error}"))?,
            None => None,
        };
        Ok(Self { scheme, host, port, username, password })
    }

    /// `host:port` with an IPv6 literal bracketed, as CONNECT/SOCKS expect.
    fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// `Proxy-Authorization: Basic …` value, when the URL carried a user.
    fn basic_authorization(&self) -> Option<String> {
        let username = self.username.as_ref()?;
        let password = self.password.as_deref().unwrap_or("");
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{username}:{password}"));
        Some(format!("Basic {encoded}"))
    }
}

/// Decode percent-escapes in URL userinfo (the `url` crate keeps the raw
/// percent-encoded form, but SOCKS5/HTTP auth need the actual bytes).
fn percent_decode(value: &str) -> Result<Option<String>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes.get(index + 1).and_then(|byte| hex_value(*byte));
            let low = bytes.get(index + 2).and_then(|byte| hex_value(*byte));
            let (Some(high), Some(low)) = (high, low) else {
                return Err("malformed percent-escape".to_string());
            };
            out.push(high << 4 | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out)
        .map(Some)
        .map_err(|_| "userinfo is not UTF-8".to_string())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Error code of a free-lane dispatch that failed in the egress (proxy
/// unreachable, CONNECT refused, SOCKS negotiation failed) rather than at the
/// upstream. The free-lane loop rotates to another exit instead of blaming the
/// credential, and does not cool the provider.
pub const EGRESS_ERROR_CODE: &str = "free_lane_egress";

/// A failure of the egress itself. Lives inside an [`io::Error`] so the
/// transport's signature stays `io::Result`; [`is_egress_failure`] classifies
/// it for the callers.
#[derive(Debug)]
struct EgressFailure {
    stage: &'static str,
    detail: String,
}

impl std::fmt::Display for EgressFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "egress {} failed: {}", self.stage, self.detail)
    }
}

impl std::error::Error for EgressFailure {}

fn egress_failure(stage: &'static str, detail: impl Into<String>) -> io::Error {
    io::Error::other(EgressFailure { stage, detail: detail.into() })
}

/// Did this transport error come from the proxy/tunnel rather than the
/// upstream? Those are retryable on another egress.
pub fn is_egress_failure(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<EgressFailure>())
}

/// POST `body` to `url` the way Bun's `fetch` does, over the direct egress.
pub async fn post(
    url: &str,
    headers: &[(String, String)],
    body: Bytes,
    timeout: Duration,
) -> io::Result<RawResponse> {
    post_via(url, headers, body, timeout, &Egress::Direct).await
}

/// POST `body` to `url` through `egress`.
pub async fn post_via(
    url: &str,
    headers: &[(String, String)],
    body: Bytes,
    timeout: Duration,
    egress: &Egress,
) -> io::Result<RawResponse> {
    let target = Target::parse(url)?;
    if headers.iter().any(|(name, value)| !valid_header_field(name, value)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bun transport: invalid HTTP header field",
        ));
    }
    let head = render_request_head(&target, "POST", headers, Some(body.len()))?;

    let (head_tx, head_rx) = oneshot::channel::<io::Result<RawHead>>();
    let (body_tx, body_rx) = mpsc::channel::<io::Result<Bytes>>(BODY_CHANNEL_CAPACITY);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let egress = egress.clone();
    let worker = tokio::task::spawn_blocking(move || {
        run_worker(
            target,
            head,
            body,
            timeout,
            egress,
            worker_cancel,
            head_tx,
            body_tx,
        );
    });

    let head = match tokio::time::timeout(timeout, head_rx).await {
        Ok(Ok(Ok(head))) => head,
        Ok(Ok(Err(error))) => {
            let _ = worker.await;
            return Err(error);
        }
        Ok(Err(_)) => {
            let _ = worker.await;
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "bun transport: connection closed before the response head",
            ));
        }
        Err(_) => {
            // The worker polls the flag every READ_POLL; setting it tears the
            // socket down instead of waiting for the deadline.
            cancel.store(true, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "bun transport: timeout awaiting response headers",
            ));
        }
    };

    Ok(RawResponse {
        status: head.status,
        headers: head.headers,
        body: Box::pin(tokio_stream::wrappers::ReceiverStream::new(body_rx)),
    })
}

/// Parsed request target. Kept apart from `reqwest::Url` so the worker owns
/// only plain data.
struct Target {
    scheme: String,
    host: String,
    port: u16,
    /// path + query, as sent on the request line
    path: String,
}

impl Target {
    fn parse(url: &str) -> io::Result<Self> {
        let parsed = reqwest::Url::parse(url).map_err(invalid_input)?;
        let scheme = parsed.scheme().to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bun transport: unsupported URL scheme '{scheme}'"),
            ));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bun transport: URL has no host"))?
            .to_string();
        let port = parsed.port_or_known_default().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "bun transport: URL has no port")
        })?;
        let mut path = parsed.path().to_string();
        if let Some(query) = parsed.query() {
            path.push('?');
            path.push_str(query);
        }
        Ok(Self { scheme, host, port, path })
    }

    /// `Host` header value: the authority with a default port omitted, IPv6
    /// bracketed (the way Bun takes it from the parsed URL).
    fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let default_port = if self.scheme == "https" { 443 } else { 80 };
        if self.port == default_port {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    /// `host:port` without the default-port elision: what CONNECT and SOCKS5
    /// must name.
    fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// `InvalidInput` wrapper for URL errors.
fn invalid_input<E: std::fmt::Display>(error: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("bun transport: {error}"))
}

/// Render the request head exactly like Bun's `fetch` writer:
///  1. the WebKit common headers this request carries, in their fixed
///     spelling and order (Content-Length sits in this block when there is a
///     body);
///  2. every other header, lowercased and sorted, exactly as the official CLI
///     hands it to `fetch`;
///  3. the headers `fetch` adds itself: `Connection`, `User-Agent` and
///     `Accept` unless the caller set them, `Host`, `Accept-Encoding`, and a
///     trailing `Content-Length` only when it was not written in step 1.
fn render_request_head(
    target: &Target,
    method: &str,
    headers: &[(String, String)],
    body_len: Option<usize>,
) -> io::Result<Vec<u8>> {
    // Header names are case-insensitive; Bun lowercases the application
    // headers but writes the common block with its canonical spelling.
    let mut values: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        // These belong to the transport: framing and the connection itself are
        // rendered by this function, never by the caller.
        if matches!(lower.as_str(), "host" | "connection" | "content-length" | "transfer-encoding") {
            continue;
        }
        values
            .entry(lower)
            .and_modify(|existing| {
                existing.push_str(", ");
                existing.push_str(value);
            })
            .or_insert_with(|| value.clone());
    }
    // fetch writes its default `Accept: */*` in the trailing block, where the
    // official clients' Accept appears; an explicit value stays in the sorted
    // application block.
    if values.get("accept").is_some_and(|value| value == "*/*") {
        values.remove("accept");
    }
    let has_user_agent = values.contains_key("user-agent");
    let has_accept = values.contains_key("accept");
    let has_accept_encoding = values.contains_key("accept-encoding");

    let mut head: Vec<u8> = Vec::with_capacity(512);
    head.extend_from_slice(method.as_bytes());
    head.push(b' ');
    head.extend_from_slice(target.path.as_bytes());
    head.extend_from_slice(b" HTTP/1.1\r\n");
    let mut wrote_length = false;
    for name in BUN_COMMON_HEADERS {
        let lower = name.to_ascii_lowercase();
        if lower == "content-length" && body_len.is_some() {
            let length = body_len.unwrap_or_default();
            head.extend_from_slice(format!("{name}: {length}\r\n").as_bytes());
            wrote_length = true;
            continue;
        }
        if let Some(value) = values.remove(&lower) {
            head.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
    }
    // The rest is sorted as Bun's fetch sorts it; the names stay lowercase.
    for (name, value) in std::mem::take(&mut values) {
        head.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    head.extend_from_slice(b"Connection: keep-alive\r\n");
    if !has_user_agent {
        head.extend_from_slice(format!("User-Agent: {FALLBACK_USER_AGENT}\r\n").as_bytes());
    }
    if !has_accept {
        head.extend_from_slice(b"Accept: */*\r\n");
    }
    head.extend_from_slice(format!("Host: {}\r\n", target.host_header()).as_bytes());
    if !has_accept_encoding {
        head.extend_from_slice(b"Accept-Encoding: gzip, deflate, br, zstd\r\n");
    }
    if let Some(length) = body_len {
        if !wrote_length {
            head.extend_from_slice(format!("Content-Length: {length}\r\n").as_bytes());
        }
    }
    head.extend_from_slice(b"\r\n");
    Ok(head)
}

/// An HTTP token / header value that cannot split the request head.
fn valid_header_field(name: &str, value: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    for byte in name.bytes() {
        if byte <= b' ' || byte >= 0x7f || br#""(),/:;<=>?@[\]{}"#.contains(&byte) {
            return false;
        }
    }
    for byte in value.bytes() {
        if (byte < b' ' && byte != b'\t') || byte == 0x7f {
            return false;
        }
    }
    true
}

/// The SSL context every free-lane connection uses. Building it reads the
/// system CA bundle, so it is done once per process.
fn connector() -> io::Result<&'static SslConnector> {
    static CONNECTOR: OnceLock<Result<SslConnector, String>> = OnceLock::new();
    CONNECTOR
        .get_or_init(|| build_connector().map_err(|error| format!("bun transport: {error}")))
        .as_ref()
        .map_err(|message| io::Error::other(message.clone()))
}

/// BoringSSL context configured with Bun's ClientHello parameters.
///
/// `SslConnector::builder` already installs the system trust store
/// (`set_default_verify_paths`) and peer verification, so certificates are
/// validated like any other HTTPS client; the hostname is checked by
/// `ConnectConfiguration::into_ssl`.
fn build_connector() -> Result<SslConnector, boring::error::ErrorStack> {
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    // The only ALPN protocol Bun offers.
    builder.set_alpn_protos(b"\x08http/1.1")?;
    // status_request (OCSP) and signed_certificate_timestamp: Bun sends both,
    // and their absence changes the JA4 extension count from 13 to 11.
    builder.enable_signed_cert_timestamps();
    builder.set_curves_list(BUN_CURVE_LIST)?;
    builder.set_sigalgs_list(BUN_SIGNATURE_ALGORITHMS)?;
    Ok(builder.build())
}

/// A connected socket, TLS or plain. Plain exists for the local mock upstream
/// in tests; production free-lane traffic is always HTTPS.
enum Socket {
    Tls(Box<boring::ssl::SslStream<TcpStream>>),
    Plain(TcpStream),
}

impl Socket {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Tls(stream) => stream.get_ref().set_read_timeout(timeout),
            Self::Plain(stream) => stream.set_read_timeout(timeout),
        }
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Tls(stream) => stream.get_ref().set_write_timeout(timeout),
            Self::Plain(stream) => stream.set_write_timeout(timeout),
        }
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tls(stream) => stream.read(buf),
            Self::Plain(stream) => stream.read(buf),
        }
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tls(stream) => stream.write(buf),
            Self::Plain(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tls(stream) => stream.flush(),
            Self::Plain(stream) => stream.flush(),
        }
    }
}

/// Parsed response head.
struct RawHead {
    status: u16,
    headers: HeaderMap,
}

/// How the response body is delimited.
#[derive(Clone, Copy)]
enum BodyFraming {
    Chunked,
    Length(u64),
    Eof,
}

/// Deadline + cancellation probe shared by every blocking read.
struct ReadContext {
    deadline: Instant,
    cancel: Arc<AtomicBool>,
}

impl ReadContext {
    fn check(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "bun transport: request cancelled",
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "bun transport: request deadline reached",
            ));
        }
        Ok(())
    }

    /// Remaining time, clamped to the handshake budget.
    fn handshake_budget(&self) -> Duration {
        self.deadline
            .saturating_duration_since(Instant::now())
            .min(HANDSHAKE_TIMEOUT)
            .max(Duration::from_millis(1))
    }

    /// A context for the proxy handshake: the request deadline, further
    /// clamped to [`HANDSHAKE_TIMEOUT`] so a black-holed proxy cannot sit on
    /// the whole request timeout before the lane rotates to the next exit.
    fn tunnel(&self) -> ReadContext {
        ReadContext {
            deadline: self.deadline.min(Instant::now() + HANDSHAKE_TIMEOUT),
            cancel: Arc::clone(&self.cancel),
        }
    }
}

/// Blocking worker: connect, handshake, send the request, parse the head and
/// pump the body through the channel.
// The transport's data-flow pieces are all flat values on purpose: the worker
// is a blocking function with no borrows, so every input is owned and moved.
#[allow(clippy::too_many_arguments)]
fn run_worker(
    target: Target,
    head: Vec<u8>,
    body: Bytes,
    timeout: Duration,
    egress: Egress,
    cancel: Arc<AtomicBool>,
    head_tx: oneshot::Sender<io::Result<RawHead>>,
    body_tx: mpsc::Sender<io::Result<Bytes>>,
) {
    let context = ReadContext { deadline: Instant::now() + timeout, cancel };
    let mut head_tx = Some(head_tx);
    let result = exchange(&target, &egress, &head, &body, &context, &mut head_tx, &body_tx);
    if let Err(error) = result {
        match head_tx.take() {
            Some(sender) => {
                let _ = sender.send(Err(error));
            }
            None => {
                let _ = body_tx.blocking_send(Err(error));
            }
        }
    }
}

/// The whole blocking exchange once the worker owns the data.
fn exchange(
    target: &Target,
    egress: &Egress,
    head: &[u8],
    body: &Bytes,
    context: &ReadContext,
    head_tx: &mut Option<oneshot::Sender<io::Result<RawHead>>>,
    body_tx: &mpsc::Sender<io::Result<Bytes>>,
) -> io::Result<()> {
    let mut socket = connect(target, egress, context)?;
    socket.set_write_timeout(Some(context.handshake_budget()))?;
    socket.write_all(head)?;
    if !body.is_empty() {
        socket.write_all(body)?;
    }
    socket.flush()?;
    // After the handshake the reader polls at READ_POLL so the deadline and
    // the cancellation flag are observed while the response is in flight.
    socket.set_read_timeout(Some(context.handshake_budget().min(READ_POLL)))?;
    let mut reader = BufReader::with_capacity(8 * 1024, socket);
    let (status, headers, framing) = read_response_head(&mut reader, context)?;
    let changed = match head_tx.take() {
        Some(sender) => sender.send(Ok(RawHead { status, headers: headers.clone() })).is_ok(),
        None => false,
    };
    if !changed {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "bun transport: response head receiver dropped",
        ));
    }
    stream_body(&mut reader, framing, &headers, body_tx, context)
}

/// Open the wire path to the upstream: direct TCP, or a proxy tunnel followed
/// by the Bun TLS handshake. A proxy that cannot be reached or refuses the
/// tunnel is reported as an [`EgressFailure`], so the free-lane loop can rotate
/// to another exit instead of blaming the credential.
fn connect(target: &Target, egress: &Egress, context: &ReadContext) -> io::Result<Socket> {
    match egress {
        Egress::Direct => {
            let stream = dial_tcp(&target.host, target.port, context)?;
            handshake(target, stream, context)
        }
        Egress::HttpConnect { url } => {
            let proxy = parse_proxy(url)?;
            if proxy.scheme == ProxyScheme::Https {
                return Err(egress_failure(
                    "tls-to-proxy",
                    "https:// proxies are not supported by this transport; configure an http:// or socks5:// proxy",
                ));
            }
            let stream = dial_proxy(&proxy, context)?;
            let tunnel = context.tunnel();
            let stream = http_connect_tunnel(&proxy, target, stream, &tunnel)?;
            handshake(target, stream, context)
        }
        Egress::Socks5 { url } => {
            let proxy = parse_proxy(url)?;
            let stream = dial_proxy(&proxy, context)?;
            let tunnel = context.tunnel();
            let stream = socks5_tunnel(&proxy, target, stream, false, &tunnel)?;
            handshake(target, stream, context)
        }
        Egress::Socks5h { url } => {
            let proxy = parse_proxy(url)?;
            let stream = dial_proxy(&proxy, context)?;
            let tunnel = context.tunnel();
            let stream = socks5_tunnel(&proxy, target, stream, true, &tunnel)?;
            handshake(target, stream, context)
        }
    }
}

/// Parse the proxy URL of an [`Egress`] variant, turning a parse error into an
/// egress failure (a bad configuration is visible and retryable elsewhere).
fn parse_proxy(url: &str) -> io::Result<ProxyUrl> {
    ProxyUrl::parse(url).map_err(|message| egress_failure("proxy-url", message))
}

/// Dial the proxy itself. Any failure here is a property of the exit, not of
/// the upstream request.
fn dial_proxy(proxy: &ProxyUrl, context: &ReadContext) -> io::Result<TcpStream> {
    dial_tcp(&proxy.host, proxy.port, context)
        .map_err(|error| egress_failure("connect", format!("{}: {error}", proxy.authority())))
}

/// Resolve and connect a TCP socket inside the shared handshake budget.
fn dial_tcp(host: &str, port: u16, context: &ReadContext) -> io::Result<TcpStream> {
    let mut last_error: Option<io::Error> = None;
    for address in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, context.handshake_budget()) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "bun transport: no address resolved")
    }))
}

/// Establish an HTTP CONNECT tunnel: send the CONNECT request (with Basic
/// proxy auth when the URL carried a user), require a 2xx answer, and hand the
/// socket back for the TLS handshake. Everything after the 2xx is upstream
/// traffic, not HTTP.
fn http_connect_tunnel(
    proxy: &ProxyUrl,
    target: &Target,
    mut stream: TcpStream,
    context: &ReadContext,
) -> io::Result<TcpStream> {
    let authority = target.authority();
    let mut request = String::with_capacity(256);
    request.push_str(&format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n"));
    // Ask the proxy to keep the tunnel open; every proxy ignores the header or
    // keeps the connection alive by default.
    request.push_str("Proxy-Connection: keep-alive\r\n");
    if let Some(value) = proxy.basic_authorization() {
        request.push_str(&format!("Proxy-Authorization: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.set_write_timeout(Some(context.handshake_budget()))?;
    stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|error| egress_failure("connect-write", format!("{}: {error}", proxy.authority())))?;
    let block = read_proxy_head(&mut stream, context)?;
    let head = String::from_utf8_lossy(&block);
    let status_line = head.lines().next().unwrap_or_default();
    let status = parse_status_line(status_line)
        .map_err(|_| egress_failure("connect-reply", format!("malformed proxy reply '{status_line}'")))?;
    if !(200..300).contains(&status) {
        // 407 is the common one (bad proxy credentials), 401/403/429 a gated
        // proxy. The reply's headers may echo a server banner, so only the
        // status is surfaced.
        return Err(egress_failure(
            "connect-refused",
            format!("proxy {} answered CONNECT with {status}", proxy.authority()),
        ));
    }
    Ok(stream)
}

/// Read a proxy reply head (byte at a time, so nothing past `\r\n\r\n` is
/// consumed from the tunnel).
fn read_proxy_head(stream: &mut TcpStream, context: &ReadContext) -> io::Result<Vec<u8>> {
    stream.set_read_timeout(Some(context.handshake_budget().min(READ_POLL)))?;
    let mut out: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while out.len() < MAX_PROXY_HEAD_BYTES {
        let read = read_polling(stream, &mut byte, context).map_err(|error| {
            egress_failure("connect-read", error.to_string())
        })?;
        if read == 0 {
            return Err(egress_failure(
                "connect-read",
                "proxy closed the connection before replying",
            ));
        }
        out.push(byte[0]);
        if out.len() >= 4 && &out[out.len() - 4..] == b"\r\n\r\n" {
            return Ok(out);
        }
    }
    Err(egress_failure("connect-read", "proxy reply head too large"))
}

/// Establish a SOCKS5 tunnel (RFC 1928 + RFC 1929 user/password auth).
///
/// `local_dns` selects the scheme contract: `socks5h://` resolves the target
/// hostname locally and sends an IP literal, `socks5://` hands the hostname to
/// the proxy (ATYP=domain).
fn socks5_tunnel(
    proxy: &ProxyUrl,
    target: &Target,
    mut stream: TcpStream,
    local_dns: bool,
    context: &ReadContext,
) -> io::Result<TcpStream> {
    stream.set_write_timeout(Some(context.handshake_budget()))?;
    stream.set_read_timeout(Some(context.handshake_budget().min(READ_POLL)))?;
    let authority = proxy.authority();
    // Greeting: offer no-auth and, when credentials exist, user/password.
    let methods: &[u8] = if proxy.username.is_some() { &[0x00, 0x02] } else { &[0x00] };
    let mut greeting = Vec::with_capacity(2 + methods.len());
    greeting.push(0x05);
    greeting.push(methods.len() as u8);
    greeting.extend_from_slice(methods);
    write_all_egress(&mut stream, &greeting, &authority, "socks5-greeting")?;
    let mut reply = [0u8; 2];
    read_exact_egress(&mut stream, &mut reply, &authority, "socks5-greeting", context)?;
    if reply[0] != 0x05 {
        return Err(egress_failure(
            "socks5-greeting",
            format!("proxy {authority} is not SOCKS5 (version {})", reply[0]),
        ));
    }
    match reply[1] {
        0x00 => {}
        0x02 => socks5_authenticate(proxy, &mut stream, &authority, context)?,
        other => {
            return Err(egress_failure(
                "socks5-auth",
                format!("proxy {authority} selected unsupported auth method {other:#04x}"),
            ))
        }
    }
    // CONNECT request.
    let mut request = Vec::with_capacity(32);
    request.extend_from_slice(&[0x05, 0x01, 0x00]);
    if local_dns {
        let address = resolve_target(target)?;
        push_socks5_address(&mut request, &address);
    } else if let Ok(address) = target.host.parse::<IpAddr>() {
        push_socks5_address(&mut request, &address);
    } else {
        let host = target.host.as_bytes();
        if host.len() > 255 {
            return Err(egress_failure("socks5-connect", "target hostname is longer than 255 bytes"));
        }
        request.push(0x03);
        request.push(host.len() as u8);
        request.extend_from_slice(host);
    }
    request.extend_from_slice(&target.port.to_be_bytes());
    write_all_egress(&mut stream, &request, &authority, "socks5-connect")?;
    // Reply: version, status, reserved, address type, address, port.
    let mut head = [0u8; 4];
    read_exact_egress(&mut stream, &mut head, &authority, "socks5-connect", context)?;
    if head[0] != 0x05 || head[2] != 0x00 {
        return Err(egress_failure(
            "socks5-connect",
            format!("proxy {authority} sent a malformed SOCKS5 reply"),
        ));
    }
    if head[1] != 0x00 {
        return Err(egress_failure(
            "socks5-connect",
            format!("proxy {authority} refused CONNECT: {}", socks5_reply_message(head[1])),
        ));
    }
    let address_len = match head[3] {
        0x01 => 4usize,
        0x04 => 16usize,
        0x03 => {
            let mut length = [0u8; 1];
            read_exact_egress(&mut stream, &mut length, &authority, "socks5-connect", context)?;
            length[0] as usize
        }
        other => {
            return Err(egress_failure(
                "socks5-connect",
                format!("proxy {authority} sent unknown address type {other:#04x}"),
            ))
        }
    };
    let mut discard = vec![0u8; address_len + 2];
    read_exact_egress(&mut stream, &mut discard, &authority, "socks5-connect", context)?;
    Ok(stream)
}

/// RFC 1929 username/password sub-negotiation.
fn socks5_authenticate(
    proxy: &ProxyUrl,
    stream: &mut TcpStream,
    authority: &str,
    context: &ReadContext,
) -> io::Result<()> {
    let Some(username) = proxy.username.as_deref() else {
        return Err(egress_failure(
            "socks5-auth",
            format!("proxy {authority} requires user/password auth but the URL carries none"),
        ));
    };
    let password = proxy.password.as_deref().unwrap_or("");
    if username.len() > 255 || password.len() > 255 {
        return Err(egress_failure("socks5-auth", "proxy credentials are longer than 255 bytes"));
    }
    let mut request = Vec::with_capacity(3 + username.len() + password.len());
    request.push(0x01);
    request.push(username.len() as u8);
    request.extend_from_slice(username.as_bytes());
    request.push(password.len() as u8);
    request.extend_from_slice(password.as_bytes());
    write_all_egress(stream, &request, authority, "socks5-auth")?;
    let mut reply = [0u8; 2];
    read_exact_egress(stream, &mut reply, authority, "socks5-auth", context)?;
    if reply[0] != 0x01 || reply[1] != 0x00 {
        return Err(egress_failure(
            "socks5-auth",
            format!("proxy {authority} rejected the username/password (status {})", reply[1]),
        ));
    }
    Ok(())
}

/// Resolve the target host to one address (local-DNS SOCKS5 scheme).
fn resolve_target(target: &Target) -> io::Result<IpAddr> {
    if let Ok(address) = target.host.parse::<IpAddr>() {
        return Ok(address);
    }
    (target.host.as_str(), target.port)
        .to_socket_addrs()?
        .next()
        .map(|address| address.ip())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("bun transport: could not resolve '{}' for the SOCKS5 tunnel", target.host),
            )
        })
}

fn push_socks5_address(request: &mut Vec<u8>, address: &IpAddr) {
    match address {
        IpAddr::V4(v4) => {
            request.push(0x01);
            request.extend_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            request.push(0x04);
            request.extend_from_slice(&v6.octets());
        }
    }
}

fn socks5_reply_message(code: u8) -> &'static str {
    match code {
        0x01 => "general server failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown error",
    }
}

fn write_all_egress(
    stream: &mut TcpStream,
    bytes: &[u8],
    authority: &str,
    stage: &'static str,
) -> io::Result<()> {
    stream.write_all(bytes).and_then(|()| stream.flush()).map_err(|error| {
        egress_failure(stage, format!("proxy {authority}: {error}"))
    })
}

fn read_exact_egress(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    authority: &str,
    stage: &'static str,
    context: &ReadContext,
) -> io::Result<()> {
    read_exact_polling(stream, buffer, context).map_err(|error| {
        egress_failure(stage, format!("proxy {authority}: {error}"))
    })
}

/// Wrap the TCP connection in TLS when the target is HTTPS.
fn handshake(target: &Target, stream: TcpStream, context: &ReadContext) -> io::Result<Socket> {
    if target.scheme != "https" {
        return Ok(Socket::Plain(stream));
    }
    let budget = context.handshake_budget();
    let _ = stream.set_read_timeout(Some(budget));
    let _ = stream.set_write_timeout(Some(budget));
    let connector = connector()?;
    let config = connector
        .configure()
        .map_err(|error| io::Error::other(format!("bun transport: {error}")))?;
    let mut ssl = config
        .into_ssl(&target.host)
        .map_err(|error| io::Error::other(format!("bun transport: {error}")))?;
    // Bun requests OCSP stapling; this is the status_request extension.
    ssl.set_status_type(StatusType::OCSP)
        .map_err(|error| io::Error::other(format!("bun transport: {error}")))?;
    let stream = ssl
        .connect(stream)
        .map_err(|error| io::Error::other(format!("bun transport: TLS handshake failed: {error}")))?;
    Ok(Socket::Tls(Box::new(stream)))
}

/// Read and parse the response head, skipping interim 1xx responses.
fn read_response_head<R: Read>(
    reader: &mut R,
    context: &ReadContext,
) -> io::Result<(u16, HeaderMap, BodyFraming)> {
    loop {
        let block = read_head_block(reader, context)?;
        let text = std::str::from_utf8(&block[..block.len() - 4])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bun transport: non-UTF-8 response head"))?;
        let mut lines = text.split("\r\n");
        let status_line = lines.next().unwrap_or_default();
        let status = parse_status_line(status_line)?;
        let mut headers = HeaderMap::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (name, value) = line.split_once(':').ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "bun transport: malformed response header")
            })?;
            let name = HeaderName::from_bytes(name.trim().as_bytes()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "bun transport: invalid response header name")
            })?;
            let value = HeaderValue::from_str(value.trim()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "bun transport: invalid response header value")
            })?;
            headers.append(name, value);
        }
        // 103 Early Hints and friends carry no body; keep reading until the
        // final response. 101 switches protocols and is never used here.
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        let framing = response_framing(&headers)?;
        return Ok((status, headers, framing));
    }
}

/// Read bytes up to and including the blank line that ends a head.
fn read_head_block<R: Read>(reader: &mut R, context: &ReadContext) -> io::Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while out.len() < MAX_HEAD_BYTES {
        let read = read_polling(reader, &mut byte, context)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "bun transport: connection closed before the response head",
            ));
        }
        out.push(byte[0]);
        if out.len() >= 4 && &out[out.len() - 4..] == b"\r\n\r\n" {
            return Ok(out);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "bun transport: response head too large",
    ))
}

/// Parse `HTTP/1.1 200 OK` into its numeric status.
fn parse_status_line(line: &str) -> io::Result<u16> {
    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bun transport: malformed status line '{line}'"),
        ));
    }
    parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|code| (100..=999).contains(code))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bun transport: malformed status line '{line}'"),
            )
        })
}

/// Determine how the response body is delimited.
fn response_framing(headers: &HeaderMap) -> io::Result<BodyFraming> {
    let transfer_encoding = headers
        .get_all(reqwest::header::TRANSFER_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join(",");
    if transfer_encoding.to_ascii_lowercase().contains("chunked") {
        return Ok(BodyFraming::Chunked);
    }
    if let Some(length) = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
    {
        let length = length.trim().parse::<u64>().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "bun transport: invalid Content-Length")
        })?;
        return Ok(BodyFraming::Length(length));
    }
    Ok(BodyFraming::Eof)
}

/// Pump the response body to the async side.
fn stream_body<R: Read>(
    reader: &mut R,
    framing: BodyFraming,
    headers: &HeaderMap,
    body_tx: &mpsc::Sender<io::Result<Bytes>>,
    context: &ReadContext,
) -> io::Result<()> {
    let encoding = headers
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty() && value != "identity");
    match encoding {
        // A compressed body has to be inflated before it can be used. The
        // free lane only compresses non-streaming JSON (an error envelope), so
        // buffering is safe; a compressed SSE stream is not expected and would
        // be inflated up front rather than truncated.
        Some(encoding) => {
            let raw = read_whole_body(reader, framing, context)?;
            let decoded = decode_body(&encoding, &raw)?;
            if decoded.is_empty() {
                return Ok(());
            }
            send_chunk(body_tx, Bytes::from(decoded), context)
        }
        None => match framing {
            BodyFraming::Chunked => stream_chunked(reader, body_tx, context),
            BodyFraming::Length(length) => stream_fixed(reader, length, body_tx, context),
            BodyFraming::Eof => stream_to_eof(reader, body_tx, context),
        },
    }
}

/// Decode a buffered body for `Content-Encoding`.
///
/// The request advertises `gzip, deflate, br, zstd` (Bun's `fetch` list), so
/// every one of those must be decodable here — otherwise an error envelope or
/// a compressed response turns into a false 502 instead of the upstream's real
/// verdict. All four decoders are pure Rust; `zstd` uses `ruzstd` rather than
/// the C library so the build stays free of another native toolchain.
fn decode_body(encoding: &str, raw: &[u8]) -> io::Result<Vec<u8>> {
    use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
    let mut decoded = Vec::new();
    match encoding {
        "identity" => decoded.extend_from_slice(raw),
        "gzip" | "x-gzip" => {
            GzDecoder::new(raw).read_to_end(&mut decoded).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bun transport: gzip decode failed: {error}"),
                )
            })?;
        }
        "deflate" => {
            // RFC 9110 says zlib-wrapped, but some servers send raw DEFLATE.
            let zlib_wrapped = raw.len() >= 2
                && raw[0] & 0x0f == 8
                && (u16::from(raw[0]) << 8 | u16::from(raw[1])) % 31 == 0;
            let read = if zlib_wrapped {
                ZlibDecoder::new(raw).read_to_end(&mut decoded)
            } else {
                DeflateDecoder::new(raw).read_to_end(&mut decoded)
            };
            read.map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bun transport: deflate decode failed: {error}"),
                )
            })?;
        }
        "br" => {
            brotli::Decompressor::new(raw, 4096)
                .read_to_end(&mut decoded)
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("bun transport: brotli decode failed: {error}"),
                    )
                })?;
        }
        "zstd" => {
            let mut reader = ruzstd::decoding::StreamingDecoder::new(raw).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bun transport: zstd frame header invalid: {error}"),
                )
            })?;
            reader.read_to_end(&mut decoded).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bun transport: zstd decode failed: {error}"),
                )
            })?;
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("bun transport: unsupported response Content-Encoding '{other}'"),
            ))
        }
    }
    Ok(decoded)
}

/// Copy exactly `length` bytes of body.
fn stream_fixed<R: Read>(
    reader: &mut R,
    mut remaining: u64,
    body_tx: &mpsc::Sender<io::Result<Bytes>>,
    context: &ReadContext,
) -> io::Result<()> {
    let mut buffer = vec![0u8; BODY_CHUNK];
    while remaining > 0 {
        context.check()?;
        let want = (buffer.len() as u64).min(remaining) as usize;
        let read = read_polling(reader, &mut buffer[..want], context)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "bun transport: connection closed before the body ended",
            ));
        }
        remaining -= read as u64;
        send_chunk(body_tx, Bytes::copy_from_slice(&buffer[..read]), context)?;
    }
    Ok(())
}

/// Copy a body with no declared length until the peer closes the connection.
fn stream_to_eof<R: Read>(
    reader: &mut R,
    body_tx: &mpsc::Sender<io::Result<Bytes>>,
    context: &ReadContext,
) -> io::Result<()> {
    let mut buffer = vec![0u8; BODY_CHUNK];
    loop {
        context.check()?;
        let read = read_polling(reader, &mut buffer, context)?;
        if read == 0 {
            return Ok(());
        }
        send_chunk(body_tx, Bytes::copy_from_slice(&buffer[..read]), context)?;
    }
}

/// Decode a chunked body, forwarding each chunk as it arrives.
fn stream_chunked<R: Read>(
    reader: &mut R,
    body_tx: &mpsc::Sender<io::Result<Bytes>>,
    context: &ReadContext,
) -> io::Result<()> {
    let mut buffer = vec![0u8; BODY_CHUNK];
    loop {
        let size = read_chunk_size(reader, context)?;
        if size == 0 {
            // Trailer section: header lines until the empty one.
            loop {
                if read_line(reader, context)?.is_empty() {
                    return Ok(());
                }
            }
        }
        let mut remaining = size;
        while remaining > 0 {
            context.check()?;
            let want = (buffer.len() as u64).min(remaining) as usize;
            let read = read_polling(reader, &mut buffer[..want], context)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "bun transport: connection closed inside a chunk",
                ));
            }
            remaining -= read as u64;
            send_chunk(body_tx, Bytes::copy_from_slice(&buffer[..read]), context)?;
        }
        // Every chunk is followed by CRLF.
        let mut terminator = [0u8; 2];
        read_exact_polling(reader, &mut terminator, context)?;
        if &terminator != b"\r\n" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bun transport: malformed chunk terminator",
            ));
        }
    }
}

/// Read one chunk-size line (hex, optional extensions) as a number.
fn read_chunk_size<R: Read>(reader: &mut R, context: &ReadContext) -> io::Result<u64> {
    let line = read_line(reader, context)?;
    let size = line.split(';').next().unwrap_or_default().trim();
    u64::from_str_radix(size, 16).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bun transport: invalid chunk size '{line}'"),
        )
    })
}

/// Read one CRLF-terminated line (without the terminator).
fn read_line<R: Read>(reader: &mut R, context: &ReadContext) -> io::Result<String> {
    let mut out: Vec<u8> = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    while out.len() < 8192 {
        let read = read_polling(reader, &mut byte, context)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "bun transport: connection closed inside a line",
            ));
        }
        if byte[0] == b'\n' {
            if out.last() == Some(&b'\r') {
                out.pop();
            }
            return String::from_utf8(out).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "bun transport: non-UTF-8 chunk line")
            });
        }
        out.push(byte[0]);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "bun transport: line too long",
    ))
}

/// Read a whole body into memory (used only when it must be decompressed).
fn read_whole_body<R: Read>(
    reader: &mut R,
    framing: BodyFraming,
    context: &ReadContext,
) -> io::Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    match framing {
        BodyFraming::Chunked => {
            let mut buffer = vec![0u8; BODY_CHUNK];
            loop {
                let size = read_chunk_size(reader, context)?;
                if size == 0 {
                    loop {
                        if read_line(reader, context)?.is_empty() {
                            break;
                        }
                    }
                    break;
                }
                if out.len() as u64 + size > MAX_BUFFERED_BODY_BYTES as u64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "bun transport: compressed body too large",
                    ));
                }
                let mut remaining = size;
                while remaining > 0 {
                    let want = (buffer.len() as u64).min(remaining) as usize;
                    let read = read_polling(reader, &mut buffer[..want], context)?;
                    if read == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "bun transport: connection closed inside a chunk",
                        ));
                    }
                    out.extend_from_slice(&buffer[..read]);
                    remaining -= read as u64;
                }
                let mut terminator = [0u8; 2];
                read_exact_polling(reader, &mut terminator, context)?;
            }
        }
        BodyFraming::Length(length) => {
            if length > MAX_BUFFERED_BODY_BYTES as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "bun transport: compressed body too large",
                ));
            }
            out.resize(length as usize, 0);
            read_exact_polling(reader, &mut out, context)?;
        }
        BodyFraming::Eof => {
            let mut buffer = vec![0u8; BODY_CHUNK];
            loop {
                let read = read_polling(reader, &mut buffer, context)?;
                if read == 0 {
                    break;
                }
                if out.len() + read > MAX_BUFFERED_BODY_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "bun transport: compressed body too large",
                    ));
                }
                out.extend_from_slice(&buffer[..read]);
            }
        }
    }
    Ok(out)
}

/// One read that retries while the socket poll times out, checking the request
/// deadline and the cancellation flag between attempts.
fn read_polling<R: Read>(reader: &mut R, buffer: &mut [u8], context: &ReadContext) -> io::Result<usize> {
    loop {
        match reader.read(buffer) {
            Ok(read) => return Ok(read),
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut =>
            {
                context.check()?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// Fill `buffer` completely, retrying across socket poll timeouts.
fn read_exact_polling<R: Read>(
    reader: &mut R,
    buffer: &mut [u8],
    context: &ReadContext,
) -> io::Result<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = read_polling(reader, &mut buffer[filled..], context)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "bun transport: connection closed mid-body",
            ));
        }
        filled += read;
    }
    Ok(())
}

/// Hand one body chunk to the async side. A dropped receiver is a cancelled
/// request: the error tears the worker down and closes the socket.
fn send_chunk(
    body_tx: &mpsc::Sender<io::Result<Bytes>>,
    chunk: Bytes,
    context: &ReadContext,
) -> io::Result<()> {
    if chunk.is_empty() {
        return Ok(());
    }
    context.check()?;
    body_tx.blocking_send(Ok(chunk)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "bun transport: response receiver dropped",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str) -> Target {
        Target::parse(url).unwrap()
    }

    /// Every application header is lowercased and sorted, the common headers
    /// keep Bun's spelling/order, and framing headers come last — the exact
    /// layout captured from the official CLI's `fetch`.
    #[test]
    fn request_head_matches_the_bun_fetch_layout() {
        let headers = vec![
            ("authorization".to_string(), "Bearer public".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
            ("user-agent".to_string(), "opencode/latest/2.1.4/cli".to_string()),
            ("x-opencode-client".to_string(), "cli".to_string()),
            ("x-opencode-project".to_string(), "global".to_string()),
            ("x-opencode-session".to_string(), "ses_x".to_string()),
            ("x-session-affinity".to_string(), "ses_x".to_string()),
            ("x-session-id".to_string(), "ses_x".to_string()),
            ("x-opencode-session-id".to_string(), "ses_x".to_string()),
        ];
        let head = render_request_head(
            &target("https://opencode.ai/inference/openai/v1/chat/completions"),
            "POST",
            &headers,
            Some(7),
        )
        .unwrap();
        let text = String::from_utf8(head).unwrap();
        let expected = "POST /inference/openai/v1/chat/completions HTTP/1.1\r\n\
Authorization: Bearer public\r\n\
Content-Length: 7\r\n\
Content-Type: application/json\r\n\
User-Agent: opencode/latest/2.1.4/cli\r\n\
x-opencode-client: cli\r\n\
x-opencode-project: global\r\n\
x-opencode-session: ses_x\r\n\
x-opencode-session-id: ses_x\r\n\
x-session-affinity: ses_x\r\n\
x-session-id: ses_x\r\n\
Connection: keep-alive\r\n\
Accept: */*\r\n\
Host: opencode.ai\r\n\
Accept-Encoding: gzip, deflate, br, zstd\r\n\
\r\n";
        assert_eq!(text, expected);
    }

    /// The transport headers belong to the transport even if a caller tries to
    /// set them, and a non-default port survives in the Host header.
    #[test]
    fn request_head_ignores_caller_framing_headers() {
        let headers = vec![
            ("Host".to_string(), "evil.example".to_string()),
            ("Content-Length".to_string(), "999".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ("Connection".to_string(), "close".to_string()),
        ];
        let head = render_request_head(
            &target("http://127.0.0.1:8080/v1/x"),
            "POST",
            &headers,
            Some(3),
        )
        .unwrap();
        let text = String::from_utf8(head).unwrap();
        assert!(text.contains("\r\nHost: 127.0.0.1:8080\r\n"), "{text}");
        assert!(text.contains("\r\nContent-Length: 3\r\n"), "{text}");
        assert!(text.contains("\r\nConnection: keep-alive\r\n"), "{text}");
        assert!(!text.contains("evil.example"), "{text}");
        assert!(!text.contains("chunked"), "{text}");
    }

    #[test]
    fn header_fields_that_could_split_the_request_are_rejected() {
        assert!(!valid_header_field("x-test", "a\r\nInjected: yes"));
        assert!(!valid_header_field("bad name", "value"));
        assert!(valid_header_field("x-opencode-session", "ses_abc"));
    }

    #[test]
    fn framing_prefers_chunked_over_content_length() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        headers.insert(reqwest::header::CONTENT_LENGTH, "5".parse().unwrap());
        assert!(matches!(response_framing(&headers).unwrap(), BodyFraming::Chunked));
        let mut length_only = HeaderMap::new();
        length_only.insert(reqwest::header::CONTENT_LENGTH, "5".parse().unwrap());
        assert!(matches!(response_framing(&length_only).unwrap(), BodyFraming::Length(5)));
    }

    #[test]
    fn status_line_parses_like_http() {
        assert_eq!(parse_status_line("HTTP/1.1 200 OK").unwrap(), 200);
        assert_eq!(parse_status_line("HTTP/1.1 429 Too Many Requests").unwrap(), 429);
        assert!(parse_status_line("garbage").is_err());
    }

    /// Every encoding the request advertises must decode, or an upstream error
    /// envelope turns into a false 502 instead of the upstream's real verdict.
    #[test]
    fn decode_body_decodes_every_advertised_encoding() {
        let plain = b"hello free lane";
        assert_eq!(decode_body("identity", plain).unwrap(), plain);

        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(plain).unwrap();
        assert_eq!(decode_body("gzip", &gzip.finish().unwrap()).unwrap(), plain);

        let mut deflate =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        deflate.write_all(plain).unwrap();
        assert_eq!(decode_body("deflate", &deflate.finish().unwrap()).unwrap(), plain);

        let mut brotli = Vec::new();
        brotli::enc::BrotliCompress(
            &mut std::io::Cursor::new(plain),
            &mut brotli,
            &brotli::enc::BrotliEncoderParams::default(),
        )
        .unwrap();
        assert_eq!(decode_body("br", &brotli).unwrap(), plain);

        // A one-frame zstd stream produced by the reference encoder for
        // "hello free lane" (`ruzstd` is decode-only, so the frame is fixed).
        let zstd: &[u8] = &[
            0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x0f, 0x79, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f,
            0x20, 0x66, 0x72, 0x65, 0x65, 0x20, 0x6c, 0x61, 0x6e, 0x65,
        ];
        assert_eq!(decode_body("zstd", zstd).unwrap(), plain);

        assert!(decode_body("compress", plain).is_err());
    }

    /// Collect a [`RawResponse`] body for assertions.
    async fn collect_body(mut body: BoxStream<'static, io::Result<Bytes>>) -> Vec<u8> {
        use futures::StreamExt as _;
        let mut out = Vec::new();
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    /// A compressed upstream body must be decoded end to end on the transport
    /// path: a brotli-encoded `429 FreeUsageLimitError` envelope has to arrive
    /// as the original JSON, not as a fake 502 from the decoder.
    #[tokio::test]
    async fn post_decodes_a_brotli_encoded_error_body() {
        use std::io::BufRead as _;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload = br#"{"type":"error","error":{"type":"FreeUsageLimitError","message":"Free usage limit reached"}}"#.to_vec();
        let mut encoded = Vec::new();
        brotli::enc::BrotliCompress(
            &mut std::io::Cursor::new(&payload),
            &mut encoded,
            &brotli::enc::BrotliEncoderParams::default(),
        )
        .unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap();
                    }
                }
                let mut request_body = vec![0u8; content_length];
                reader.read_exact(&mut request_body).unwrap();
            }
            let head = format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Encoding: br\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                encoded.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&encoded).unwrap();
            stream.flush().unwrap();
        });

        let response = post(
            &format!("http://{address}/inference/openai/v1/chat/completions"),
            &[("content-type".into(), "application/json".into())],
            Bytes::from_static(b"{}"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(response.status, 429);
        let decoded = collect_body(response.body).await;
        let text = String::from_utf8(decoded).unwrap();
        assert!(text.contains("FreeUsageLimitError"), "{text}");
        server.join().unwrap();
    }
}
#[cfg(test)]
mod egress_tests {
    use super::{is_egress_failure, mask_proxy_url, post_via, validate_proxy_url, Egress};
    use bytes::Bytes;
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    // -----------------------------------------------------------------
    // Mock upstream and proxy helpers (plain TCP, no TLS: the tunnel path is
    // what these tests exercise; TLS-over-tunnel reuses the direct handshake).
    // -----------------------------------------------------------------

    fn read_head_block(stream: &mut TcpStream) -> Option<String> {
        let mut out: Vec<u8> = Vec::new();
        let mut byte = [0u8; 1];
        while out.len() < 64 * 1024 {
            let read = stream.read(&mut byte).ok()?;
            if read == 0 {
                return None;
            }
            out.push(byte[0]);
            if out.ends_with(b"\r\n\r\n") {
                return Some(String::from_utf8_lossy(&out).to_string());
            }
        }
        None
    }

    fn read_exact_or(stream: &mut TcpStream, buffer: &mut [u8]) -> bool {
        stream.read_exact(buffer).is_ok()
    }

    /// A local upstream that answers every request with a 200 JSON body and
    /// closes the connection (so the tunnel copy terminates).
    fn spawn_http_upstream() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else { break };
                std::thread::spawn(move || {
                    let Some(head) = read_head_block(&mut stream) else { return };
                    let content_length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let mut body = vec![0u8; content_length];
                    if content_length > 0 && !read_exact_or(&mut stream, &mut body) {
                        return;
                    }
                    let payload = br#"{"ok":true}"#;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = stream.write_all(payload);
                    let _ = stream.flush();
                });
            }
        });
        format!("http://{address}")
    }

    fn pump(mut left: TcpStream, mut right: TcpStream) {
        let mut left_read = left.try_clone().unwrap();
        let mut right_write = right.try_clone().unwrap();
        let forward = std::thread::spawn(move || {
            let _ = std::io::copy(&mut left_read, &mut right_write);
            let _ = right_write.shutdown(std::net::Shutdown::Write);
        });
        let _ = std::io::copy(&mut right, &mut left);
        let _ = forward.join();
    }

    #[derive(Debug, Clone)]
    struct ProxyHit {
        connect_line: String,
        authorization: Option<String>,
    }

    /// A local HTTP CONNECT proxy. `reply = Some("407 …")` answers the CONNECT
    /// with that status; `None` accepts it and tunnels. `expected_auth` makes
    /// the proxy require the `Proxy-Authorization` value (407 otherwise).
    fn spawn_connect_proxy(
        reply: Option<&'static str>,
        expected_auth: Option<String>,
    ) -> (String, Arc<Mutex<Vec<ProxyHit>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let hits: Arc<Mutex<Vec<ProxyHit>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&hits);
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut client) = incoming else { break };
                let hits = Arc::clone(&recorded);
                let expected_auth = expected_auth.clone();
                std::thread::spawn(move || {
                    let Some(head) = read_head_block(&mut client) else { return };
                    let mut lines = head.split("\r\n");
                    let connect_line = lines.next().unwrap_or_default().to_string();
                    let mut authorization = None;
                    for line in lines {
                        if let Some(value) = line.strip_prefix("Proxy-Authorization: ") {
                            authorization = Some(value.trim().to_string());
                        }
                    }
                    hits.lock().unwrap().push(ProxyHit {
                        connect_line: connect_line.clone(),
                        authorization: authorization.clone(),
                    });
                    if let Some(status) = reply {
                        let _ = write!(
                            client,
                            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        return;
                    }
                    if let Some(expected) = expected_auth.as_deref() {
                        if authorization.as_deref() != Some(expected) {
                            let _ = write!(
                                client,
                                "HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n"
                            );
                            return;
                        }
                    }
                    let Some(authority) = connect_line.split_whitespace().nth(1) else {
                        return;
                    };
                    let (host, port) = match authority.rsplit_once(':') {
                        Some((host, port)) => (host, port.parse::<u16>().unwrap_or(80)),
                        None => (authority, 80),
                    };
                    let Ok(upstream) = TcpStream::connect((host, port)) else {
                        let _ = write!(
                            client,
                            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n"
                        );
                        return;
                    };
                    let _ = client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
                    pump(client, upstream);
                });
            }
        });
        (format!("http://{address}"), hits)
    }

    #[derive(Debug, Clone, Copy)]
    enum Socks5Mode {
        NoAuth,
        Password {
            username: &'static str,
            password: &'static str,
        },
        NoAcceptableMethod,
        RefuseConnect(u8),
    }

    #[derive(Debug, Clone)]
    struct Socks5Hit {
        atyp: u8,
        host: String,
        port: u16,
    }

    /// A local SOCKS5 proxy recording what address type/host each CONNECT used.
    fn spawn_socks5_proxy(mode: Socks5Mode) -> (String, Arc<Mutex<Vec<Socks5Hit>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let hits: Arc<Mutex<Vec<Socks5Hit>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&hits);
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut client) = incoming else { break };
                let hits = Arc::clone(&recorded);
                std::thread::spawn(move || {
                    let mut greeting = [0u8; 2];
                    if !read_exact_or(&mut client, &mut greeting) || greeting[0] != 0x05 {
                        return;
                    }
                    let mut methods = vec![0u8; greeting[1] as usize];
                    if !read_exact_or(&mut client, &mut methods) {
                        return;
                    }
                    match mode {
                        Socks5Mode::NoAcceptableMethod => {
                            let _ = client.write_all(&[0x05, 0xff]);
                            return;
                        }
                        Socks5Mode::Password { username, password } => {
                            if !methods.contains(&0x02) {
                                let _ = client.write_all(&[0x05, 0xff]);
                                return;
                            }
                            let _ = client.write_all(&[0x05, 0x02]);
                            let mut version = [0u8; 1];
                            if !read_exact_or(&mut client, &mut version) || version[0] != 0x01 {
                                return;
                            }
                            let mut length = [0u8; 1];
                            if !read_exact_or(&mut client, &mut length) {
                                return;
                            }
                            let mut user = vec![0u8; length[0] as usize];
                            if !read_exact_or(&mut client, &mut user) {
                                return;
                            }
                            if !read_exact_or(&mut client, &mut length) {
                                return;
                            }
                            let mut pass = vec![0u8; length[0] as usize];
                            if !read_exact_or(&mut client, &mut pass) {
                                return;
                            }
                            let ok = user == username.as_bytes() && pass == password.as_bytes();
                            let _ = client.write_all(&[0x01, if ok { 0x00 } else { 0x01 }]);
                            if !ok {
                                return;
                            }
                        }
                        Socks5Mode::NoAuth | Socks5Mode::RefuseConnect(_) => {
                            if !methods.contains(&0x00) {
                                let _ = client.write_all(&[0x05, 0xff]);
                                return;
                            }
                            let _ = client.write_all(&[0x05, 0x00]);
                        }
                    }
                    let mut request = [0u8; 4];
                    if !read_exact_or(&mut client, &mut request) || request[0] != 0x05 {
                        return;
                    }
                    let host = match request[3] {
                        0x01 => {
                            let mut address = [0u8; 4];
                            if !read_exact_or(&mut client, &mut address) {
                                return;
                            }
                            std::net::Ipv4Addr::from(address).to_string()
                        }
                        0x04 => {
                            let mut address = [0u8; 16];
                            if !read_exact_or(&mut client, &mut address) {
                                return;
                            }
                            std::net::Ipv6Addr::from(address).to_string()
                        }
                        0x03 => {
                            let mut length = [0u8; 1];
                            if !read_exact_or(&mut client, &mut length) {
                                return;
                            }
                            let mut name = vec![0u8; length[0] as usize];
                            if !read_exact_or(&mut client, &mut name) {
                                return;
                            }
                            String::from_utf8_lossy(&name).to_string()
                        }
                        _ => return,
                    };
                    let mut port = [0u8; 2];
                    if !read_exact_or(&mut client, &mut port) {
                        return;
                    }
                    let port = u16::from_be_bytes(port);
                    hits.lock().unwrap().push(Socks5Hit {
                        atyp: request[3],
                        host: host.clone(),
                        port,
                    });
                    if let Socks5Mode::RefuseConnect(code) = mode {
                        let _ = client.write_all(&[0x05, code, 0x00, 0x01, 127, 0, 0, 1, 0, 0]);
                        return;
                    }
                    let Ok(upstream) = TcpStream::connect((host.as_str(), port)) else {
                        let _ = client.write_all(&[0x05, 0x05, 0x00, 0x01, 127, 0, 0, 1, 0, 0]);
                        return;
                    };
                    let _ = client.write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0]);
                    pump(client, upstream);
                });
            }
        });
        (format!("127.0.0.1:{}", address.port()), hits)
    }

    fn basic(user: &str, password: &str) -> String {
        use base64::Engine as _;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
        )
    }

    async fn collect_body(mut body: futures::stream::BoxStream<'static, std::io::Result<Bytes>>) -> Vec<u8> {
        use futures::StreamExt as _;
        let mut out = Vec::new();
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    /// `Result::expect_err` needs the Ok type to be Debug; `RawResponse` holds a
    /// live stream and deliberately is not. Match instead.
    fn expect_transport_error(
        context: &str,
        result: std::io::Result<super::RawResponse>,
    ) -> std::io::Error {
        match result {
            Ok(_) => panic!("{context}: expected an error"),
            Err(error) => error,
        }
    }

    // -----------------------------------------------------------------
    // URL classification / masking
    // -----------------------------------------------------------------

    #[test]
    fn an_absent_or_blank_proxy_url_keeps_the_direct_dial() {
        assert_eq!(Egress::from_proxy_url(None), Egress::Direct);
        assert_eq!(Egress::from_proxy_url(Some("")), Egress::Direct);
        assert_eq!(Egress::from_proxy_url(Some("   ")), Egress::Direct);
        assert!(Egress::from_proxy_url(None).is_direct());
        assert_eq!(Egress::from_proxy_url(None).as_str(), "direct");
        assert_eq!(Egress::Direct.label(), None);
        assert_eq!(Egress::Direct.masked_label(), "direct");
    }

    #[test]
    fn proxy_url_schemes_select_the_matching_tunnel() {
        let http = Egress::from_proxy_url(Some(" http://127.0.0.1:7890 "));
        assert_eq!(http, Egress::HttpConnect { url: "http://127.0.0.1:7890".into() });
        assert_eq!(http.as_str(), "http_connect");
        assert_eq!(http.label().as_deref(), Some("http://127.0.0.1:7890"));

        let https = Egress::from_proxy_url(Some("https://proxy.example:8443"));
        assert_eq!(https.as_str(), "http_connect", "https is a CONNECT scheme");
        assert_eq!(https.label().as_deref(), Some("https://proxy.example:8443"));

        let socks5 = Egress::from_proxy_url(Some("socks5://127.0.0.1:1080"));
        assert_eq!(socks5, Egress::Socks5 { url: "socks5://127.0.0.1:1080".into() });
        assert_eq!(socks5.as_str(), "socks5");

        let socks5h = Egress::from_proxy_url(Some("socks5h://user:pass@127.0.0.1:1080"));
        assert_eq!(
            socks5h,
            Egress::Socks5h { url: "socks5h://user:pass@127.0.0.1:1080".into() }
        );
        assert_eq!(socks5h.as_str(), "socks5h");
        // The accounting identity never carries the userinfo.
        assert_eq!(socks5h.label().as_deref(), Some("socks5h://127.0.0.1:1080"));
    }

    #[test]
    fn proxy_urls_are_validated_and_ipv6_hosts_survive_parsing() {
        assert!(validate_proxy_url("http://127.0.0.1:3128").is_ok());
        assert!(validate_proxy_url("socks5://user:pass@127.0.0.1:1080").is_ok());
        assert!(validate_proxy_url("socks5h://[::1]:1080").is_ok());
        assert!(validate_proxy_url("socks5://127.0.0.1").is_err(), "no default SOCKS port");
        assert!(validate_proxy_url("ftp://127.0.0.1:21").is_err());
        assert!(validate_proxy_url("not a url").is_err());

        let ipv6 = Egress::from_proxy_url(Some("socks5h://[::1]:1080"));
        assert_eq!(ipv6.label().as_deref(), Some("socks5h://[::1]:1080"));
    }

    #[test]
    fn status_labels_mask_the_password_and_most_of_the_host() {
        let egress = Egress::from_proxy_url(Some("http://user:super-secret@104.28.208.136:3128"));
        let masked = egress.masked_label();
        assert!(masked.starts_with("http://104.28.*.*:3128"), "{masked}");
        assert!(!masked.contains("super-secret"), "{masked}");
        assert!(!masked.contains("user"), "{masked}");
        assert!(mask_proxy_url("socks5h://alice:hunter2@p.example.com:1080")
            .starts_with("socks5h://p***.example.com:1080"));
        // Debug output must not leak the credentials either.
        let debug = format!("{egress:?}");
        assert!(!debug.contains("super-secret"), "{debug}");
        assert!(!debug.contains("user"), "{debug}");
        // An unparsable URL is never echoed (it may contain the password).
        assert_eq!(mask_proxy_url("garbage://user:secret@"), "<unparsable-proxy>");
    }

    // -----------------------------------------------------------------
    // HTTP CONNECT
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn http_connect_tunnels_a_request_with_basic_proxy_auth() {
        let upstream = spawn_http_upstream();
        let (proxy, hits) = spawn_connect_proxy(None, Some(basic("user", "s3cret")));
        let proxy_url = proxy.replacen("http://", "http://user:s3cret@", 1);
        let response = post_via(
            &format!("{upstream}/inference/v1/chat/completions"),
            &[("content-type".into(), "application/json".into())],
            Bytes::from_static(b"{}"),
            Duration::from_secs(5),
            &Egress::from_proxy_url(Some(&proxy_url)),
        )
        .await
        .expect("the CONNECT tunnel must serve the request");
        assert_eq!(response.status, 200);
        assert_eq!(collect_body(response.body).await, br#"{"ok":true}"#);
        let hits = hits.lock().unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].connect_line.starts_with("CONNECT 127.0.0.1:"),
            "CONNECT names the upstream authority: {}",
            hits[0].connect_line
        );
        assert_eq!(hits[0].authorization.as_deref(), Some(basic("user", "s3cret").as_str()));
    }

    #[tokio::test]
    async fn a_refused_connect_is_a_retryable_egress_failure() {
        let upstream = spawn_http_upstream();
        for status in [
            "401 Unauthorized",
            "407 Proxy Authentication Required",
            "429 Too Many Requests",
            "502 Bad Gateway",
        ] {
            let (proxy, _) = spawn_connect_proxy(Some(status), None);
            let result = post_via(
                &format!("{upstream}/x"),
                &[],
                Bytes::from_static(b"{}"),
                Duration::from_secs(5),
                &Egress::from_proxy_url(Some(&proxy)),
            )
            .await;
        let error = expect_transport_error("a non-2xx CONNECT must fail", result);
            assert!(is_egress_failure(&error), "{error}");
            assert_eq!(error.kind(), ErrorKind::Other, "{error}");
            let code = status.split(' ').next().unwrap();
            assert!(error.to_string().contains(code), "{error}");
        }
    }

    #[tokio::test]
    async fn a_hung_connect_reply_observes_the_request_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // Accept and hold the socket open without ever answering CONNECT.
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(5));
                drop(stream);
            }
        });
        let started = Instant::now();
        let result = post_via(
            "http://127.0.0.1:9/x",
            &[],
            Bytes::from_static(b"{}"),
            Duration::from_millis(250),
            &Egress::HttpConnect { url: format!("http://{address}") },
        )
        .await;
        let error = expect_transport_error("a hung CONNECT must time out", result);
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the deadline must bound the wait, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn an_unreachable_proxy_is_an_egress_failure() {
        // A closed listener: the proxy accepts the TCP connection and closes it
        // before answering CONNECT (deterministic, unlike a privileged port the
        // host may silently drop).
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                drop(incoming);
            }
        });
        let result = post_via(
            "http://127.0.0.1:9/x",
            &[],
            Bytes::from_static(b"{}"),
            Duration::from_secs(5),
            &Egress::HttpConnect { url: format!("http://{address}") },
        )
        .await;
        let error = expect_transport_error("an unreachable proxy must fail", result);
        assert!(is_egress_failure(&error), "{error}");
    }

    // -----------------------------------------------------------------
    // SOCKS5 / SOCKS5H
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn socks5_without_auth_tunnels_to_the_upstream() {
        let upstream = spawn_http_upstream();
        let upstream_address = upstream.trim_start_matches("http://").to_string();
        let (proxy, hits) = spawn_socks5_proxy(Socks5Mode::NoAuth);
        let response = post_via(
            &format!("{upstream}/inference/v1/chat/completions"),
            &[],
            Bytes::from_static(b"{}"),
            Duration::from_secs(5),
            &Egress::from_proxy_url(Some(&format!("socks5://{proxy}"))),
        )
        .await
        .expect("the SOCKS5 tunnel must serve the request");
        assert_eq!(response.status, 200);
        assert_eq!(collect_body(response.body).await, br#"{"ok":true}"#);
        let hits = hits.lock().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].host, upstream_address.split(':').next().unwrap());
        assert_eq!(
            hits[0].port,
            upstream_address.rsplit_once(':').unwrap().1.parse::<u16>().unwrap()
        );
    }

    #[tokio::test]
    async fn socks5_with_username_password_authenticates() {
        let upstream = spawn_http_upstream();
        let (proxy, _) = spawn_socks5_proxy(Socks5Mode::Password {
            username: "alice",
            password: "s3cret",
        });
        let response = post_via(
            &format!("{upstream}/x"),
            &[],
            Bytes::from_static(b"{}"),
            Duration::from_secs(5),
            &Egress::from_proxy_url(Some(&format!("socks5://alice:s3cret@{proxy}"))),
        )
        .await
        .expect("the authenticated SOCKS5 tunnel must serve the request");
        assert_eq!(response.status, 200);
    }

    #[tokio::test]
    async fn socks5_failures_are_retryable_egress_failures() {
        let upstream = spawn_http_upstream();
        // No acceptable auth method.
        let (proxy, _) = spawn_socks5_proxy(Socks5Mode::NoAcceptableMethod);
        let result = post_via(
            &format!("{upstream}/x"),
            &[],
            Bytes::from_static(b"{}"),
            Duration::from_secs(5),
            &Egress::from_proxy_url(Some(&format!("socks5://{proxy}"))),
        )
        .await;
        let error = expect_transport_error("a refused method must fail", result);
        assert!(is_egress_failure(&error), "{error}");
        // The proxy refuses the CONNECT itself (host unreachable).
        let (proxy, _) = spawn_socks5_proxy(Socks5Mode::RefuseConnect(0x04));
        let result = post_via(
            &format!("{upstream}/x"),
            &[],
            Bytes::from_static(b"{}"),
            Duration::from_secs(5),
            &Egress::from_proxy_url(Some(&format!("socks5://{proxy}"))),
        )
        .await;
        let error = expect_transport_error("a refused CONNECT must fail", result);
        assert!(is_egress_failure(&error), "{error}");
        assert!(error.to_string().contains("host unreachable"), "{error}");
        // Wrong password.
        let (proxy, _) = spawn_socks5_proxy(Socks5Mode::Password {
            username: "alice",
            password: "right",
        });
        let result = post_via(
            &format!("{upstream}/x"),
            &[],
            Bytes::from_static(b"{}"),
            Duration::from_secs(5),
            &Egress::from_proxy_url(Some(&format!("socks5://alice:wrong@{proxy}"))),
        )
        .await;
        let error = expect_transport_error("wrong proxy credentials must fail", result);
        assert!(is_egress_failure(&error), "{error}");
        assert!(error.to_string().contains("rejected"), "{error}");
    }

    /// Scheme contract: `socks5://` hands the hostname to the proxy (ATYP 3),
    /// `socks5h://` resolves it locally and sends an IP literal (ATYP 1/4).
    #[tokio::test]
    async fn socks5_sends_the_domain_while_socks5h_resolves_locally() {
        for (scheme, expected_domain) in [("socks5", true), ("socks5h", false)] {
            let (proxy, hits) = spawn_socks5_proxy(Socks5Mode::RefuseConnect(0x04));
            // `localhost` (not 127.0.0.1) is the point: only the local-DNS
            // variant can turn it into a literal before the proxy sees it.
            let result = post_via(
                "http://localhost:9/x",
                &[],
                Bytes::from_static(b"{}"),
                Duration::from_secs(5),
                &Egress::from_proxy_url(Some(&format!("{scheme}://{proxy}"))),
            )
            .await;
        let error = expect_transport_error("the proxy refuses the CONNECT on purpose", result);
            assert!(is_egress_failure(&error), "{error}");
            let hits = hits.lock().unwrap();
            assert_eq!(hits.len(), 1, "{scheme}");
            if expected_domain {
                assert_eq!(hits[0].atyp, 0x03, "socks5 keeps the hostname");
                assert_eq!(hits[0].host, "localhost");
            } else {
                assert!(
                    hits[0].atyp == 0x01 || hits[0].atyp == 0x04,
                    "socks5h must send an IP literal, got atyp {:#04x}",
                    hits[0].atyp
                );
                assert_ne!(hits[0].host, "localhost");
            }
        }
    }

    /// An https:// proxy is recognised but this transport does not speak TLS to
    /// the proxy; the failure must be explicit rather than a silent direct dial.
    #[tokio::test]
    async fn an_https_proxy_is_reported_as_unsupported() {
        let result = post_via(
            "http://127.0.0.1:9/x",
            &[],
            Bytes::from_static(b"{}"),
            Duration::from_secs(5),
            &Egress::HttpConnect { url: "https://127.0.0.1:3128".into() },
        )
        .await;
        let error = expect_transport_error("https proxies are not supported", result);
        assert!(is_egress_failure(&error), "{error}");
        assert!(error.to_string().contains("https://"), "{error}");
    }
}
