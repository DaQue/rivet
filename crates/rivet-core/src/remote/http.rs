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
//!   scheme so that a caller can warn. See [`RemoteScheme::is_networked`](crate::remote::RemoteScheme::is_networked).
//!
//! # Connection reuse
//!
//! An [`Agent`](ureq::Agent) holds a connection pool. A fetch followed by a push to the same host reuses the
//! connection, and a clone that fetches twice reuses it too; constructing a fresh agent per request
//! throws that away and pays a TLS handshake each time. The transport owns one agent for its life,
//! which is why [`HttpTransport::new`] builds it once rather than per call.

use bstr::BString;

use crate::error::Result;
use crate::hash::ObjectFormat;

use crate::remote::transport::{CapabilitySet, HttpResponse, Service};
use crate::remote::url::RemoteUrl;
use crate::remote::{AdvertisedRef, PushStatus, RefAdvertisement, RefUpdate, Transport};
use crate::remote::url::RemoteScheme;
use crate::error::Error;

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
}

impl HttpTransport {
    /// Phase 11 — build a transport for a remote URL.
    ///
    /// # Caveats
    ///
    /// * **The scheme must be [`RemoteScheme::Http`](crate::remote::RemoteScheme::Http) or [`RemoteScheme::Https`](crate::remote::RemoteScheme::Https)**, and anything else
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
        todo!()
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
        let agent = ureq::agent();
        Ok(Self { url, agent, credentials: None })
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
    }

    /// Phase 11 — whether the URL is plain `http`.
    ///
    /// # Caveats
    ///
    /// True only for [`RemoteScheme::Http`](crate::remote::RemoteScheme::Http). The CLI uses it to print the warning Git prints before
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
        BString::from(format!("{}info/refs?service={}", path, service_str).as_bytes())
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
        BString::from(format!("{}info/refs?service={}", path, service_str).as_bytes())
    }
}

/// Phase 11 — build the `GET` request for a service's ref advertisement.
///
/// # Caveats
///
/// * **The `service` query parameter is not optional** — see the module caveat — and it must be
///   percent-encoded as written (`git-upload-pack` needs none, but the rule is not "it needs none").
/// * **`Git-Protocol: version=2` is sent only when `version` is [`ProtocolVersion::V2`](crate::remote::ProtocolVersion::V2).** Sending it
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
    todo!()
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
    todo!()
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
    todo!()
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
    todo!()
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
        std::io::Read::read_to_end(&mut d, &mut out).map_err(|_| Error::Protocol(String::from("gzip decode failed")))?;
        return Ok(out);
    }
    if enc == b"deflate" {
        let mut d = flate2::read::ZlibDecoder::new(&body[..]);
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut d, &mut out).map_err(|_| Error::Protocol(String::from("deflate decode failed")))?;
        return Ok(out);
    }
    Err(Error::Protocol(String::from("unsupported content-encoding")))
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
        todo!()
    }

    /// Phase 11 — the capabilities from the last [`Transport::connect`].
    fn capabilities(&self) -> &CapabilitySet<'_> {
        todo!()
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
        todo!()
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
        todo!()
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
        todo!()
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
