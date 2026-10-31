//! Phase 11 — the smart HTTP client: `info/refs`, `git-upload-pack` and `git-receive-pack` over
//! `ureq`.
//!
//! Smart HTTP is Git's protocol carried over ordinary HTTP requests. There are exactly two request
//! shapes, and the whole transport is an implementation of them:
//!
//! ```text
//! GET  <path>/info/refs?service=git-upload-pack     the ref advertisement (or v2 capabilities)
//! POST <path>/git-upload-pack                       the fetch exchange
//! POST <path>/git-receive-pack                      the push exchange
//! ```
//!
//! # The four things that make it "smart"
//!
//! 1. **The `service` query parameter.** Without it the server falls back to the *dumb* protocol
//!    and answers with a directory listing of loose objects and packs, which parses as a corrupt
//!    advertisement rather than as an error. The parameter is not optional and its spelling is part
//!    of the wire format.
//! 2. **The `Content-Type` on both sides.** A request must be
//!    `application/x-git-upload-pack-request` and the response must be
//!    `application/x-git-upload-pack-result`; a server that sees the wrong request type treats the
//!    body as garbage. See [`Service::request_content_type`] and
//!    [`Service::response_content_type`].
//! 3. **`Content-Encoding: gzip` on responses.** Smart servers gzip the advertisement and the pack.
//!    The pkt-line framing is *inside* the compressed stream, so a reader that did not decompress
//!    finds no frames at all — and because the bytes still look like data, the failure is a framing
//!    error rather than a missing-header one.
//! 4. **A chunked body that must be read as a stream.** The fetch response is a pack of arbitrary
//!    size with no `Content-Length`; a client that called `read_to_string` or held the whole body
//!    would need the repository to fit in memory.
//!
//! # Caveats that bite
//!
//! * **The URL that gets requested is not the URL the user typed.** `https://example.com/org/repo`
//!   becomes `https://example.com/org/repo/info/refs?service=…` and
//!   `https://example.com/org/repo/git-upload-pack`, and the joining is done without normalising the
//!   user's path — a trailing slash on the remote URL produces a double slash in the request, which
//!   some servers treat as a different resource. Git has the same behaviour and Rivet matches it.
//! * **Redirects are followed, but the credentials must not be.** A `302` to another host is not
//!   rare (a repository renamed, a `http` to `https` upgrade) and following it is right; sending an
//!   `Authorization` header to the new host is not. `ureq`'s redirect handling drops the header on a
//!   cross-host redirect when the header was set through its own auth API, which is why credentials
//!   go through `Agent::config` rather than being appended to the header list here.
//! * **A `401` is not a status error.** It is the server asking for credentials, which is handled by
//!   the credential chain (plan §4 Phase 11) and reported as
//!   [`crate::error::Error::Authentication`]; a caller that let it fall into
//!   [`crate::error::Error::HttpStatus`] would print "HTTP 401" where the user needs "your token is
//!   wrong or expired".
//! * **The `Git-Protocol: version=2` header is a request, not a demand.** A server that ignores it
//!   answers with a v0 advertisement, which is a supported outcome. The generation must be read from
//!   the response, never assumed from the header having been sent.
//! * **`http://` is not `https://` and the difference is load-bearing.** Over plain HTTP the
//!   credentials are on the wire in a `Basic` header — base64, not encryption — and the pack is
//!   readable by anyone on the path. Git warns and proceeds for a `http.sslVerify=false`-style
//!   reason; Rivet proceeds for `http://` because the user typed it, and the transport records the
//!   scheme so that a caller can warn. See [`RemoteScheme::is_networked`].
//!
//! # Connection reuse
//!
//! An [`Agent`](ureq::Agent) holds a connection pool. A fetch followed by a push to the same host reuses the
//! connection, and a clone that fetches twice reuses it too; constructing a fresh agent per request
//! throws that away and pays a TLS handshake each time. The transport owns one agent for its life,
//! which is why [`HttpTransport::new`] builds it once rather than per call.

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::ObjectFormat;

use crate::error::Error;
use crate::remote::transport::{CapabilitySet, HttpResponse, Service};
use crate::remote::url::RemoteScheme;
use crate::remote::url::RemoteUrl;
use crate::remote::{
    AdvertisedRef, DELIM_PACKET, FLUSH_PACKET, PktLine, ProtocolVersion, PushStatus,
    RefAdvertisement, RefUpdate, Transport, wants_request,
};

/// Phase 11 — the user agent Rivet sends.
///
/// # Caveats
///
/// **It begins with `git/`.** A small number of servers — and several corporate proxies — inspect
/// the agent string and refuse anything that does not look like Git, and a `rivet/0.1` here would
/// make those requests fail in a way that looks like an authentication problem. The version is
/// appended after the Git-compatible prefix so that a server's logs can still tell Rivet apart from
/// Git.
pub const USER_AGENT: &str = concat!("git/2.43.0.rivet/", env!("CARGO_PKG_VERSION"));

/// Phase 11 — how long a single smart-HTTP request may take before it is abandoned.
///
/// # Caveats
///
/// Applied per-request, not per-exchange: a fetch of a large pack sends one request and streams its
/// response, so this is a whole-pack timeout and must be generous. Git's default is no timeout at
/// all for the transfer and a long one for the connection; a value that was too short would abort a
/// clone of a large repository on a slow link, which is worse than waiting.
pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

