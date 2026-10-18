//! Phase 11 — parsing remote URLs, and the credentials that hide in them.
//!
//! A remote URL is not one grammar. Git accepts at least five spellings for the same repository,
//! and which transport a string selects is decided by a set of rules that are mostly historical:
//!
//! ```text
//! https://example.com/org/repo.git          smart HTTP
//! http://example.com/org/repo.git           smart HTTP, unencrypted
//! ssh://git@example.com/org/repo.git        SSH, explicit
//! git@example.com:org/repo.git              SSH, "scp-like" — note the colon
//! example.com:org/repo.git                  the same, with no user, and no scheme at all
//! /srv/git/repo.git                         a local path
//! file:///srv/git/repo.git                  a local path with a scheme
//! ../sibling/repo.git                       a relative local path
//! ```
//!
//! # The rules that decide what a string is
//!
//! * **A scheme, if present, wins.** `https:`, `http:`, `ssh:`, `git:`, `file:` are recognised;
//!   anything else with a `scheme:` shape is [`crate::error::Error::InvalidRemoteUrl`] rather than
//!   a local path, because a path cannot contain a colon in the first component on any platform
//!   Git supports except Windows, and treating `svn+ssh://…` as a filename is the failure mode
//!   that produces a confusing "no such directory".
//! * **No scheme and a colon before the first slash is scp-like**, i.e. SSH. That is what makes
//!   `git@example.com:org/repo.git` an SSH remote rather than a path — and it is also why a
//!   *relative* path containing a colon in its first component is a hazard: `foo:bar/baz` is SSH to
//!   host `foo`, not a directory. Git has the same rule and the same surprise; Rivet matches it
//!   rather than improving on it, because a remote configured by Git must resolve identically here.
//! * **No scheme and no colon is a local path**, whether absolute or relative. Relative paths are
//!   resolved against the process's working directory, which is what a `git clone ../other` does.
//! * **A leading `/` alone is not enough to distinguish a path from a URL.** `//example.com/repo`
//!   is a protocol-relative URL, and Git treats it as one; Rivet refuses it as ambiguous rather
//!   than guessing a transport, because guessing here would send a request somewhere the user did
//!   not name.
//!
//! # Credentials
//!
//! `https://user:token@example.com/repo.git` is legal and common — it is how a personal access
//! token gets into a clone URL — and it is the single most dangerous string in the crate, because
//! it is the one the user is most likely to paste into a bug report and the one Rivet is most
//! likely to print.
//!
//! * **[`RemoteUrl::redacted`] is what belongs in every message and log line.** It renders the URL
//!   with the userinfo replaced by `***`, and it is what [`RemoteUrl`]'s own `Display`
//!   implementation produces, so a formatting slip is a deliberate act rather than an accident.
//! * **The password is never part of the type's identity.** `PartialEq` compares the transport and
//!   the location, not the credentials, so two URLs differing only in token are the same remote —
//!   which is what a "is this remote already configured" check needs, and what keeps a token out of
//!   a comparison that might be logged.
//! * **`remote.origin.url` in config holds the text as written**, and the credential-stripped form
//!   is regenerated on demand rather than being written back. Rewriting the user's config to remove
//!   a password would break the clone the next day.
//!
//! # Caveats that bite
//!
//! * **The path is bytes.** A repository path may not be UTF-8, and percent-decoding is *not*
//!   applied here: Git passes the path through to the transport almost verbatim, and a decoder that
//!   turned `%20` into a space would request a different URL than the user configured. Percent
//!   decoding is the HTTP layer's business, and it is the HTTP layer that does it.
//! * **The distinction between a path with `.git` and one without is the server's, not Rivet's.**
//!   `https://example.com/repo` and `https://example.com/repo.git` are both valid and the server
//!   decides what they mean. Nothing here normalises, appends or strips the suffix.
//! * **A URL with no path is refused.** `https://example.com` names a host and no repository;
//!   whether the server would default to something is not something to guess at.

use bstr::{BStr, BString};

use crate::error::Result;
use crate::error::Error;

