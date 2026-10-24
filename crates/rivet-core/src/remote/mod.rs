//! Phase 11 — talking to other repositories: the wire protocol, the transports, fetch and push.
//!
//! This is the module that makes Rivet a Git client rather than a Git library. Everything below it
//! — the object database, the pack format, the refs store — is about a repository on this disk;
//! everything here is about one that is somewhere else, reachable over a stream of bytes whose format
//! two independent implementations have to agree on.
//!
//! # The shape of the layer
//!
//! ```text
//!   fetch / push          ← policy: which objects, which refs, what is refused
//!      │
//!   Transport (trait)     ← the seam: one fetch path, three transports
//!      ├── HttpTransport     smart HTTP over ureq
//!      ├── SshTransport      ssh + git-upload-pack on the far end
//!      └── LocalTransport    a directory on this disk
//!          │
//!   pktline               ← framing: 4 hex bytes, flush, delim, sideband channels
//! ```
//!
//! [`Transport`] is the seam and it is a trait rather than an enum on purpose. The plan's rule is
//! that smart HTTP and SSH share one fetch path, and an enum would put the sharing in a `match` that
//! every new transport has to edit; a trait puts it in the code that drives the exchange, and adding a
//! transport then touches only the transport. The one thing the trait cannot hide is that HTTP is
//! request-and-response while SSH is a conversation over one long-lived pipe — and it does not need
//! to, because both are, from [`fetch()`]'s point of view, "write the wants, read the pack".
//!
//! # Everything here is bytes
//!
//! Ref names, paths, hostnames, progress text and capability strings are [`bstr::BString`] and
//! [`bstr::BStr`], never `String`. A ref name is a path on disk and may be in any encoding the
//! filesystem allows; a repository path in an SSH URL is an argument to a remote shell and is not
//! required to be UTF-8; a server's progress text is whatever the server's locale produced. `String`
//! appears only for text Rivet composes itself — an error message — which is the same rule the rest
//! of the crate follows.
//!
//! # Credentials never enter a log line
//!
//! A remote URL may carry a user and a password, and an HTTP exchange may carry a token in a header.
//! The rule this module enforces mechanically is that the credential-bearing spelling of a URL is
//! reachable only through an explicitly-named method, and that `Display` and `Debug` produce the
//! redacted form. `format!("{url}")` therefore cannot leak a token into a `git fetch` error message,
//! a log line, or a bug report — which is the failure mode that turns a helpful diagnostic into a
//! disclosure. See [`RemoteUrl::redacted`], [`redact_url`] and [`HttpRequest`]'s treatment of
//! `Authorization`.
//!
//! # What is deliberately not here
//!
//! * **Protocol v2.** [`ProtocolVersion`] exists and the HTTP transport can request v2, but the
//!   negotiation paths are v0. v2's `ls-refs` and `fetch` commands are a different shape of exchange
//!   and adding them is a change to [`fetch()`] rather than to any signature here.
//! * **Shallow and partial clones.** No `deepen`, no filter spec, and no `FetchSpec` field
//!   pretending otherwise.
//! * **Refspec parsing.** The wildcard grammar belongs with the config that spells it; see
//!   [`fetch`](mod@self::fetch) module documentation for what stands in for it.
//! * **Bundles, `git://`, and dumb HTTP.** A `git://` URL parses and reports that Rivet does not
//!   speak the transport, which is a better failure than attempting a TCP connection to port 9418 and
//!   hanging. Dumb HTTP is deliberately unsupported: it requires walking loose objects over
//!   individual HTTP requests, and no server has offered it as the only option for a decade.
//! * **`push --atomic`.** Named in [`push()`]'s capability list as a thing Rivet does not request,
//!   because requesting it and then reporting a partial success normally would be worse than not
//!   offering it.

pub mod credential;
pub mod fetch;
pub mod http;
pub mod pktline;
pub mod push;
pub mod sideband;
pub mod ssh;
pub mod transport;
pub mod url;

// ── framing ─────────────────────────────────────────────────────────────────────────────────────

pub use crate::remote::pktline::{
    DELIM_PACKET, FLUSH_PACKET, MAX_PKT_DATA, MAX_SIDEBAND_DATA, MIN_PKT_LEN, PktLine,
    RESPONSE_END_PACKET, SidebandKind, SidebandPacket, SidebandReader,
};

// ── urls ────────────────────────────────────────────────────────────────────────────────────────

pub use crate::remote::url::{RemoteScheme, RemoteUrl, credential_span, redact_url};

// ── the transport seam ──────────────────────────────────────────────────────────────────────────

pub use crate::remote::transport::{
    Ack, AdvertisedRef, Capability, CapabilitySet, HttpRequest, HttpResponse, LocalTransport,
    ProtocolVersion, RefAdvertisement, Service, Transport, can_use_multi_ack, haves_request,
    is_fast_forward, parse_ack, wants_request,
};

// ── transports ──────────────────────────────────────────────────────────────────────────────────

pub use crate::remote::http::{
    DEFAULT_TIMEOUT, HttpTransport, USER_AGENT, advertise_request, decode_body, is_auth_challenge,
    is_success, media_type, send, send_streaming, service_request,
};

pub use crate::remote::ssh::{
    GIT_SSH_COMMAND_ENV, GIT_SSH_ENV, GIT_SSH_PORT_ENV, SSH_PROGRAM, SshInvocation, SshTransport,
    has_ssh_override, quote_remote_path, remote_command, ssh_args, ssh_command_for,
};

// ── fetch ───────────────────────────────────────────────────────────────────────────────────────

pub use crate::remote::fetch::{
    FetchCapabilities, FetchOptions, FetchOutcome, FetchRefUpdate, FetchRefs, FetchSpec,
    HAVES_PER_ROUND, MAX_NEGOTIATION_ROUNDS, Negotiation, fetch, guess_remote_name, is_up_to_date,
    local_haves, missing_from, negotiate, tracking_ref_name, wants_for_tracking,
    write_tracking_refs,
};

// ── push ────────────────────────────────────────────────────────────────────────────────────────

pub use crate::remote::push::{
    PushOutcome, PushSpec, PushStatus, REPORT_STATUS, REPORT_STATUS_V2, RefUpdate, SIDE_BAND_64K,
    STATUS_OK, THIN_PACK, check_updates, force_prefix, parse_report_status, push,
    push_capabilities, push_is_fast_forward, push_request, update_commands,
};

// ── sideband ────────────────────────────────────────────────────────────────────────────────────

pub use crate::remote::sideband::{
    ChannelWrite, PackSink, SidebandOptions, StoredPack, demultiplex, frame, read_status_stream,
    store_pack, stream_pack, write_framed,
};

/// Phase 11 — the name the shared vocabulary fixes for "a remote-tracking ref was moved".
///
/// # Caveats
///
/// **It is [`FetchRefUpdate`] under a second name**, and the duplication is deliberate: the plan and
/// the CLI brief both say `FetchOutcome` and `RefUpdate` and neither names this type, so a caller
/// reading the plan looks for a name that does not exist. A type alias costs nothing and makes the
/// plan's vocabulary findable; the underlying type is the one with the doc comment explaining the
/// fields, and this alias deliberately carries only the pointer.
pub type FetchRefResult = FetchRefUpdate;