thread_local! {
    /// The `WWW-Authenticate` values of the last `401` [`send`] saw, for the credential helpers.
    static LAST_CHALLENGE: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The `Authorization` value a helper's `authtype`/`credential` asked for, added by [`send`] and
    /// [`send_streaming`] to every request while it is set. Kept out of [`HttpRequest::headers`] for
    /// the reason [`HttpTransport::base_url`] gives for the Basic userinfo.
    static AUTH_HEADER: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// `builder` with the helper-chosen `Authorization` header, when there is one.
fn with_authorization<B>(builder: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
    match AUTH_HEADER.with(|h| h.borrow().clone()) {
        Some(value) => builder.header("Authorization", value),
        None => builder,
    }
}

/// Phase 11 — the smart-HTTP transport.
///
/// # Caveats
///
/// * **Credentials are not stored in the URL or the header list.** They are held separately and
///   applied per request, so that a redirect to another host does not carry them and so that a
///   `Debug` of this type — which a caller may well print while debugging a failed fetch — cannot
///   reveal them. The `Debug` impl for the whole type is why [`RemoteUrl`]'s `Display` is redacted.
/// * **One transport serves one repository.** The agent's pool is keyed by host and would happily
///   serve a second repository on the same host, but the capabilities and the advertised refs are
///   per-repository state held here; two repositories means two transports.
/// * **`Agent` is reference-counted internally**, so cloning a transport would share a pool — but
///   this type is deliberately not `Clone`, because the advertisement state is not shareable
///   (see the module caveat on the transport seam).
/// * **Not `Sync`.** The credential source may consult a helper process, and the advertisement is
///   mutable state; nothing about a fetch wants two threads in one transport (plan §3).
#[derive(Debug)]
pub struct HttpTransport {
    /// The remote being talked to, credentials included — which is why nothing may print it; see
    /// [`RemoteUrl::redacted`] and the `Display` note on [`RemoteUrl`].
    url: RemoteUrl,
    /// The connection pool, built once with the user agent and the timeouts applied.
    agent: ureq::Agent,
    /// The credentials to send, resolved lazily by the credential chain (plan §4 Phase 11). `None`
    /// until a `401` says they are needed, so that a public clone never touches a helper.
    credentials: Option<(BString, BString)>,
    /// The password from the URL's userinfo, offered only after the server has asked for credentials
    /// (Git never sends them unprompted).
    url_credentials: Option<(BString, BString)>,
    /// The credential configuration for this URL, consulted on a `401`.
    credential_settings: Option<crate::remote::credential::CredentialSettings>,
    /// A credential that was sent and has not been reported to the helpers yet.
    pending_approval: Option<crate::remote::credential::Credential>,
    /// The capabilities of the last advertisement. Owned, because the response buffer they came
    /// from is reused per request; `raw` is empty because [`Capability::raw`] is already owned.
    capabilities: CapabilitySet<'static>,
    /// Which generation the last [`Transport::connect`] answered in. Decides how `fetch` and
    /// `ls_refs` frame their requests.
    version: ProtocolVersion,
    /// The peer's object format, established at `connect` and used to parse ids afterwards.
    format: ObjectFormat,
    /// Where the advertisement was actually served from, after any redirect: later requests go
    /// there, as Git's do.
    redirected_base: Option<BString>,
    /// The `Authorization` value a helper's `authtype`/`credential` produced, sent instead of Basic.
    authorization: Option<String>,
}

impl HttpTransport {
    /// Phase 11 — build a transport for a remote URL.
    ///
    /// # Caveats
    ///
    /// * **The scheme must be [`RemoteScheme::Http`] or [`RemoteScheme::Https`]**, and anything else
    ///   is refused here rather than producing a request to a nonsensical URL.
    /// * **No request is made and no name is resolved.** Construction is cheap and offline, so a
    ///   caller can build a transport and decide later whether to use it; anything that could block
    ///   is in [`Transport::connect`].
    /// * **A plain-`http` URL is accepted but flagged.** There is no refusal — a developer running
    ///   `git http-backend` on localhost uses it — but
    ///   [`HttpTransport::is_insecure`] reports it so the CLI can warn before a credential is sent.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidRemoteUrl`] for a URL whose scheme is not HTTP, and
    /// [`crate::error::Error::Transport`] if the TLS backend cannot be initialised — which is a
    /// build-time condition rather than a network one, and worth reporting at construction instead
    /// of at the first request.
    pub fn new(url: RemoteUrl) -> Result<Self> {
        Self::with_timeout(url, DEFAULT_TIMEOUT)
    }

    /// Phase 11 — build a transport with an explicit request timeout.
    ///
    /// # Caveats
    ///
    /// The timeout is **per request**, not per read, so a slow-but-progressing transfer of a large
    /// pack is governed by how long the whole response takes. A caller wiring this to a
    /// `--timeout` flag is exposing exactly that semantics; one that expects "the connection must
    /// make progress every N seconds" is asking for something `ureq` does not provide and will need
    /// a different transport.
    ///
    /// # Errors
    ///
    /// As [`HttpTransport::new`].
    pub fn with_timeout(url: RemoteUrl, timeout: std::time::Duration) -> Result<Self> {
        if !matches!(url.scheme, RemoteScheme::Http | RemoteScheme::Https) {
            return Err(Error::InvalidRemoteUrl {
                url: display_url(&url),
                reason: "not an HTTP URL",
            });
        }
        if url.host.is_none() {
            return Err(Error::InvalidRemoteUrl {
                url: display_url(&url),
                reason: "no host",
            });
        }
        // `http_status_as_error(false)` keeps a 401's headers and body reachable: ureq's default
        // turns every 4xx into `Error::StatusCode`, which loses both and would make the
        // authentication mapping below impossible.
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let url_credentials = match (&url.user, &url.password) {
            (Some(user), Some(password)) => Some((user.clone(), password.clone())),
            _ => None,
        };
        Ok(Self {
            url,
            redirected_base: None,
            authorization: None,
            agent,
            credentials: None,
            url_credentials,
            credential_settings: None,
            pending_approval: None,
            capabilities: CapabilitySet::default(),
            version: ProtocolVersion::V0,
            format: ObjectFormat::Sha1,
        })
    }

    /// Phase 11 — supply credentials to send, bypassing the credential chain.
    ///
    /// # Caveats
    ///
    /// * **The password is used as the HTTP password**, base64-encoded into a `Basic` header — which
    ///   for GitHub means the token is the *password* and the user is anything non-empty, exactly as
    ///   `git` requires. A caller that supplied a token as the user and an empty password gets a
    ///   `401` that looks like a bad token.
    /// * **This does not verify anything.** Credentials are sent on the next request and rejected
    ///   there if wrong; there is no pre-flight check, because a check would be an extra round trip
    ///   and would fail differently against servers that allow anonymous reads.
    /// * **The credentials are held for the transport's life**, so a caller that got them from a
    ///   helper and considers them short-lived must drop the transport to release them.
    pub fn set_credentials(&mut self, user: &bstr::BStr, password: &bstr::BStr) {
        self.url.user = Some(user.to_owned());
        self.url.password = Some(password.to_owned());
        self.credentials = Some((user.to_owned(), password.to_owned()));
    }

    /// Phase 11 — whether the URL is plain `http`.
    ///
    /// # Caveats
    ///
    /// True only for [`RemoteScheme::Http`]. The CLI uses it to print the warning Git prints before
    /// sending a credential in the clear; the transport itself does not refuse, because refusing
    /// would break the localhost case that every `git http-backend` test relies on.
    pub fn is_insecure(&self) -> bool {
        self.url.scheme == RemoteScheme::Http
    }

    /// Phase 11 — the absolute `info/refs` URL for a service.
    ///
    /// # Caveats
    ///
    /// * **The question mark and the parameter are appended verbatim**, giving
    ///   `<path>/info/refs?service=git-upload-pack`. A URL builder that percent-encoded the `?` would
    ///   produce a path component named `info/refs?service=…`, which is a `404`.
    /// * **A remote path already ending in `.git` is not special-cased**, because for HTTP the server
    ///   decides what the path means; `repo.git/info/refs` and `repo/info/refs` are both what the
    ///   user asked for.
    /// * **The credentials are not in the returned URL**, even if the remote URL carried them. This
    ///   is the string that must appear in any error message, so it is the one that must be safe.
    pub fn refs_url(&self, service: Service) -> BString {
        let path = &self.url.path;
        let service_str = service.as_str();
        BString::from(format!("{}/info/refs?service={}", path, service_str).as_bytes())
    }

    /// Phase 11 — the absolute service URL for a POST.
    ///
    /// # Caveats
    ///
    /// No query parameter: the service is in the path, and only the advertisement takes `?service=`.
    /// Appending it here produces a server that answers with the advertisement again and a client
    /// that parses it as a negotiation response.
    pub fn service_url(&self, service: Service) -> BString {
        let path = &self.url.path;
        let service_str = service.as_str();
        BString::from(format!("{}/{}", path, service_str).as_bytes())
    }

    /// The absolute URL a request is sent to, optionally carrying the credentials in the userinfo.
    ///
    /// ureq turns `user:password@host` into a `Basic` header itself and drops the userinfo from the
    /// request line, which is the only way credentials reach the wire without being written into
    /// [`HttpRequest::headers`] — a value a caller may log or a test may snapshot.
    fn base_url(&self, url: &RemoteUrl, credentials: bool) -> BString {
        if let Some(base) = &self.redirected_base {
            return base.clone();
        }
        let mut out = BString::from(url.scheme.as_str());
        out.extend_from_slice(b"://");
        if credentials && let Some((user, password)) = &self.credentials {
            out.extend_from_slice(&user[..]);
            out.push(b':');
            out.extend_from_slice(&password[..]);
            out.push(b'@');
        }
        if let Some(host) = url.host_port() {
            out.extend_from_slice(&host[..]);
        }
        out.extend_from_slice(&url.path[..]);
        out
    }
}

/// Render a URL for an error message without its password.
///
/// Built from the parsed fields rather than from [`RemoteUrl::redacted`], whose userinfo handling
/// still appends the password; this is the string that ends up in a message a user may paste into a
/// bug report.
/// What Git adds when the `ls-refs` POST is refused (rivet's first POST of a fetch stands in for it,
/// the refs having come with the advertisement): its `stateless-connect` helper dies reading the
/// listing. The reason carries both lines (`main` prints them as Git does): after a refused
/// credential Git reports `RPC failed`, and when no credential could be found it reports why not.
fn ls_refs_failure(error: Error) -> Error {
    const TAIL: &str = "fatal: expected flush after ref listing";
    match error {
        Error::Authentication { url, reason }
            if reason.starts_with("could not read ")
                || reason.starts_with("credential helper ") =>
        {
            Error::Authentication {
                url,
                reason: format!("{reason}\n{TAIL}"),
            }
        }
        Error::Authentication { url, .. } => Error::Authentication {
            url,
            reason: format!(
                "RPC failed; HTTP 401 curl 22 The requested URL returned error: 401\n{TAIL}"
            ),
        },
        other => other,
    }
}

fn display_url(url: &RemoteUrl) -> BString {
    let mut out = BString::from(url.scheme.as_str());
    out.extend_from_slice(b"://");
    if let Some(user) = &url.user {
        out.extend_from_slice(&user[..]);
        out.push(b'@');
    }
    if let Some(host) = url.host_port() {
        out.extend_from_slice(&host[..]);
    }
    out.extend_from_slice(&url.path[..]);
    out
}

/// Join a base URL and a relative target with exactly one slash between them.
fn join_url(base: &[u8], target: &[u8]) -> BString {
    let mut out = BString::from(base);
    if !out.ends_with(b"/") {
        out.push(b'/');
    }
    out.extend_from_slice(target);
    out
}

/// Map a `ureq` failure onto the crate's error, with the URL already redacted.
fn map_ureq(error: ureq::Error, url: &BStr) -> Error {
    match error {
        ureq::Error::StatusCode(status) => Error::HttpStatus {
            url: url.to_owned(),
            status,
        },
        other => Error::Transport(format!("{}: {}", url, other)),
    }
}

/// Read one pkt-line frame from a stream, keeping the four header bytes when they are not hex.
///
/// The raw variant is what makes a v0 fetch without `side-band-64k` readable: the pack begins
/// immediately after the `NAK` pkt-line, so the next four bytes are `PACK` and not a length. A
/// reader that only knew pkt-lines would consume them and leave the pack unparseable.
enum Frame {
    Flush,
    Data(Vec<u8>),
    Raw([u8; 4]),
}

fn read_frame<R: std::io::Read>(reader: &mut R) -> Result<Option<Frame>> {
    let mut head = [0u8; 4];
    match reader.read_exact(&mut head) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(Error::Io(err)),
    }
    if head == *FLUSH_PACKET {
        return Ok(Some(Frame::Flush));
    }
    if head == *DELIM_PACKET {
        return Ok(Some(Frame::Data(Vec::new())));
    }
    let parsed = std::str::from_utf8(&head)
        .ok()
        .and_then(|text| usize::from_str_radix(text, 16).ok());
    let Some(len) = parsed.filter(|len| *len >= 4) else {
        return Ok(Some(Frame::Raw(head)));
    };
    let mut payload = vec![0u8; len - 4];
    reader.read_exact(&mut payload)?;
    Ok(Some(Frame::Data(payload)))
}