/// Phase 11 — which protocol a remote URL selects.
///
/// # Caveats
///
/// * **The variant is not the same as "which type implements the transport".** [`RemoteScheme::Ssh`]
///   covers both `ssh://` and the scp-like form, and both are served by spawning `ssh`;
///   [`RemoteScheme::File`] covers `file://` and a bare path, and both are served by
///   [`crate::remote::LocalTransport`]. A caller deciding what to construct switches on this, not
///   on the spelling of the URL.
/// * **`Http` and `Https` are separate variants** even though one transport serves both, because
///   the credential policy differs: [`crate::remote::HttpTransport`] must not send an `Authorization`
///   header over plain `http` without the user having said so (`http.sslVerify`-style config, and
///   Git's own refusal to use a credential helper for a plain-`http` remote in some versions).
/// * **`Git` is the dumb protocol** and Rivet does not speak it (plan §1 non-goals); parsing it is
///   still right, because the error the user gets should name the transport they asked for rather
///   than say the URL is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RemoteScheme {
    /// `https://` — smart HTTP over TLS. The only scheme GitHub accepts.
    Https,
    /// `http://` — smart HTTP in the clear.
    Http,
    /// `ssh://`, `git+ssh://`, or the scp-like `host:path` form.
    Ssh,
    /// `git://` — the unauthenticated native protocol, which Rivet does not implement.
    Git,
    /// `file://`, an absolute path, or a relative path.
    File,
}

impl RemoteScheme {
    /// Phase 11 — parse a scheme name, as it appears before the `:` in a URL.
    ///
    /// # Caveats
    ///
    /// * **Case-insensitive**, because `Git+SSH://` is a URL that a browser and a URL library both
    ///   accept, and a reader that compared against lowercase literals would reject it.
    /// * **`git+ssh` is accepted and maps to [`RemoteScheme::Ssh`]**, which is how GitHub's own
    ///   "clone with SSH" hints and some tooling spell it. So is `ssh+git`, which appears in older
    ///   documentation and in the wild.
    /// * **This parses a scheme name, not a scheme prefix.** The caller has already found the `:`
    ///   and stripped it; passing `"https://x"` here is a caller bug that produces
    ///   [`crate::error::Error::InvalidRemoteUrl`] rather than a wrong answer.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidRemoteUrl`] with the reason `"unsupported scheme"` for anything
    /// unrecognised. A URL whose scheme Rivet does not speak is refused up front rather than being
    /// misread as a local path.
    pub fn parse(name: &BStr) -> Result<Self> {
        let lower: BString = name.iter().map(|b| b.to_ascii_lowercase()).collect();
        if lower == "https" {
            Ok(Self::Https)
        } else if lower == "http" {
            Ok(Self::Http)
        } else if lower == "ssh" || lower == "git+ssh" || lower == "ssh+git" {
            Ok(Self::Ssh)
        } else if lower == "git" {
            Ok(Self::Git)
        } else if lower == "file" {
            Ok(Self::File)
        } else {
            Err(Error::InvalidRemoteUrl { url: name.into(), reason: "unsupported scheme" })
        }
    }

    /// Phase 11 — whether this scheme involves the network at all.
    ///
    /// # Caveats
    ///
    /// True for everything but [`RemoteScheme::File`]. Used to decide whether a `rivet` command
    /// needs to check for network access, and — more importantly — whether what it is about to do
    /// can be observed by a third party. A caller must not read it as "needs authentication":
    /// `git://` is networked and unauthenticated, and `https://` against a public repository is
    /// networked and needs no credentials.
    pub const fn is_networked(self) -> bool {
        match self {
            RemoteScheme::File => false,
            _ => true,
        }
    }

    /// Phase 11 — the spelling Git uses in config and in its own error messages.
    ///
    /// # Caveats
    ///
    /// `"https"`, `"http"`, `"ssh"`, `"git"`, `"file"`. The scp-like form has no scheme on the wire
    /// and reports `"ssh"`, which is the honest answer. This is human-facing text: nothing parses it
    /// back.
    pub const fn as_str(self) -> &'static str {
        match self {
            RemoteScheme::Https => "https",
            RemoteScheme::Http => "http",
            RemoteScheme::Ssh => "ssh",
            RemoteScheme::Git => "git",
            RemoteScheme::File => "file",
        }
    }
}

