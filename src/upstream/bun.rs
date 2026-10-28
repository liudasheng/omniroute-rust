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
//! request, no proxy support. The free lane is a chat lane (streaming SSE), so
//! connection pooling would buy little and complicate cancellation. Reads
//! happen on a blocking worker ([`tokio::task::spawn_blocking`]) and are handed
//! to the async side through a bounded channel; a 200 ms read timeout keeps the
//! worker responsive to the request deadline and to a dropped receiver, so a
//! client disconnect or a cancelled request tears the socket down instead of
//! leaking a thread.
//!
//! This module deliberately does not use `reqwest`: its HTTP/1.1 writer
//! canonicalizes and orders headers itself, which is part of the fingerprint.

use boring::ssl::{SslConnector, SslMethod, StatusType};
use bytes::Bytes;
use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::io::{self, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
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

/// POST `body` to `url` the way Bun's `fetch` does.
pub async fn post(
    url: &str,
    headers: &[(String, String)],
    body: Bytes,
    timeout: Duration,
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
    let worker = tokio::task::spawn_blocking(move || {
        run_worker(target, head, body, timeout, worker_cancel, head_tx, body_tx);
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
}

/// Blocking worker: connect, handshake, send the request, parse the head and
/// pump the body through the channel.
fn run_worker(
    target: Target,
    head: Vec<u8>,
    body: Bytes,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
    head_tx: oneshot::Sender<io::Result<RawHead>>,
    body_tx: mpsc::Sender<io::Result<Bytes>>,
) {
    let context = ReadContext { deadline: Instant::now() + timeout, cancel };
    let mut head_tx = Some(head_tx);
    let result = exchange(&target, &head, &body, &context, &mut head_tx, &body_tx);
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
    head: &[u8],
    body: &Bytes,
    context: &ReadContext,
    head_tx: &mut Option<oneshot::Sender<io::Result<RawHead>>>,
    body_tx: &mpsc::Sender<io::Result<Bytes>>,
) -> io::Result<()> {
    let mut socket = connect(target, context)?;
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

/// Open a TCP connection and, for HTTPS, complete the Bun TLS handshake.
fn connect(target: &Target, context: &ReadContext) -> io::Result<Socket> {
    let addresses = (target.host.as_str(), target.port).to_socket_addrs()?;
    let mut last_error: Option<io::Error> = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, context.handshake_budget()) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return handshake(target, stream, context);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "bun transport: no address resolved")
    }))
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
        // buffering is safe; a compressed SSE stream is not expected and is
        // refused rather than silently truncated.
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

    #[test]
    fn uncompressed_body_is_decoded_verbatim() {
        assert_eq!(decode_body("identity", b"x").unwrap(), b"x");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"hello").unwrap();
        let encoded = gz.finish().unwrap();
        assert_eq!(decode_body("gzip", &encoded).unwrap(), b"hello");
        assert!(decode_body("br", b"x").is_err());
    }
}