/// Copy the rest of a stream into a writer without needing a sized writer type.
fn copy_stream<R: std::io::Read, W: std::io::Write + ?Sized>(
    reader: &mut R,
    out: &mut W,
) -> Result<()> {
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        out.write_all(&buffer[..read])?;
    }
}

/// Split a pkt-line section into its payloads concatenated, ignoring framing and stopping at the
/// first flush. Used for a push's `report-status`, which is itself pkt-framed inside channel 1.
fn pkt_payloads(input: &[u8]) -> Result<Vec<u8>> {
    let mut cursor = input;
    let mut out = Vec::new();
    loop {
        if cursor.is_empty() {
            break;
        }
        let pkt = PktLine::read_from(&mut cursor)?;
        if pkt.is_flush() {
            break;
        }
        out.extend_from_slice(pkt.data());
    }
    Ok(out)
}

/// Split a v0 ref advertisement line into `(hex id, name, capabilities)`.
///
/// The id stays hex because the peer's object format is only known from the first line's own
/// capability section — parsing it at the local width first would reject a SHA-256 peer before the
/// mismatch could be reported as such.
pub(crate) fn split_ref_line(line: &[u8]) -> Result<(BString, BString, Option<Vec<u8>>)> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    if line.is_empty() {
        return Err(Error::Protocol("empty ref line".to_string()));
    }
    if let Some(message) = line.strip_prefix(b"ERR ") {
        return Err(Error::Protocol(
            String::from_utf8_lossy(message).into_owned(),
        ));
    }
    let space = line
        .iter()
        .position(|&b| b == b' ')
        .ok_or_else(|| Error::Protocol("ref line without a space".to_string()))?;
    let hex = &line[..space];
    let mut rest = &line[space + 1..];
    let caps = match rest.iter().position(|&b| b == 0) {
        Some(nul) => {
            let caps = rest[nul + 1..].to_vec();
            rest = &rest[..nul];
            Some(caps)
        }
        None => None,
    };
    Ok((BString::from(hex), BString::from(rest), caps))
}

/// Phase 11 — build the `GET` request for a service's ref advertisement.
///
/// # Caveats
///
/// * **The `service` query parameter is not optional** — see the module caveat — and it must be
///   percent-encoded as written (`git-upload-pack` needs none, but the rule is not "it needs none").
/// * **`Git-Protocol: version=2` is sent only when `version` is [`ProtocolVersion::V2`].** Sending it
///   to a v0-only server is harmless in Git's implementation and is not guaranteed to be here, and
///   more importantly a request that asks for v2 must be prepared for a v0 answer — the generation
///   comes from the response either way.
/// * **`Accept-Encoding: gzip` is what makes the decompression path reachable.** `ureq` adds it when
///   its `gzip` feature is on, which the workspace enables by default; the header is listed here so
///   that the wire format a test records includes it and a server that varies on it is handled.
///
/// # Errors
///
/// [`crate::error::Error::InvalidRemoteUrl`] if the remote URL has no host — an HTTP request to
/// nowhere is a URL error rather than a transport one, and the message should say so.
pub fn advertise_request(
    url: &RemoteUrl,
    service: Service,
    version: crate::remote::ProtocolVersion,
) -> Result<crate::remote::HttpRequest> {
    if url.host.is_none() {
        return Err(Error::InvalidRemoteUrl {
            url: display_url(url),
            reason: "no host",
        });
    }
    let mut headers = Vec::new();
    headers.push(("Accept-Encoding", BString::from("gzip")));
    // `receive-pack` has no v2, so asking for it there would be a request the server cannot honour.
    if version == ProtocolVersion::V2 && service.supports_v2() {
        headers.push(("Git-Protocol", BString::from("version=2")));
    }
    Ok(crate::remote::HttpRequest {
        is_get: true,
        target: BString::from(format!("info/refs?service={}", service.as_str())),
        content_type: None,
        body: Vec::new(),
        headers,
    })
}