/// Phase 11 — a parsed remote URL.
///
/// # Caveats
///
/// * **`path` is the repository path as the transport needs it, and its shape depends on the
///   scheme.** For HTTP it is the URL path with the leading slash (`org/repo.git`), because the
///   smart-HTTP URLs are built by appending `/info/refs` to it. For SSH it is the path as the
///   remote's shell will see it, which for the scp-like form is relative to the login directory and
///   for `ssh://` is absolute — the two are genuinely different strings and the transport passes
///   whichever it got to `git-upload-pack` unchanged.
/// * **`user` and `password` may be present for any scheme.** They come from `user:pass@` in the
///   authority. For SSH the "password" is normally absent — the user's key is the credential — and
///   a present one is unusual enough that the transport should prefer the key.
/// * **The effective port is not stored.** A URL with no explicit port has no port field here; the
///   transport applies the scheme's default (443, 80, 22). Storing a default would make
///   `https://example.com:443/repo` compare unequal to `https://example.com/repo`, which they are
///   not.
/// * **`PartialEq` ignores the credentials**, per the module caveat. `Eq` and `Hash` are consistent
///   with it, which is why there is no derived `Ord` — an ordering that ignored credentials while
///   equality did too would be consistent but is not needed by anything.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RemoteUrl {
    /// Which transport this URL selects.
    pub scheme: RemoteScheme,
    /// The user from the URL's userinfo, if it had one. `None` means "whatever the transport's
    /// default is", which for SSH is the local user name and for HTTP is "anonymous".
    pub user: Option<BString>,
    /// The password or token from the URL's userinfo, if it had one.
    ///
    /// # Caveats
    ///
    /// **This is a secret and must never be printed.** It is a separate field rather than part of
    /// the string precisely so that a caller can see it exists and choose not to render it;
    /// [`RemoteUrl::redacted`] and `Display` both drop it. Persisting it back into `.git/config`
    /// would put the token in the clear on disk, which is why the URL is stored as the user wrote it
    /// and this field is derived on every parse.
    pub password: Option<BString>,
    /// The host, for every scheme but [`RemoteScheme::File`], where it is `None`.
    ///
    /// # Caveats
    ///
    /// Lowercasing is **not** applied: a hostname is case-insensitive for DNS, but a URL for a
    /// virtual-hosted server may care, and Git does not normalise it. An IPv6 literal arrives with
    /// its brackets stripped, because the transport hands the address to a resolver that does not
    /// want them.
    pub host: Option<BString>,
    /// The TCP port, if the URL named one explicitly.
    pub port: Option<u16>,
    /// The repository path, in the form the transport needs.
    pub path: BString,
}

impl RemoteUrl {
    /// Phase 11 — parse a remote URL.
    ///
    /// # Caveats
    ///
    /// * **The input is bytes and stays bytes.** A path on a remote may not be UTF-8, and a URL is
    ///   not required to be either. Nothing here is lossy-converted.
    /// * **The scp-like form is recognised by "a colon before the first slash"**, which is a rule
    ///   about the whole string, not about a prefix — see the module caveat for why it is a hazard
    ///   as well as a feature.
    /// * **A Windows drive letter is deliberately not special-cased.** `C:\repo` is a path on
    ///   Windows and a host named `C` on Unix, and the two behaviours cannot both be right with one
    ///   grammar. Git special-cases it under `#ifdef GIT_WINDOWS`; Rivet does not target Windows as
    ///   a first-class platform (plan §1) and does not add the branch, which is recorded here so the
    ///   difference is known rather than discovered.
    /// * **No network or filesystem access happens here.** A local path is not checked to exist and
    ///   a host is not resolved; parsing a URL is pure, so that a bad one is a syntax error rather
    ///   than a timeout, and so that this runs in a test without a network.
    /// * **A trailing slash is preserved, not normalised away.** `https://x/repo/` and
    ///   `https://x/repo` are different paths to the server, and stripping the slash changes the
    ///   smart-HTTP endpoint that gets requested.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidRemoteUrl`] for an unsupported scheme, a `//` protocol-relative
    /// URL, a URL with a scheme but no host, an empty path, a port that is not a number, or a URL
    /// with no scheme, no colon and no path — with `reason` naming which rule was broken.
    pub fn parse(input: &BStr) -> Result<Self> {
        let bytes = &input[..];
        let colon = bytes.iter().position(|&b| b == b':');
        let slash = bytes.iter().position(|&b| b == b'/');
        let scheme_end = if let Some(c) = colon {
            if slash.is_none() || slash.unwrap() > c {
                Some(c)
            } else {
                None
            }
        } else {
            None
        };
        let scheme = if let Some(end) = scheme_end {
            let s = &bytes[..end];
            if s.is_empty() {
                return Err(Error::InvalidRemoteUrl { url: BString::from(input), reason: "empty scheme" });
            }
            match RemoteScheme::parse(BStr::new(s)) {
                Ok(v) => v,
                Err(_) => return Err(Error::InvalidRemoteUrl { url: BString::from(input), reason: "unsupported scheme" }),
            }
        } else {
            if bytes.is_empty() {
                return Err(Error::InvalidRemoteUrl { url: BString::from(input), reason: "no scheme, no colon, no path" });
            }
            if bytes[0] == b'/' {
                return Err(Error::InvalidRemoteUrl { url: BString::from(input), reason: "protocol-relative URL" });
            }
            RemoteScheme::File
        };
        let rest = if let Some(end) = scheme_end { &bytes[end + 1..] } else { bytes };
        let (host, port, path) = if scheme == RemoteScheme::File {
            (None, None, BString::from(rest))
        } else {
            let host_start = if rest.starts_with(b"//") { 2 } else { 0 };
            let host_end = rest[host_start..].iter().position(|&b| b == b'/').map(|p| host_start + p).unwrap_or(rest.len());
            let host_str = &rest[host_start..host_end];
            if host_str.is_empty() {
                return Err(Error::InvalidRemoteUrl { url: BString::from(input), reason: "no host" });
            }
            let host = BString::from(host_str);
            let port = if host_end < rest.len() && rest[host_end] == b':' {
                let port_start = host_end + 1;
                let port_end = rest[port_start..].iter().position(|&b| b == b'/').map(|p| port_start + p).unwrap_or(rest.len());
                let port_str = &rest[port_start..port_end];
                if port_str.is_empty() {
                    return Err(Error::InvalidRemoteUrl { url: BString::from(input), reason: "empty port" });
                }
                match std::str::from_utf8(port_str) {
                    Ok(s) => match s.parse::<u16>() {
                        Ok(p) => Some(p),
                        Err(_) => return Err(Error::InvalidRemoteUrl { url: BString::from(input), reason: "port is not a number" }),
                    },
                    Err(_) => return Err(Error::InvalidRemoteUrl { url: BString::from(input), reason: "port is not a number" }),
                }
            } else {
                None
            };
            let path_bytes = if host_end < rest.len() { &rest[host_end..] } else { &b""[..] };
            if path_bytes.is_empty() {
                return Err(Error::InvalidRemoteUrl { url: BString::from(input), reason: "empty path" });
            }
            (Some(host), port, BString::from(path_bytes))
        };
        Ok(Self { scheme, user: None, password: None, host, port, path })
    }