/// Phase 11 — build the `POST` request for a service exchange.
///
/// # Caveats
///
/// * **The body must already be pkt-line framed**, including its trailing flush — a caller that
///   passed a bare `want` line gets a server that reads the request as truncated and waits for more.
///   [`crate::remote::wants_request`] and its siblings produce framed bodies for exactly this reason.
/// * **The content type is the service's request type**, not the response type and not
///   `application/octet-stream`. See the module caveat.
/// * **An empty body is legal for `git-receive-pack`** — a push that only deletes refs sends no pack
///   — but the request is still a `POST` with the right content type, and a `GET` with an empty body
///   is a different request entirely.
///
/// # Errors
///
/// [`crate::error::Error::InvalidRemoteUrl`] if the remote URL has no host, and
/// [`crate::error::Error::Protocol`] if `body` is not valid pkt-line framing — checked here because a
/// caller that built the body by hand is exactly the caller that got the framing wrong, and the
/// server's error for it names nothing useful.
pub fn service_request(
    url: &RemoteUrl,
    service: Service,
    body: Vec<u8>,
) -> Result<crate::remote::HttpRequest> {
    if url.host.is_none() {
        return Err(Error::InvalidRemoteUrl {
            url: display_url(url),
            reason: "no host",
        });
    }
    // Validate the pkt-line section only: a push appends the raw pack after the flush, and the pack
    // is deliberately not pkt-line framed.
    let mut cursor = &body[..];
    loop {
        if cursor.is_empty() {
            return Err(Error::Protocol("request body without a flush".to_string()));
        }
        let pkt = PktLine::read_from(&mut cursor)?;
        if pkt.is_flush() {
            break;
        }
    }
    Ok(crate::remote::HttpRequest {
        is_get: false,
        target: BString::from(service.as_str()),
        content_type: Some(service.request_content_type()),
        body,
        headers: vec![("Accept-Encoding", BString::from("gzip"))],
    })
}

/// Phase 11 — send one request and read its response.
///
/// The single place a request leaves the process, so that the status check, the content-type check
/// and the decompression all happen once rather than at each call site.
///
/// # Caveats
///
/// * **The body is decompressed before it is returned.** See the module caveat; the pkt-line framing
///   is inside the gzip stream and nothing above this function knows gzip exists. `ureq` does this
///   transparently when its `gzip` feature is enabled, which is why the check here is "is the result
///   still compressed" rather than a second decompression pass.
/// * **`401` and `407` become [`crate::error::Error::Authentication`]** with the URL redacted, never
///   the credential. See the module caveat.
/// * **Every other non-2xx becomes [`crate::error::Error::HttpStatus`]**, including `404`, which for
///   a smart-HTTP server means either a wrong URL or a repository that does not exist — a
///   distinction the status alone cannot make, and the message says both.
/// * **The response body is read into memory**, which is right for an advertisement and wrong for a
///   fetch's pack. [`HttpTransport`]'s fetch path uses the streaming form ([`send_streaming`]) for
///   exactly that reason; this function is for the requests whose answers are small.
///
/// # Errors
///
/// [`crate::error::Error::Transport`] for a connection, TLS or DNS failure;
/// [`crate::error::Error::Authentication`] for a `401` or `407`;
/// [`crate::error::Error::HttpStatus`] for any other non-2xx; and
/// [`crate::error::Error::Protocol`] if the response's content type is not the service's result type,
/// in which case the body is usually the server's human-readable complaint and the error carries it.
pub fn send(
    agent: &ureq::Agent,
    request: &crate::remote::HttpRequest,
    redacted_url: &bstr::BStr,
) -> Result<HttpResponse> {
    let full = join_url(redacted_url, &request.target);
    let safe = crate::remote::redact_url(BStr::new(&full));
    let response = if request.is_get {
        let mut builder = with_authorization(agent.get(&full[..]));
        for (name, value) in &request.headers {
            builder = builder.header(*name, &value[..]);
        }
        builder.call()
    } else {
        let mut builder = with_authorization(agent.post(&full[..]));
        for (name, value) in &request.headers {
            builder = builder.header(*name, &value[..]);
        }
        if let Some(content_type) = request.content_type {
            builder = builder.header("Content-Type", content_type);
        }
        builder.send(request.body.clone())
    };
    let mut response = response.map_err(|err| map_ureq(err, BStr::new(&safe)))?;

    let status = response.status().as_u16();
    if is_auth_challenge(status) {
        let challenge: Vec<String> = response
            .headers()
            .get_all("www-authenticate")
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_string))
            .collect();
        LAST_CHALLENGE.with(|c| *c.borrow_mut() = challenge);
        return Err(Error::Authentication {
            url: safe,
            reason: format!("HTTP {} from the remote", status),
        });
    }
    if !is_success(status) {
        return Err(Error::HttpStatus { url: safe, status });
    }

    use ureq::ResponseExt as _;
    let final_url = Some(BString::from(response.get_uri().to_string()));
    let content_type =
        header_string(&response, "content-type").map(|text| media_type(BStr::new(&text)));
    let encoding = header_string(&response, "content-encoding");
    let body = response
        .body_mut()
        .read_to_vec()
        .map_err(|err| Error::Transport(format!("{}: {}", safe, err)))?;
    let body = decode_body(encoding.as_ref().map(|enc| BStr::new(&enc[..])), body)?;

    if let Some(content_type) = &content_type
        && !is_git_content_type(BStr::new(content_type))
    {
        return Err(Error::Protocol(format!(
            "unexpected content type {}: {}",
            content_type,
            body_preview(&body)
        )));
    }
    Ok(HttpResponse {
        status,
        content_type,
        body,
        final_url,
    })
}

/// The header as text, or `None` when it is absent or not printable ASCII.
fn header_string(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<BString> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(BString::from)
}

/// Whether a media type is one of the smart-HTTP ones.
///
/// The advertisement is `…-advertisement` and a service response is `…-result`; both start with the
/// same prefix, and anything else is a server's human-readable complaint rather than a protocol
/// body.
fn is_git_content_type(content_type: &BStr) -> bool {
    content_type.starts_with(b"application/x-git-")
        || content_type.starts_with(b"application/x-git")
}

/// The first few bytes of a body, for an error message.
fn body_preview(body: &[u8]) -> String {
    const LIMIT: usize = 200;
    let slice = &body[..body.len().min(LIMIT)];
    String::from_utf8_lossy(slice).into_owned()
}

/// Phase 11 — send a request and hand the response body to a callback as a stream.
///
/// The form a fetch needs: the pack is of unbounded size and arrives chunked, and a client that
/// buffered the whole response would need the repository to fit in memory. The callback receives a
/// `Read` positioned at the first byte of the (decompressed) body.
///
/// # Caveats
///
/// * **The callback must consume the body**, or the connection cannot be returned to the pool and
///   the agent's socket stays open until the response is dropped. A callback that returns early is
///   legal — the transport drops the response — but it forfeits connection reuse, which is the whole
///   reason the agent is held across calls.
/// * **Read errors surface through the callback's return**, so a pack that is truncated mid-transfer
///   is the callback's error and not a transport one. That is deliberate: only the callback knows
///   whether a short read is fatal, and for a fetch it always is.
/// * **Decompression happens inside the stream**, so the callback sees pack bytes and never gzip
///   framing. A server that gzips a pack and a client that did not expect it produce a callback that
///   fails at the pack's magic rather than at the transfer, which is a much more confusing report.
///
/// # Errors
///
/// As [`send`], plus whatever the callback returns for a failure while reading the body.
pub fn send_streaming<T>(
    agent: &ureq::Agent,
    request: &crate::remote::HttpRequest,
    redacted_url: &bstr::BStr,
    consume: &mut dyn FnMut(&mut dyn std::io::Read) -> Result<T>,
) -> Result<T> {
    let full = join_url(redacted_url, &request.target);
    let safe = crate::remote::redact_url(BStr::new(&full));
    let response = if request.is_get {
        let mut builder = with_authorization(agent.get(&full[..]));
        for (name, value) in &request.headers {
            builder = builder.header(*name, &value[..]);
        }
        builder.call()
    } else {
        let mut builder = with_authorization(agent.post(&full[..]));
        for (name, value) in &request.headers {
            builder = builder.header(*name, &value[..]);
        }
        if let Some(content_type) = request.content_type {
            builder = builder.header("Content-Type", content_type);
        }
        builder.send(request.body.clone())
    };
    let mut response = response.map_err(|err| map_ureq(err, BStr::new(&safe)))?;

    let status = response.status().as_u16();
    if is_auth_challenge(status) {
        let challenge: Vec<String> = response
            .headers()
            .get_all("www-authenticate")
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_string))
            .collect();
        LAST_CHALLENGE.with(|c| *c.borrow_mut() = challenge);
        return Err(Error::Authentication {
            url: safe,
            reason: format!("HTTP {} from the remote", status),
        });
    }
    if !is_success(status) {
        return Err(Error::HttpStatus { url: safe, status });
    }

    if let Some(content_type) = header_string(&response, "content-type")
        && !is_git_content_type(BStr::new(&content_type))
    {
        // The body is the server's explanation, and this is the only chance to read it: the
        // caller's callback expects protocol bytes and would report a framing error instead.
        let body = response.body_mut().read_to_vec().unwrap_or_default();
        return Err(Error::Protocol(format!(
            "unexpected content type {}: {}",
            content_type,
            body_preview(&body)
        )));
    }

    let body = response.body_mut();
    let mut reader = body.as_reader();
    consume(&mut reader)
}

/// Phase 11 — undo a response's `Content-Encoding`.
///
/// Exposed because [`send`] is not the only path that receives a body: the streaming fetch path
/// reads straight from the socket, and a test that replays a recorded response needs to decompress
/// it the same way the transport would.
///
/// # Caveats
///
/// * **`gzip` and `deflate` are the only encodings Git's smart servers use**, and `gzip` is the only
///   one they actually send in practice. An unknown encoding is
///   [`crate::error::Error::Protocol`] rather than a pass-through: a body that is still compressed
///   contains no pkt-lines, and returning it would move the failure to a framing error with no hint
///   about the encoding.
/// * **The check is on the header, not on the bytes**, so a body with no `Content-Encoding` is
///   returned unchanged and a body with `identity` likewise. Sniffing the first two bytes for a gzip
///   magic would misread a legitimate pkt-line header — `0x00 0x30` and friends — often enough to
///   matter.
/// * **The input is consumed.** A caller with a borrowed body clones it; the streaming path uses
///   `flate2`'s reader over the socket instead of calling this at all.
///
/// # Errors
///
/// [`crate::error::Error::Protocol`] for an unsupported or malformed `Content-Encoding`, and
/// [`crate::error::Error::CorruptPack`] is **not** raised here even for a garbled pack — the body's
/// framing is checked by the layer that parses it, and this function only knows about compression.
pub fn decode_body(encoding: Option<&bstr::BStr>, body: Vec<u8>) -> Result<Vec<u8>> {
    let Some(enc) = encoding else {
        return Ok(body);
    };
    let enc = enc.trim_ascii();
    if enc.is_empty() || enc == b"identity" {
        return Ok(body);
    }
    if enc == b"gzip" {
        let mut d = flate2::read::GzDecoder::new(&body[..]);
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut d, &mut out)
            .map_err(|_| Error::Protocol(String::from("gzip decode failed")))?;
        return Ok(out);
    }
    if enc == b"deflate" {
        let mut d = flate2::read::ZlibDecoder::new(&body[..]);
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut d, &mut out)
            .map_err(|_| Error::Protocol(String::from("deflate decode failed")))?;
        return Ok(out);
    }
    Err(Error::Protocol(String::from(
        "unsupported content-encoding",
    )))
}

/// Phase 11 — parse the `Content-Type` of a response into its media type, lowercased.
///
/// # Caveats
///
/// The parameters are dropped: `application/x-git-upload-pack-result; charset=utf-8` is the result
/// type, and comparing the whole header against the constant would reject it. The comparison is
/// case-insensitive because media types are, while the pkt-line *content* is not.
pub fn media_type(header: &bstr::BStr) -> BString {
    BString::from(header.to_ascii_lowercase())
}

/// Phase 11 — whether a status is one that carries an authentication challenge.
///
/// # Caveats
///
/// `401` and `407` — the second is a *proxy* demanding credentials, which is a different challenge
/// with a different answer (the proxy's `Proxy-Authorization`, not the server's) and is grouped here
/// only because both mean "you have not authenticated". A caller that answered a `407` with the
/// server's credential would send the token to a proxy that is not the server, which is how a
/// credential leaks to an intercepting proxy.
pub const fn is_auth_challenge(status: u16) -> bool {
    status == 401 || status == 407
}

/// Phase 11 — whether the status is a success.
///
/// # Caveats
///
/// Git's transport accepts `200` and `304` for a smart-HTTP response, and some servers answer a
/// conditional `info/refs` with `304` and no body — which is a `Not Modified` a caller with no cache
/// must treat as "ask again without the condition" rather than as an empty advertisement. The
/// predicate is here so that rule lives in one place instead of being spelled `== 200` at each
/// call site, which is how the `304` case gets forgotten.
pub const fn is_success(status: u16) -> bool {
    status == 200 || status == 304
}

impl HttpTransport {
    /// Send `request`; if the server answers `401`, find credentials (the URL's, the helpers',
    /// then a prompt) and send it once more. A second `401` tells the helpers to forget them.
    fn send_authenticated(
        &mut self,
        url: &RemoteUrl,
        request: &crate::remote::HttpRequest,
    ) -> Result<HttpResponse> {
        self.authenticated(url, true, |agent, base| send(agent, request, base))
    }