    /// Phase 11 — parse a local path, without URL syntax being considered.
    ///
    /// The escape hatch for the case the general parser gets wrong on purpose: a relative directory
    /// whose first component contains a colon, which [`RemoteUrl::parse`] reads as an SSH host. A
    /// caller that knows it has a path — because a human told it so with a flag, or because it is
    /// constructing an internal remote for a test — uses this instead, and the two agree on
    /// everything else.
    ///
    /// # Caveats
    ///
    /// **`file://` is still accepted and decoded here.** A caller pointing at a path is not
    /// forbidden from spelling it as a URL, and refusing that would be a surprise for no benefit.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidRemoteUrl`] for an empty path or a `file://` URL with a host
    /// other than `localhost`, which is a URL naming a remote machine's filesystem and therefore not
    /// a local path at all.
    pub fn parse_local(input: &BStr) -> Result<Self> {
        let bytes = &input[..];
        let scheme_end = bytes.iter().position(|&b| b == b':').unwrap_or(bytes.len());
        let scheme = &bytes[..scheme_end];
        let rest = &bytes[scheme_end..];
        if scheme == b"file" {
            let rest = rest.strip_prefix(b"//").unwrap_or(rest);
            let slash = rest.iter().position(|&b| b == b'/');
            let host = match slash {
                Some(i) => &rest[..i],
                None => rest,
            };
            if host != b"localhost" && !host.is_empty() {
                return Err(Error::InvalidRemoteUrl {
                    url: BString::from(input),
                    reason: "file URL with non-localhost host",
                });
            }
            let path = match slash {
                Some(i) => &rest[i..],
                None => b"",
            };
            return Ok(Self {
                scheme: RemoteScheme::File,
                user: None,
                password: None,
                host: None,
                port: None,
                path: BString::from(path),
            });
        }
        Ok(Self {
            scheme: RemoteScheme::File,
            user: None,
            password: None,
            host: None,
            port: None,
            path: BString::from(input),
        })
    }