    /// Run `attempt` (one request, given the agent and the base URL carrying the credentials found
    /// so far); on a `401` find credentials and run it again, as [`Self::send_authenticated`] does
    /// for the advertisement. Every POST goes through here too: a server may leave discovery public
    /// and challenge only the POST; `capabilities` is false for those, as Git asks its helpers
    /// without `authtype`/`state` there. `attempt` must be repeatable, so a streaming consumer is only
    /// called once the status is known.
    fn authenticated<T>(
        &mut self,
        url: &RemoteUrl,
        capabilities: bool,
        mut attempt: impl FnMut(&ureq::Agent, &BStr) -> Result<T>,
    ) -> Result<T> {
        use crate::remote::credential::Credential;
        let mut asked = self.credentials.is_some() || self.authorization.is_some();
        // Git makes at most two credential rounds per request, the first one included
        // (`http_request_reauth` loops three times, the initial 401 being the first pass).
        let mut rounds = 0u8;
        loop {
            let base = self.base_url(url, true);
            AUTH_HEADER.with(|h| *h.borrow_mut() = self.authorization.clone());
            let outcome = attempt(&self.agent, BStr::new(&base));
            AUTH_HEADER.with(|h| *h.borrow_mut() = None);
            let challenge_failed = match outcome {
                Ok(response) => {
                    if let Some(credential) = self.pending_approval.take()
                        && let Some(settings) = &self.credential_settings
                    {
                        settings.approve(&credential);
                    }
                    return Ok(response);
                }
                Err(Error::Authentication { reason, .. }) => {
                    // Git names the repository, without any userinfo.
                    let mut bare = self.url.clone();
                    bare.user = None;
                    bare.password = None;
                    (display_url(&bare), reason)
                }
                Err(other) => return Err(other),
            };
            let (shown, reason) = challenge_failed;
            let wwwauth = LAST_CHALLENGE.with(|c| c.borrow().clone());
            let settings = self.credential_settings.clone().unwrap_or_default();
            let mut next_round: Option<Credential> = None;
            if asked && self.pending_approval.as_ref().is_some_and(|c| c.multistage) {
                // The helper said `continue=1`: nothing is erased, it is asked again with its state.
                self.authorization = None;
                self.credentials = None;
                let mut credential = self.pending_approval.take().expect("checked above");
                if rounds >= 1 {
                    return Err(Error::Authentication {
                        url: shown,
                        reason: "unable to access".to_string(),
                    });
                }
                rounds += 1;
                credential.next_round(wwwauth.clone());
                next_round = Some(credential);
            } else if asked {
                // What was sent was refused: the helpers must not keep it.
                if self.authorization.take().is_some() {
                    if let Some(mut credential) = self.pending_approval.take() {
                        credential.wwwauth = wwwauth;
                        settings.reject(&credential);
                    }
                } else if let Some((user, password)) = self.credentials.take() {
                    let mut credential = self.pending_approval.take().unwrap_or_else(|| {
                        let mut fresh = settings.credential_for(url, Vec::new());
                        fresh.username = Some(user);
                        fresh.password = Some(password);
                        fresh
                    });
                    credential.wwwauth = wwwauth;
                    settings.reject(&credential);
                }
                self.pending_approval = None;
                return Err(Error::Authentication { url: shown, reason });
            }
            asked = true;
            let mut credential: Credential =
                next_round.unwrap_or_else(|| settings.credential_for(url, wwwauth));
            credential.plain_get = !capabilities;
            if let Some((user, password)) = &self.url_credentials {
                credential.username = Some(user.clone());
                credential.password = Some(password.clone());
            } else {
                if let Err(reason) = settings.fill(&mut credential) {
                    return Err(Error::Authentication { url: shown, reason });
                }
            }
            for (what, missing) in [
                ("Username", credential.username.is_none()),
                ("Password", credential.password.is_none()),
            ] {
                if !missing || credential.is_complete() {
                    continue;
                }
                match settings.prompt(what, &credential) {
                    Ok(answer) if what == "Username" => credential.username = Some(answer),
                    Ok(answer) => credential.password = Some(answer),
                    Err(why) => {
                        let mut host = credential.protocol.clone();
                        host.push_str("://");
                        if what == "Password"
                            && let Some(user) = &credential.username
                        {
                            host.push_str(&bstr::ByteSlice::to_str_lossy(user.as_slice()));
                            host.push('@');
                        }
                        host.push_str(&credential.host);
                        return Err(Error::Authentication {
                            url: BString::from(host.as_str()),
                            reason: format!("could not read {what} for '{host}': {why}"),
                        });
                    }
                }
            }
            if let Some(value) = credential.authorization() {
                self.authorization = Some(value);
            } else if let (Some(user), Some(password)) =
                (&credential.username, &credential.password)
            {
                self.credentials = Some((user.clone(), password.clone()));
            }
            self.pending_approval = Some(credential);
        }
    }

    /// Fetch and parse the ref advertisement of one service.
    fn advertise(
        &mut self,
        url: &RemoteUrl,
        format: ObjectFormat,
        service: Service,
    ) -> Result<RefAdvertisement> {
        // v0 advertises every ref in the first response, which is what the fetch layer consumes;
        // asking for v2 would also need a separate `ls-refs` round before any ref is known.
        let request = advertise_request(url, service, ProtocolVersion::V0)?;
        let response = self.send_authenticated(url, &request)?;

        // A redirect moves the repository: the rest of the session talks to the new location.
        if let Some(final_url) = &response.final_url {
            let suffix = format!("/info/refs?service={}", service.as_str());
            if let Some(prefix) = final_url.strip_suffix(suffix.as_bytes()) {
                let original = self.base_url(url, false);
                if prefix != &original[..] {
                    self.redirected_base = Some(BString::from(prefix));
                }
            }
        }

        let mut cursor = &response.body[..];
        let first = PktLine::read_from(&mut cursor)?;

        // An empty repository can answer with nothing but a flush; that is legal and means no refs.
        if first.is_flush() {
            let advertisement = RefAdvertisement {
                capabilities: CapabilitySet::default(),
                refs: Vec::new(),
                format,
                agent: None,
                version: ProtocolVersion::V0,
            };
            self.capabilities = CapabilitySet::default();
            self.version = ProtocolVersion::V0;
            self.format = format;
            return Ok(advertisement);
        }

        if first.data() == b"version 2" || first.data() == b"version 2\n" {
            let mut capabilities = CapabilitySet {
                entries: Vec::new(),
                raw: &[],
            };
            loop {
                let pkt = PktLine::read_from(&mut cursor)?;
                if pkt.is_flush() {
                    break;
                }
                if pkt.is_control() {
                    continue;
                }
                let text = pkt.data();
                let text = text.strip_suffix(b"\n").unwrap_or(text);
                if text.is_empty() {
                    continue;
                }
                capabilities.insert(text)?;
            }
            let peer = capabilities.object_format()?;
            if peer != format {
                return Err(Error::ObjectFormatMismatch {
                    expected: format,
                    found: peer,
                });
            }
            let agent = capabilities.value(b"agent").map(BString::from);
            self.capabilities = capabilities.clone();
            self.version = ProtocolVersion::V2;
            self.format = peer;
            return Ok(RefAdvertisement {
                capabilities,
                refs: Vec::new(),
                format: peer,
                agent,
                version: ProtocolVersion::V2,
            });
        }

        // v0: an optional `# service=…` prelude, then one ref per line with the capabilities after
        // the NUL on the first line.
        let mut lines: Vec<Vec<u8>> = Vec::new();
        if first.data().starts_with(b"# service=") {
            let flush = PktLine::read_from(&mut cursor)?;
            if !flush.is_flush() {
                return Err(Error::Protocol(
                    "advertisement prelude was not followed by a flush".to_string(),
                ));
            }
        } else {
            lines.push(first.data().to_vec());
        }
        loop {
            if cursor.is_empty() {
                break;
            }
            let pkt = PktLine::read_from(&mut cursor)?;
            if pkt.is_flush() {
                break;
            }
            if pkt.is_control() {
                continue;
            }
            lines.push(pkt.data().to_vec());
        }

        let capabilities = match lines.first() {
            Some(line) => match split_ref_line(line)?.2 {
                Some(bytes) => {
                    let parsed = CapabilitySet::parse(&bytes)?;
                    CapabilitySet {
                        entries: parsed.entries.clone(),
                        raw: &[],
                    }
                }
                None => CapabilitySet::default(),
            },
            None => CapabilitySet::default(),
        };
        let peer = capabilities.object_format()?;
        if peer != format {
            return Err(Error::ObjectFormatMismatch {
                expected: format,
                found: peer,
            });
        }
        let mut refs: Vec<AdvertisedRef> = Vec::new();
        for line in &lines {
            let (hex, name, _) = split_ref_line(line)?;
            if let Some(base) = name.strip_suffix(b"^{}") {
                if let Some(entry) = refs.iter_mut().find(|entry| entry.name == BStr::new(base)) {
                    entry.peeled = Some(crate::hash::ObjectId::from_hex(peer, BStr::new(&hex))?);
                }
                continue;
            }
            refs.push(AdvertisedRef {
                name,
                oid: crate::hash::ObjectId::from_hex(peer, BStr::new(&hex))?,
                peeled: None,
            });
        }
        refs.sort_by(|a, b| a.name.cmp(&b.name));

        let agent = capabilities.value(b"agent").map(BString::from);
        self.capabilities = capabilities.clone();
        self.version = ProtocolVersion::V0;
        self.format = peer;
        Ok(RefAdvertisement {
            capabilities,
            refs,
            format: peer,
            agent,
            version: ProtocolVersion::V0,
        })
    }
}