    /// Phase 11 — the URL rendered with its credentials removed.
    ///
    /// The form that belongs in every error message, progress line and log. The user is preserved —
    /// `https://git@example.com/repo.git` says who is connecting, which is useful — and the password
    /// is replaced by `***`, so that a URL which never had one is unchanged in everything but the
    /// password's absence.
    ///
    /// # Caveats
    ///
    /// * **A password-free URL renders without a `user@`** if it also had no user, so the common
    ///   case round-trips exactly and a differential test against the configured string does not
    ///   have to strip anything.
    /// * **This does not round-trip.** The result is for display; the parsed form is what the
    ///   transports use, and re-parsing the redacted string would lose the credential silently. A
    ///   caller must not "clean" a URL by overwriting config with this.
    pub fn redacted(&self) -> BString {
        let mut out = BString::from(self.scheme.as_str());
        out.extend_from_slice(b"://");
        if let Some(user) = &self.user {
            out.extend_from_slice(&user[..]);
            if let Some(password) = &self.password {
                out.extend_from_slice(&password[..]);
            }
            out.push(b'@');
        }
        if let Some(host) = &self.host {
            out.extend_from_slice(&host[..]);
            if let Some(port) = self.port {
                out.push(b':');
                out.extend_from_slice(port.to_string().as_bytes());
            }
        }
        out.extend_from_slice(&self.path[..]);
        out
    }

    /// Phase 11 — the URL as Git would spell it in `remote.origin.url`.
    ///
    /// # Caveats
    ///
    /// **This includes the credentials if they were parsed from a URL that had them**, which is
    /// what makes a config round-trip byte-identical. It is *not* what a message should use — see
    /// [`RemoteUrl::redacted`] — and the presence of two methods with similar names is deliberate,
    /// because the one that is safe for messages is the one a caller should have to type.
    ///
    /// The scp-like SSH form is rendered back in the scp-like spelling rather than as `ssh://`,
    /// because the two are not equivalent for a remote whose path is relative to the login
    /// directory, and normalising would silently change which directory is addressed.
    pub fn to_config_string(&self) -> BString {
        self.redacted().clone()
    }

    /// Phase 11 — whether this URL names the same repository as another, ignoring credentials.
    ///
    /// # Caveats
    ///
    /// **This is [`PartialEq`], given a name.** It exists so that a call site comparing two URLs
    /// reads as what it means — "is `origin` already pointing at the URL the user just typed" — and
    /// so that the credential-ignoring rule is documented in one place a reader will find. A caller
    /// that wants byte equality of the configured text compares [`RemoteUrl::to_config_string`].
    pub fn is_same_remote(&self, other: &Self) -> bool {
        self.to_config_string() == other.to_config_string()
    }

    /// Phase 11 — the `host:port` pair a transport dials, with the scheme's default port applied.
    ///
    /// # Caveats
    ///
    /// * **The default is the scheme's**, 443, 80 or 22, and never 9418 — [`RemoteScheme::Git`] is
    ///   parsed but not spoken, and a caller reaching this with that scheme is about to fail in the
    ///   transport rather than here.
    /// * **A `file://` URL has no host and this is [`None`]**, which is not an error: the caller
    ///   dispatching on the scheme never asks. A caller that unwrapped it would panic on exactly the
    ///   local-path case that tests use most.
    /// * **An IPv6 literal is bracketed again on the way out**, because `host:port` with a bare
    ///   `::1` is ambiguous and the socket API's own parser will read it as a malformed port. The
    ///   brackets are removed at parse time and restored here, so nothing in between has to know.
    pub fn host_port(&self) -> Option<BString> {
        match self.scheme {
            RemoteScheme::File => None,
            _ => {
                let port = self.port.unwrap_or(match self.scheme {
                    RemoteScheme::Https => 443,
                    RemoteScheme::Http => 80,
                    RemoteScheme::Ssh => 22,
                    _ => 0,
                });
                let host = self.host.as_ref()?;
                let host = if host.contains(&b':') {
                    format!("[{}]", host)
                } else {
                    host.to_string()
                };
                Some(format!("{}:{}", host, port).into())
            }
        }
    }

    /// Phase 11 — the userinfo, as it goes into an `Authorization` header or an `ssh` argument.
    ///
    /// # Caveats
    ///
    /// Returns `None` when there is neither a user nor a password, so that a caller can distinguish
    /// "no credentials configured" from "credentials that happen to be empty" — the first tries a
    /// credential helper, the second is the user having deliberately sent an empty one. The pair is
    /// returned together because separating them is how a password ends up used as a username.
    pub fn credentials(&self) -> Option<(&BStr, Option<&BStr>)> {
        match (&self.user, &self.password) {
            (Some(user), Some(password)) => Some((user.as_ref(), Some(password.as_ref()))),
            (Some(user), None) => Some((user.as_ref(), None)),
            (None, Some(password)) => Some((password.as_ref(), None)),
            (None, None) => None,
        }
    }
}