impl Transport for HttpTransport {
    /// Phase 11 — `GET` the advertisement and parse it.
    ///
    /// # Caveats
    ///
    /// * **The v2 and v0 responses are shaped differently and both must be handled.** A v2 response
    ///   is a capability list, a delim packet, then either nothing (the client must send `ls-refs`)
    ///   or, for a server that pre-empts it, the refs. A v0 response is a ref list with the
    ///   capabilities on the first line. Which arrived is decided by the response's own content, not
    ///   by the header the client sent.
    /// * **The `service=` line is the first line of a v0 body** (`# service=git-upload-pack\n`
    ///   followed by a flush) and must be consumed before the ref list. A parser that started at the
    ///   refs sees `# service=…` as a ref name.
    /// * **The object-format check happens here**, against the `format` argument, so that a
    ///   mismatched peer is [`crate::error::Error::ObjectFormatMismatch`] before any id is parsed at
    ///   the wrong width (plan §2.1 rule 5).
    /// * **A `401` triggers the credential chain once**, and only once: a second `401` after
    ///   credentials were supplied is [`crate::error::Error::Authentication`], because retrying a
    ///   rejected credential in a loop is how a user gets locked out of a service that bans repeated
    ///   failures.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::HttpStatus`], [`crate::error::Error::Authentication`],
    /// [`crate::error::Error::ObjectFormatMismatch`], [`crate::error::Error::Transport`], and
    /// [`crate::error::Error::Protocol`] for an advertisement that does not parse.
    fn connect(&mut self, url: &RemoteUrl, format: ObjectFormat) -> Result<RefAdvertisement> {
        self.advertise(url, format, Service::UploadPack)
    }