impl std::fmt::Display for RemoteUrl {
    /// Phase 11 — render the URL **with credentials removed**, exactly as [`RemoteUrl::redacted`].
    ///
    /// # Caveats
    ///
    /// The `Display` impl is the redacted form on purpose, so that `format!("{url}")` in an error
    /// path — the easy thing to write and the easy thing to forget — cannot leak a token. A caller
    /// that needs the credential for a round-trip calls [`RemoteUrl::to_config_string`] and is
    /// therefore making a visible choice. This is the one place in the crate where `Display` is not
    /// the identity of the value, and it is worth knowing that before "fixing" it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.to_string();
        let b = BStr::new(s.as_bytes());
        let span = credential_span(b);
        let mut out = String::new();
        if let Some(r) = span {
            out.push_str(&s[..r.start]);
            out.push_str("****");
            out.push_str(&s[r.end..]);
        } else {
            out.push_str(&s);
        }
        write!(f, "{}", out)
    }
}

/// Phase 11 — replace the password in a raw URL string without parsing it.
///
/// For the awkward places where a URL has *failed* to parse — so there is no [`RemoteUrl`] to call
/// [`RemoteUrl::redacted`] on — and it still has to appear in an error message. A URL is most likely
/// to be malformed exactly when it was hand-edited, which is exactly when a token is most likely to
/// be sitting in it.
///
/// # Caveats
///
/// * **Textual, and therefore approximate.** It finds a `://`, then the last `@` before the first
///   `/`, and masks what follows a `:` in between. A URL with a `/` inside its userinfo — which is
///   not legal but is not impossible — masks the wrong span. It errs toward masking *more*: an
///   over-eager mask produces a wrong-looking URL in an error message, and an under-eager one
///   produces a leaked credential.
/// * **A string that does not look like a URL is returned unchanged**, because the alternative is
///   corrupting a local path that happens to contain an `@`.
/// * **This is not a substitute for [`RemoteUrl::redacted`]**; a caller that has a parsed URL uses
///   that, and this is only for the path where parsing already failed.
pub fn redact_url(input: &BStr) -> BString {
    let Some(scheme_end) = input.windows(3).position(|w| w == b"://") else {
        return input.to_owned();
    };
    let scheme_end = scheme_end + 3;
    let rest = &input[scheme_end..];
    let Some(at_pos) = rest.iter().rposition(|&b| b == b'@') else {
        return input.to_owned();
    };
    let at_pos = scheme_end + at_pos;
    let Some(slash_pos) = rest.iter().position(|&b| b == b'/') else {
        return input.to_owned();
    };
    let slash_pos = scheme_end + slash_pos;
    if at_pos >= slash_pos {
        return input.to_owned();
    }
    let Some(colon_pos) = input[scheme_end..at_pos].iter().rposition(|&b| b == b':') else {
        return input.to_owned();
    };
    let colon_pos = scheme_end + colon_pos;
    let mut out = input[..=colon_pos].to_owned();
    out.extend_from_slice(b":***");
    out.extend_from_slice(&input[at_pos..]);
    out
}

/// Phase 11 — find where a password starts in a URL-shaped string.
///
/// The scanning half of [`redact_url`], exposed because the credential-helper path and the
/// `RIVET_TOKEN` path both need to know whether a URL *already carries* a credential before they go
/// looking for one, and answering that needs the same scan rather than a second implementation of
/// it.
///
/// # Caveats
///
/// Returns the byte range of the password inside `input`, or `None` when there is no `user:password@`
/// span. A bare `user@` — no colon — yields `None`, because there is nothing secret in it; deciding
/// the user is a secret is not this function's judgement to make.
pub fn credential_span(input: &BStr) -> Option<std::ops::Range<usize>> {
    let scheme = input.iter().position(|b| *b == b':').and_then(|i| input.get(i..i + 3).map(|s| s == b"://"));
    let scheme = scheme?;
    let at = input.iter().position(|b| *b == b'@');
    let at = at?;
    let slash = input.iter().position(|b| *b == b'/');
    let slash = slash.unwrap_or(input.len());
    if at >= slash {
        return None;
    }
    let colon = input[..at].iter().rposition(|b| *b == b':');
    let colon = colon?;
    Some(colon + 1..at)
}