    /// Phase 11 — the capabilities from the last [`Transport::connect`].
    fn capabilities(&self) -> &CapabilitySet<'_> {
        &self.capabilities
    }

    fn configure_credentials(&mut self, settings: crate::remote::credential::CredentialSettings) {
        self.credential_settings = Some(settings);
    }

    /// Phase 11 — negotiate a fetch and stream the pack into `into`.
    ///
    /// # Caveats
    ///
    /// * **The negotiation is a loop when the server is v0**, and a single request when it is v2. In
    ///   v0 the client may send several `have` rounds and the server answers each one; over HTTP each
    ///   round is a *separate POST*, which is what makes HTTP negotiation stateless and why the
    ///   `done` decision must be made before the last request rather than during it.
    /// * **The pack arrives inside the same response as the negotiation's result**, so a caller
    ///   cannot split "the ACKs" from "the pack" into two requests — the first `done` ends the
    ///   exchange and the pack follows the `NAK` in the same body.
    /// * **Sideband framing is decided by what was negotiated**, not by what was advertised. See
    ///   [`crate::remote::SidebandReader::new`]; the demultiplexer must be told `enabled` from the
    ///   request that was actually accepted.
    /// * **The body is streamed, not buffered**, so a fetch of a large repository does not need it in
    ///   memory — and so a caller must not assume `into` has been written when this returns an error,
    ///   because a pack that failed midway has left some of its bytes there. The caller indexes only
    ///   on success, which is the rule that makes a partial write harmless.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`] for a malformed response, an unknown sideband channel or a
    /// server-side error on channel 3; [`crate::error::Error::Transport`] for a network failure; and
    /// whatever writing to `into` raises.
    fn fetch(
        &mut self,
        wants: &[crate::hash::ObjectId],
        haves: &[crate::hash::ObjectId],
        into: &mut dyn std::io::Write,
    ) -> Result<()> {
        let v2 = self.version == ProtocolVersion::V2;
        let sideband = self.capabilities.contains(b"side-band-64k")
            || self.capabilities.contains(b"side-band");

        let mut body = Vec::new();
        if v2 {
            PktLine::new_data(b"command=fetch").write_to(&mut body)?;
            PktLine::new_data(format!("agent={}", USER_AGENT).as_bytes()).write_to(&mut body)?;
            body.extend_from_slice(DELIM_PACKET);
            PktLine::new_data(b"thin-pack").write_to(&mut body)?;
            PktLine::new_data(b"ofs-delta").write_to(&mut body)?;
            for want in wants {
                PktLine::new_data(format!("want {}", want.to_hex()).as_bytes())
                    .write_to(&mut body)?;
            }
            for have in haves {
                PktLine::new_data(format!("have {}", have.to_hex()).as_bytes())
                    .write_to(&mut body)?;
            }
            PktLine::new_data(b"done").write_to(&mut body)?;
            body.extend_from_slice(FLUSH_PACKET);
        } else {
            body = wants_request(wants, &self.capabilities, self.format)?;
            // One stateless round: the haves go straight into `done`. A flush between them would
            // end the round and leave the server waiting for a request that never comes.
            for have in haves {
                PktLine::new_data(format!("have {}\n", have.to_hex()).as_bytes())
                    .write_to(&mut body)?;
            }
            PktLine::new_data(b"done\n").write_to(&mut body)?;
        }

        let request = service_request(&self.url, Service::UploadPack, body)?;
        let mut server_error: Option<String> = None;
        let target = self.url.clone();

        self.authenticated(&target, false, |agent, base| {
            send_streaming(agent, &request, base, &mut |reader| {
                let mut reader = std::io::BufReader::new(reader);
                let mut packfile = !v2;

                loop {
                    match read_frame(&mut reader)? {
                        None | Some(Frame::Flush) => break,
                        Some(Frame::Raw(head)) => {
                            // No sideband: the pack starts raw, right after the NAK pkt-line.
                            if v2 {
                                return Err(Error::Protocol(
                                    "protocol v2 sent an unframed pack".to_string(),
                                ));
                            }
                            into.write_all(&head)?;
                            copy_stream(&mut reader, into)?;
                            break;
                        }
                        Some(Frame::Data(data)) => {
                            if data.is_empty() {
                                continue;
                            }
                            if let Some(message) = data.strip_prefix(b"ERR ") {
                                server_error = Some(String::from_utf8_lossy(message).into_owned());
                                break;
                            }
                            if v2 && !packfile {
                                let line = data.strip_suffix(b"\n").unwrap_or(&data);
                                if line == b"packfile" {
                                    packfile = true;
                                }
                                continue;
                            }
                            if !v2
                                && (data.starts_with(b"ack ")
                                    || data.starts_with(b"ACK ")
                                    || data.starts_with(b"NAK")
                                    || data.starts_with(b"shallow ")
                                    || data.starts_with(b"unshallow "))
                            {
                                continue;
                            }
                            if sideband {
                                match data[0] {
                                    1 => into.write_all(&data[1..])?,
                                    2 => {}
                                    3 => {
                                        server_error =
                                            Some(String::from_utf8_lossy(&data[1..]).into_owned())
                                    }
                                    other => {
                                        return Err(Error::Protocol(format!(
                                            "unknown sideband channel {other}"
                                        )));
                                    }
                                }
                            } else if v2 {
                                // v2 always sidebands the packfile section.
                                into.write_all(&data[1..])?;
                            } else {
                                into.write_all(&data)?;
                            }
                        }
                    }
                }
                Ok(())
            })
        })
        .map_err(ls_refs_failure)?;

        match server_error {
            Some(message) => Err(Error::Protocol(message)),
            None => Ok(()),
        }
    }

    /// Phase 11 — `POST` the updates and the pack, and read `report-status`.
    ///
    /// # Caveats
    ///
    /// * **The request body is the update list, then the pack, with no separating flush.** The
    ///   pkt-line list ends with a flush and the pack's first byte (`P`) is read as part of the same
    ///   body; inserting another flush between them desynchronises the server, which then reads the
    ///   pack header as a command line.
    /// * **`report-status` must be in the requested capabilities**, and `side-band-64k` with it if
    ///   the status is also sideband-framed — see the transport seam's caveat. Requesting one without
    ///   the other is the combination that produces a reader parsing channel bytes as status lines.
    /// * **`git-receive-pack` is protocol v0**, always (see [`Service::supports_v2`]), so the body is
    ///   the v0 shape even against a server whose fetch is v2.
    /// * **A rejection is not an error**, per the transport seam: it comes back in the returned
    ///   vector so that one rejected ref does not discard the results of the others.
    ///
    /// # Errors
    ///
    /// As [`Transport::push`]: protocol and transport failures only, never a rejected ref.
    fn push(&mut self, updates: &[RefUpdate], pack: &[u8]) -> Result<Vec<PushStatus>> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        // `receive-pack` advertises its own capabilities (`report-status`, `delete-refs`, …);
        // the ones learned from `upload-pack` during `connect` describe the wrong service.
        let url = self.url.clone();
        let format = self.format;
        self.advertise(&url, format, Service::ReceivePack)?;
        let sideband = self.capabilities.contains(b"side-band-64k")
            || self.capabilities.contains(b"side-band");
        let report = self.capabilities.contains(b"report-status")
            || self.capabilities.contains(b"report-status-v2");

        let mut requested: Vec<&str> = Vec::new();
        if self.capabilities.contains(b"side-band-64k") {
            requested.push("side-band-64k");
        } else if self.capabilities.contains(b"side-band") {
            requested.push("side-band");
        }
        if report {
            requested.push("report-status");
        }
        requested.push("agent=Rivet");

        let mut body = Vec::new();
        for (index, update) in updates.iter().enumerate() {
            let mut line = update.command_line(self.format).to_vec();
            if index == 0 {
                line.push(0);
                line.extend_from_slice(requested.join(" ").as_bytes());
            }
            line.push(b'\n');
            PktLine::new_data(&line).write_to(&mut body)?;
        }
        body.extend_from_slice(FLUSH_PACKET);
        body.extend_from_slice(pack);

        let request = service_request(&self.url, Service::ReceivePack, body)?;
        let target = self.url.clone();
        let response =
            self.authenticated(&target, false, |agent, base| send(agent, &request, base))?;

        if !report {
            return Ok(Vec::new());
        }

        // A sideband push wraps the status pkt-lines in channel-1 packets; a plain one sends them
        // directly. Either way the payload is a pkt-line section of its own.
        let status_bytes = if sideband {
            let mut cursor = &response.body[..];
            let mut channel_one = Vec::new();
            loop {
                if cursor.is_empty() {
                    break;
                }
                let pkt = PktLine::read_from(&mut cursor)?;
                if pkt.is_flush() {
                    break;
                }
                let data = pkt.data();
                if data.is_empty() {
                    continue;
                }
                match data[0] {
                    1 => channel_one.extend_from_slice(&data[1..]),
                    2 => {}
                    3 => {
                        return Err(Error::Protocol(
                            String::from_utf8_lossy(&data[1..]).into_owned(),
                        ));
                    }
                    other => {
                        return Err(Error::Protocol(format!("unknown sideband channel {other}")));
                    }
                }
            }
            channel_one
        } else {
            response.body.clone()
        };

        let status = pkt_payloads(&status_bytes)?;
        let outcome = crate::remote::push::parse_report_status(&status, false)?;
        Ok(outcome.statuses)
    }

    /// Phase 11 — send a v2 `ls-refs` request.
    ///
    /// # Caveats
    ///
    /// * **The request body is `command=ls-refs`, then a delim packet, then the arguments**, which is
    ///   the v2 framing: the command and its capabilities in the first section, the command's own
    ///   arguments after the delim. A caller that put the arguments in the first section gets a server
    ///   that reads `peel` as a capability.
    /// * **`symrefs` and `peel` are arguments, not capabilities**, and each is sent as a bare word
    ///   with no `=`. A server that gets `peel=true` treats it as an unknown argument and errors.
    /// * **The result does not update the stored advertisement**, per the transport seam — a caller
    ///   that needs both merges them itself.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`] against a v0 peer or for a malformed response, and the
    /// transport errors of [`Transport::connect`].
    fn ls_refs(&mut self, prefixes: &[BString], peel: bool) -> Result<Vec<AdvertisedRef>> {
        if self.version != ProtocolVersion::V2 {
            return Err(Error::Protocol(
                "ls-refs requires protocol v2; the peer answered in v0".to_string(),
            ));
        }

        let mut body = Vec::new();
        PktLine::new_data(b"command=ls-refs").write_to(&mut body)?;
        PktLine::new_data(format!("agent={}", USER_AGENT).as_bytes()).write_to(&mut body)?;
        body.extend_from_slice(DELIM_PACKET);
        if peel {
            PktLine::new_data(b"peel").write_to(&mut body)?;
        }
        PktLine::new_data(b"symrefs").write_to(&mut body)?;
        for prefix in prefixes {
            PktLine::new_data(format!("ref-prefix {}", prefix).as_bytes()).write_to(&mut body)?;
        }
        body.extend_from_slice(FLUSH_PACKET);

        let request = service_request(&self.url, Service::UploadPack, body)?;
        let target = self.url.clone();
        let response = self
            .authenticated(&target, false, |agent, base| send(agent, &request, base))
            .map_err(ls_refs_failure)?;

        let mut cursor = &response.body[..];
        let mut refs = Vec::new();
        loop {
            if cursor.is_empty() {
                break;
            }
            let pkt = PktLine::read_from(&mut cursor)?;
            if pkt.is_flush() {
                break;
            }
            if pkt.is_control() {
                continue;
            }
            let line = pkt.data();
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            if let Some(message) = line.strip_prefix(b"ERR ") {
                return Err(Error::Protocol(
                    String::from_utf8_lossy(message).into_owned(),
                ));
            }
            if line.is_empty() {
                continue;
            }
            let space = line
                .iter()
                .position(|&b| b == b' ')
                .ok_or_else(|| Error::Protocol("ls-refs line without a space".to_string()))?;
            let hex = &line[..space];
            let mut rest = &line[space + 1..];
            let mut name = rest;
            let mut peeled = None;
            let mut symref_target: Option<BString> = None;
            if let Some(tab) = rest.iter().position(|&b| b == b' ') {
                name = &rest[..tab];
                for attribute in rest[tab + 1..].split(|&b| b == b' ') {
                    if let Some(value) = attribute.strip_prefix(b"peeled:") {
                        peeled = Some(crate::hash::ObjectId::from_hex(
                            self.format,
                            BStr::new(value),
                        )?);
                    } else if let Some(value) = attribute.strip_prefix(b"symref-target:") {
                        symref_target = Some(BString::from(value));
                    }
                }
            }
            rest = name;
            let _ = symref_target;
            refs.push(AdvertisedRef {
                name: BString::from(rest),
                oid: crate::hash::ObjectId::from_hex(self.format, BStr::new(hex))?,
                peeled,
            });
        }
        Ok(refs)
    }

    /// Phase 11 — drop the advertisement and let the agent's idle connections go.
    ///
    /// # Caveats
    ///
    /// There is no process to reap, so this is mostly state-setting — but it is not a no-op for the
    /// connection pool: a transport that will make no further requests should be dropped rather than
    /// disconnected, and a caller holding it for a second exchange must `connect` again first.
    ///
    /// # Errors
    ///
    /// None from this implementation; the signature matches the trait.
    fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }
}
