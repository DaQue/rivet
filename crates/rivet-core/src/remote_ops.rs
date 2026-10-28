//! Porcelain-level remote operations: `fetch`, `push` and `clone` as the commands use them.
//!
//! `remote::fetch` and `remote::push` move objects and know nothing about configuration,
//! refspecs, reflogs or what the user is shown. This module is that layer: it reads
//! `remote.<name>.*`, maps remote refs to local ones, records the reflog lines Git records,
//! writes `FETCH_HEAD`, and returns the per-ref results the CLI prints.

use std::path::Path;

use bstr::{BStr, BString, ByteSlice};

use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::object::ObjectKind;
use crate::refs::RefName;
use crate::remote::{
    AdvertisedRef, HttpTransport, LocalTransport, RefAdvertisement, RefUpdate, RemoteScheme,
    RemoteUrl, SshTransport, Transport,
};
use crate::repo::Repository;

/// `[+]<src>:<dst>` — which remote refs go to which local ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refspec {
    /// A leading `+`: update even when the move is not a fast-forward.
    pub force: bool,
    /// The remote side; may contain one `*`.
    pub src: BString,
    /// The local side; empty when the refspec only names what to fetch.
    pub dst: BString,
}

impl Refspec {
    /// Parse `+refs/heads/*:refs/remotes/origin/*`, `main`, `main:other` or `:gone`.
    pub fn parse(text: &BStr) -> Result<Self> {
        let (force, rest) = match text.strip_prefix(b"+") {
            Some(rest) => (true, rest),
            None => (false, text.as_bytes()),
        };
        // The last colon splits, as in Git: `a:b:c` is the source `a:b` and the destination `c`.
        let (src, dst) = match rest.rfind_byte(b':') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &b""[..]),
        };
        if src.iter().filter(|b| **b == b'*').count() > 1
            || dst.iter().filter(|b| **b == b'*').count() > 1
        {
            return Err(Error::Message(format!("invalid refspec '{text}'")));
        }
        Ok(Self {
            force,
            src: BString::from(src),
            dst: BString::from(dst),
        })
    }

    /// Whether the source contains a `*`.
    pub fn is_pattern(&self) -> bool {
        self.src.contains(&b'*')
    }

    /// The local name a remote ref maps to under this refspec.
    pub fn map(&self, name: &[u8]) -> Option<BString> {
        if !self.is_pattern() {
            return (self.src.as_slice() == name).then(|| self.dst.clone());
        }
        let star = self.src.find_byte(b'*')?;
        let (prefix, suffix) = (&self.src[..star], &self.src[star + 1..]);
        if name.len() < prefix.len() + suffix.len()
            || !name.starts_with(prefix)
            || !name.ends_with(suffix)
        {
            return None;
        }
        let captured = &name[prefix.len()..name.len() - suffix.len()];
        let mut out = BString::default();
        match self.dst.find_byte(b'*') {
            Some(i) => {
                out.extend_from_slice(&self.dst[..i]);
                out.extend_from_slice(captured);
                out.extend_from_slice(&self.dst[i + 1..]);
            }
            None => out.extend_from_slice(&self.dst),
        }
        Some(out)
    }
}

/// A configured remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteConfig {
    /// The name after `remote.`.
    pub name: BString,
    /// `remote.<name>.url`, in order.
    pub urls: Vec<BString>,
    /// `remote.<name>.pushurl`, in order.
    pub push_urls: Vec<BString>,
    /// `remote.<name>.fetch`, in order.
    pub fetch: Vec<Refspec>,
    /// `remote.<name>.push`, in order.
    pub push: Vec<BString>,
}

/// Open the transport a URL names.
pub fn open_transport(url: &RemoteUrl) -> Result<Box<dyn Transport>> {
    Ok(match url.scheme {
        RemoteScheme::File => {
            use std::os::unix::ffi::OsStrExt;
            if crate::bundle::looks_like_bundle(Path::new(std::ffi::OsStr::from_bytes(&url.path))) {
                Box::new(crate::bundle::BundleTransport::open(url))
            } else {
                Box::new(LocalTransport::open(url)?)
            }
        }
        RemoteScheme::Ssh => Box::new(SshTransport::new(url.clone())?),
        RemoteScheme::Http | RemoteScheme::Https => Box::new(HttpTransport::new(url.clone())?),
        RemoteScheme::Git => {
            return Err(Error::InvalidRemoteUrl {
                url: url.redacted(),
                reason: "the git:// protocol is not supported",
            });
        }
    })
}

/// Turn a path given on the command line into the absolute form Git stores for a local remote.
pub fn absolutize_url(url: &str) -> String {
    let looks_local = !url.contains("://") && !url.contains(':') || url.starts_with("file:");
    if looks_local
        && !url.starts_with("file:")
        && !Path::new(url).is_absolute()
        && let Ok(abs) = std::fs::canonicalize(url)
    {
        return abs.to_string_lossy().into_owned();
    }
    url.to_string()
}

fn short_name(name: &[u8]) -> String {
    for prefix in [&b"refs/heads/"[..], b"refs/tags/", b"refs/remotes/"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return String::from_utf8_lossy(rest).into_owned();
        }
    }
    String::from_utf8_lossy(name).into_owned()
}

/// One line of the ref summary `fetch` and `push` print.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefChange {
    /// ` ` fast-forward, `+` forced, `*` new, `-` deleted, `!` rejected, `=` up to date.
    pub flag: char,
    /// `1234567..89abcde`, `[new branch]`, `[rejected]`, …
    pub summary: String,
    /// The remote name, shortened.
    pub from: String,
    /// The local name, shortened.
    pub to: String,
    /// A parenthesised reason, with its leading space, or empty.
    pub note: String,
}

impl RefChange {
    /// Render for `fetch`, with the source column padded to `width`.
    pub fn fetch_line(&self, width: usize) -> String {
        // Git sets a parenthesised note off with two spaces when fetching (one when pushing).
        let separator = if self.note.is_empty() { "" } else { " " };
        format!(
            " {} {:<17} {:<width$} -> {}{}{}",
            self.flag, self.summary, self.from, self.to, separator, self.note
        )
    }

    /// Render for `push`, which does not pad the source.
    pub fn push_line(&self) -> String {
        if self.from.is_empty() {
            return format!(
                " {} {:<17} {}{}",
                self.flag, self.summary, self.to, self.note
            );
        }
        format!(
            " {} {:<17} {} -> {}{}",
            self.flag, self.summary, self.from, self.to, self.note
        )
    }
}

/// How tags are fetched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TagMode {
    /// Follow tags that point at fetched history.
    #[default]
    Auto,
    /// Fetch every tag (`--tags`).
    All,
    /// Fetch none (`--no-tags`).
    None,
}

/// What a fetch should do.
#[derive(Clone, Debug, Default)]
pub struct FetchRequest {
    /// Refspecs given on the command line; empty means the remote's configured ones.
    pub refspecs: Vec<Refspec>,
    /// Tag handling.
    pub tags: TagMode,
    /// Delete tracking refs whose remote ref is gone.
    pub prune: bool,
    /// Treat every update as forced.
    pub force: bool,
    /// Report what would happen and change nothing.
    pub dry_run: bool,
    /// Write `FETCH_HEAD`.
    pub write_fetch_head: bool,
    /// Record the new refs in `packed-refs` with no reflog, as `clone` does.
    pub pack_refs: bool,
    /// The command named in reflog lines (`pull`); empty means `fetch <remote>`.
    pub action: String,
}

/// What a fetch did.
#[derive(Clone, Debug)]
pub struct FetchReport {
    /// The URL as configured, for `From <url>`.
    pub url: String,
    /// The lines to print, in order, with the column width for the source names.
    pub changes: Vec<RefChange>,
    /// Width of the source column.
    pub width: usize,
    /// Number of refs Git would call rejected.
    pub rejected: usize,
    /// The remote's advertisement, for callers that need `HEAD`.
    pub advertisement: RefAdvertisement,
    /// `FETCH_HEAD` entries: object, whether it is meant for merging, description.
    pub fetch_head: Vec<(ObjectId, bool, String)>,
}

struct Mapping {
    src: BString,
    dst: Option<BString>,
    oid: ObjectId,
    force: bool,
    for_merge: bool,
    /// Whether the ref gets a line in `FETCH_HEAD`.
    listed: bool,
}

impl Repository {
    /// Names of the configured remotes, in the order they appear.
    pub fn remote_names(&self) -> Vec<BString> {
        let mut names: Vec<BString> = Vec::new();
        for entry in self.config().entries() {
            if entry.section.eq_ignore_ascii_case(b"remote")
                && let Some(sub) = &entry.subsection
                && !names.contains(sub)
            {
                names.push(sub.clone());
            }
        }
        names
    }

    /// The configuration of a remote, or `None` when there is no such remote.
    pub fn remote_config(&self, name: &str) -> Result<Option<RemoteConfig>> {
        if !self
            .remote_names()
            .iter()
            .any(|n| n.as_slice() == name.as_bytes())
        {
            return Ok(None);
        }
        let all = |key: &str| -> Result<Vec<BString>> {
            Ok(self
                .config()
                .get_all(BStr::new(format!("remote.{name}.{key}").as_bytes()))?
                .into_iter()
                .map(|v| v.to_owned())
                .collect())
        };
        let mut fetch = Vec::new();
        for spec in all("fetch")? {
            fetch.push(Refspec::parse(BStr::new(&spec[..]))?);
        }
        Ok(Some(RemoteConfig {
            name: BString::from(name),
            urls: all("url")?,
            push_urls: all("pushurl")?,
            fetch,
            push: all("push")?,
        }))
    }

    /// The remote and merge ref configured for a branch (`branch.<b>.remote` / `.merge`).
    pub fn branch_upstream(&self, branch: &str) -> Result<Option<(String, String)>> {
        let get = |key: &str| -> Result<Option<String>> {
            Ok(self
                .config()
                .get(BStr::new(format!("branch.{branch}.{key}").as_bytes()))?
                .map(|v| v.to_string()))
        };
        Ok(match (get("remote")?, get("merge")?) {
            (Some(remote), Some(merge)) => Some((remote, merge)),
            _ => None,
        })
    }

    /// Local ref tips (peeled to commits) that tell the remote what we already have.
    fn local_tips(&self) -> Vec<ObjectId> {
        let mut tips = Vec::new();
        if let Ok(all) = self.refs().list(b"refs/") {
            for (_, target) in all {
                if let Ok(oid) = target.oid()
                    && self.odb().contains(&oid)
                    && !tips.contains(&oid)
                {
                    tips.push(oid);
                }
            }
        }
        if let Ok(Some(head)) = self.head().map(|h| h.oid())
            && !tips.contains(&head)
        {
            tips.push(head);
        }
        tips
    }

    pub(crate) fn store_downloaded(&mut self, pack: &[u8]) -> Result<()> {
        if !pack.is_empty() {
            let format = self.format();
            crate::remote::sideband::store_pack_bytes(pack, self.odb_mut(), format)?;
        }
        Ok(())
    }

    /// Walk everything reachable from `starts` and fail if any object is absent.
    ///
    /// `known` are local tips whose closure is already trusted (git's `--not --all`); shallow
    /// boundaries end the commit walk, and gitlinks are not followed.
    pub(crate) fn ensure_connected(
        &self,
        starts: &[ObjectId],
        known: &[ObjectId],
        url: &str,
    ) -> Result<()> {
        let odb = self.odb();
        let format = self.format();
        let missing = || Error::Message(format!("{url} did not send all necessary objects\n"));
        let mut seen: std::collections::HashSet<ObjectId> = known.iter().copied().collect();
        let mut stack = Vec::new();
        for start in starts {
            if seen.insert(*start) {
                stack.push(*start);
            }
        }
        while let Some(oid) = stack.pop() {
            if !odb.contains(&oid) {
                return Err(missing());
            }
            let object = odb.read(&oid)?;
            let mut next = Vec::new();
            match object.parse_as(format)? {
                crate::object::AnyObject::Blob(_) => {}
                crate::object::AnyObject::Tag(tag) => next.push(tag.object),
                crate::object::AnyObject::Commit(commit) => {
                    next.push(commit.tree);
                    next.extend(odb.visible_parents(&oid, &commit));
                }
                crate::object::AnyObject::Tree(tree) => {
                    for entry in &tree.entries {
                        if entry.mode != crate::object::EntryMode::Commit {
                            next.push(entry.oid);
                        }
                    }
                }
            }
            for child in next {
                if seen.insert(child) {
                    stack.push(child);
                }
            }
        }
        Ok(())
    }

    /// Fetch from `remote` (a configured name or a URL).
    pub fn fetch_remote(&mut self, remote: &str, request: &FetchRequest) -> Result<FetchReport> {
        let config = self.remote_config(remote)?;
        let (label, url_text, configured) = match &config {
            Some(config) => {
                let url = config
                    .urls
                    .first()
                    .ok_or_else(|| Error::Message(format!("remote '{remote}' has no url")))?;
                (remote.to_string(), url.to_string(), config.fetch.clone())
            }
            None => (remote.to_string(), remote.to_string(), Vec::new()),
        };
        let url = RemoteUrl::parse(BStr::new(url_text.as_bytes()))?;
        if request.write_fetch_head && !request.dry_run {
            // Git empties FETCH_HEAD before it talks to the remote, so a fetch that fails (an
            // unreachable or refusing remote) leaves it empty, not stale.
            let path = self.git_dir().join("FETCH_HEAD");
            std::fs::write(&path, b"").map_err(|e| Error::IoAtPath {
                path: BString::from(path.to_string_lossy().as_bytes()),
                source: e,
            })?;
        }
        let mut transport = open_transport(&url)?;
        transport.configure_credentials(crate::remote::credential::CredentialSettings::for_url(
            self.config(),
            &url,
        )?);
        let advertisement = transport.connect(&url, self.format())?;

        let explicit = !request.refspecs.is_empty();
        let specs: Vec<Refspec> = if explicit {
            request.refspecs.clone()
        } else {
            configured.clone()
        };
        let upstream = match self.head()?.branch() {
            Some(branch) => self.branch_upstream(&branch.short_name().to_string())?,
            None => None,
        };

        let advertised: Vec<&AdvertisedRef> = advertisement
            .refs
            .iter()
            .filter(|r| !r.name.ends_with(b"^{}") && !r.oid.is_zero())
            .collect();
        let find_remote = |short: &[u8]| -> Option<&AdvertisedRef> {
            let mut candidates: Vec<Vec<u8>> = vec![short.to_vec()];
            for prefix in ["refs/", "refs/tags/", "refs/heads/", "refs/remotes/"] {
                let mut name = prefix.as_bytes().to_vec();
                name.extend_from_slice(short);
                candidates.push(name);
            }
            candidates.iter().find_map(|c| {
                advertised
                    .iter()
                    .copied()
                    .find(|r| r.name.as_slice() == c.as_slice())
            })
        };

        // Which remote refs go where.
        let mut mappings: Vec<Mapping> = Vec::new();
        for spec in &specs {
            if spec.is_pattern() {
                for r in advertised.iter().filter(|r| r.name.as_slice() != b"HEAD") {
                    if let Some(dst) = spec.map(&r.name) {
                        mappings.push(Mapping {
                            src: r.name.clone(),
                            dst: Some(dst),
                            oid: r.oid,
                            force: spec.force || request.force,
                            for_merge: !explicit
                                && upstream.as_ref().is_some_and(|(rem, merge)| {
                                    rem == &label && merge.as_bytes() == r.name.as_slice()
                                }),
                            listed: true,
                        });
                    }
                }
            } else if let Some(r) = find_remote(&spec.src) {
                mappings.push(Mapping {
                    src: r.name.clone(),
                    dst: (!spec.dst.is_empty()).then(|| {
                        let dst = spec.dst.clone();
                        if dst.starts_with(b"refs/") {
                            dst
                        } else {
                            let mut full = BString::from("refs/heads/");
                            full.extend_from_slice(&dst);
                            full
                        }
                    }),
                    oid: r.oid,
                    force: spec.force || request.force,
                    for_merge: true,
                    listed: true,
                });
                // An explicit `fetch origin main` also refreshes the branch's tracking ref when
                // the remote's own refspecs map it.
                if spec.dst.is_empty() {
                    for conf in &configured {
                        if let Some(dst) = conf.map(&r.name)
                            && !dst.is_empty()
                        {
                            mappings.push(Mapping {
                                src: r.name.clone(),
                                dst: Some(dst),
                                oid: r.oid,
                                force: conf.force || request.force,
                                for_merge: false,
                                listed: false,
                            });
                        }
                    }
                }
            } else {
                return Err(Error::Message(format!(
                    "couldn't find remote ref {}",
                    spec.src
                )));
            }
        }
        if specs.is_empty() {
            // `fetch <url>` with no refspec fetches HEAD into FETCH_HEAD.
            if config.is_none() && !advertised.iter().any(|r| r.name.as_slice() == b"HEAD") {
                return Err(Error::Message("couldn't find remote ref HEAD".to_string()));
            }
            if let Some(head) = advertised.iter().find(|r| r.name.as_slice() == b"HEAD") {
                mappings.push(Mapping {
                    src: head.name.clone(),
                    dst: None,
                    oid: head.oid,
                    force: false,
                    for_merge: true,
                    listed: true,
                });
            }
        }
        if request.tags == TagMode::All {
            for r in advertised
                .iter()
                .filter(|r| r.name.starts_with(b"refs/tags/"))
            {
                if !mappings.iter().any(|m| m.src == r.name) {
                    mappings.push(Mapping {
                        src: r.name.clone(),
                        dst: Some(r.name.clone()),
                        oid: r.oid,
                        force: false,
                        for_merge: false,
                        listed: true,
                    });
                }
            }
        }

        // Download what the head refs need.
        let tips = self.local_tips();
        let wants: Vec<ObjectId> = {
            let mut wants = Vec::new();
            for m in &mappings {
                if !self.odb().contains(&m.oid) && !wants.contains(&m.oid) {
                    wants.push(m.oid);
                }
            }
            wants
        };
        if !wants.is_empty() && !request.dry_run {
            let mut pack = Vec::new();
            transport.fetch(&wants, &tips, &mut pack)?;
            self.store_downloaded(&pack)?;
        }

        // Tags that point at history we now have follow along.
        if request.tags == TagMode::Auto
            && !request.dry_run
            && (configured
                .iter()
                .any(|s| s.is_pattern() && !s.dst.is_empty())
                && !explicit)
        {
            let mut tag_wants = Vec::new();
            let mut tag_maps = Vec::new();
            for r in advertised
                .iter()
                .filter(|r| r.name.starts_with(b"refs/tags/"))
            {
                if mappings.iter().any(|m| m.src == r.name) {
                    continue;
                }
                let target = r.peeled.unwrap_or(r.oid);
                if !self.odb().contains(&target) {
                    continue;
                }
                if !self.odb().contains(&r.oid) {
                    tag_wants.push(r.oid);
                }
                tag_maps.push(Mapping {
                    src: r.name.clone(),
                    dst: Some(r.name.clone()),
                    oid: r.oid,
                    force: false,
                    for_merge: false,
                    listed: false,
                });
            }
            if !tag_wants.is_empty() {
                let mut pack = Vec::new();
                transport.fetch(&tag_wants, &tips, &mut pack)?;
                self.store_downloaded(&pack)?;
            }
            mappings.extend(tag_maps);
        }
        let _ = transport.disconnect();

        // Like git's `check_connected`: every advertised tip we are about to point a ref at must
        // be complete in the odb before any ref moves. A server that sent a short pack fails here,
        // with no ref written.
        if !request.dry_run {
            let starts: Vec<ObjectId> = mappings.iter().map(|m| m.oid).collect();
            self.ensure_connected(&starts, &tips, &display_remote_url(&url_text))?;
        }

        // Width of the source column: the longest remote name among refs that map somewhere.
        let width = mappings
            .iter()
            .filter(|m| m.dst.is_some())
            .map(|m| short_name(&m.src).chars().count())
            .max()
            .unwrap_or(0)
            .max(10);

        let who = self.committer_signature().ok();
        let mut changes = Vec::new();
        let mut rejected = 0;
        let mut fetch_head = Vec::new();
        let mut packed_new: Vec<(RefName, ObjectId)> = Vec::new();

        if request.prune && !request.dry_run {
            for spec in configured
                .iter()
                .filter(|s| s.is_pattern() && !s.dst.is_empty())
            {
                let star = spec.dst.find_byte(b'*').unwrap_or(spec.dst.len());
                let prefix = spec.dst[..star].to_vec();
                for (name, _) in self.refs().list(&prefix)? {
                    let still_there = advertised.iter().any(|r| {
                        spec.map(&r.name)
                            .as_ref()
                            .is_some_and(|d| d.as_slice() == name.as_bytes())
                    });
                    if !still_there && !name.as_bytes().ends_with(b"/HEAD") {
                        let old = self.refs().resolve_oid(&name).ok();
                        self.refs().delete(&name)?;
                        let mut from = short_name(name.as_bytes());
                        from = from.to_string();
                        let _ = old;
                        changes.push(RefChange {
                            flag: '-',
                            summary: "[deleted]".to_string(),
                            from: "(none)".to_string(),
                            to: from,
                            note: String::new(),
                        });
                    }
                }
            }
        }

        for m in &mappings {
            let description = fetch_head_description(&m.src, &display_remote_url(&url_text));
            if m.listed {
                fetch_head.push((m.oid, m.for_merge, description));
            }
            let Some(dst) = &m.dst else {
                let what = if m.src.starts_with(b"refs/tags/") {
                    "tag"
                } else if m.src.starts_with(b"refs/heads/") {
                    "branch"
                } else {
                    "ref"
                };
                changes.push(RefChange {
                    flag: '*',
                    summary: what.to_string(),
                    from: short_name(&m.src),
                    to: "FETCH_HEAD".to_string(),
                    note: String::new(),
                });
                continue;
            };
            let name = RefName::from_bytes(dst)?;
            let old = self.refs().try_read(&name)?.and_then(|t| t.oid().ok());
            if old == Some(m.oid) {
                continue;
            }
            let is_tag = dst.starts_with(b"refs/tags/");
            let from = short_name(&m.src);
            let to = short_name(dst);
            let abbrev = |oid: &ObjectId| oid.to_string()[..7].to_string();
            let (flag, summary, note, reflog) = match old {
                None => {
                    let (what, msg) = if is_tag {
                        ("[new tag]", "storing tag")
                    } else if dst.starts_with(b"refs/heads/") || dst.starts_with(b"refs/remotes/") {
                        ("[new branch]", "storing head")
                    } else {
                        ("[new ref]", "storing ref")
                    };
                    ('*', what.to_string(), String::new(), Some(msg))
                }
                Some(old) if is_tag && !m.force => {
                    // A tag that moved is never silently replaced.
                    rejected += 1;
                    let _ = old;
                    (
                        '!',
                        "[rejected]".to_string(),
                        " (would clobber existing tag)".to_string(),
                        None,
                    )
                }
                Some(old) => {
                    let fast_forward = crate::remote::push_is_fast_forward(self.odb(), old, m.oid)
                        .unwrap_or(false);
                    if fast_forward {
                        (
                            ' ',
                            format!("{}..{}", abbrev(&old), abbrev(&m.oid)),
                            String::new(),
                            Some("fast-forward"),
                        )
                    } else if m.force {
                        (
                            '+',
                            format!("{}...{}", abbrev(&old), abbrev(&m.oid)),
                            " (forced update)".to_string(),
                            Some("forced-update"),
                        )
                    } else {
                        rejected += 1;
                        (
                            '!',
                            "[rejected]".to_string(),
                            " (non-fast-forward)".to_string(),
                            None,
                        )
                    }
                }
            };
            changes.push(RefChange {
                flag,
                summary,
                from,
                to,
                note,
            });
            if request.pack_refs && !request.dry_run && flag != '!' {
                packed_new.push((name.clone(), m.oid));
                continue;
            }
            if let (Some(reflog), false, false) = (reflog, request.dry_run, flag == '!') {
                let who = who.clone().ok_or_else(|| {
                    Error::Message("cannot resolve committer identity".to_string())
                })?;
                let reason = if request.action.is_empty() {
                    format!("fetch {label}: {reflog}")
                } else {
                    format!("{}: {reflog}", request.action)
                };
                self.set_ref_logged(&name, m.oid, &who, BStr::new(reason.as_bytes()))?;
            }
        }

        if !packed_new.is_empty() {
            let mut packed = self.refs().packed().clone();
            for (name, oid) in packed_new {
                let peeled = if name.as_bytes().starts_with(b"refs/tags/")
                    && self
                        .odb()
                        .read_header(&oid)
                        .map(|(k, _)| k == ObjectKind::Tag)
                        .unwrap_or(false)
                {
                    self.refs().peel_oid(oid, self.odb()).ok()
                } else {
                    None
                };
                packed.insert(crate::refs::PackedRef {
                    name,
                    target: oid,
                    peeled,
                });
            }
            if packed.traits().is_empty() {
                packed.set_traits(vec![
                    BString::from("peeled"),
                    BString::from("fully-peeled"),
                    BString::from("sorted"),
                ]);
            }
            packed.sort();
            self.refs_mut().write_packed(packed)?;
        }

        // `remote.<name>.followRemoteHEAD` defaults to `create`: learn the remote's default branch
        // the first time, without ever moving it afterwards.
        if config.is_some()
            && !explicit
            && !request.pack_refs
            && !request.dry_run
            && let Some(default) = remote_head_branch(&advertisement)
            && let Some(short) = default.strip_prefix(b"refs/heads/")
        {
            let head_ref = RefName::from_bytes(format!("refs/remotes/{label}/HEAD").as_bytes())?;
            let target = RefName::from_bytes(
                format!("refs/remotes/{label}/{}", String::from_utf8_lossy(short)).as_bytes(),
            )?;
            if !self.refs().exists(&head_ref) && self.refs().exists(&target) {
                let who = self.committer_signature()?;
                let message = crate::refs::ReflogMessage::new(&who, BStr::new("fetch"));
                let message = self.should_log_ref(&head_ref).then_some(message);
                self.refs().set_symbolic(&head_ref, &target, message)?;
            }
        }

        if request.write_fetch_head && !request.dry_run {
            let mut ordered: Vec<&(ObjectId, bool, String)> =
                fetch_head.iter().filter(|e| e.1).collect();
            ordered.extend(fetch_head.iter().filter(|e| !e.1));
            let mut text = String::new();
            for (oid, for_merge, description) in ordered {
                let flag = if *for_merge { "" } else { "not-for-merge" };
                text.push_str(&format!("{oid}\t{flag}\t{description}\n"));
            }
            let path = self.git_dir().join("FETCH_HEAD");
            std::fs::write(&path, text).map_err(|e| Error::IoAtPath {
                path: BString::from(path.to_string_lossy().as_bytes()),
                source: e,
            })?;
        }

        Ok(FetchReport {
            url: url_text,
            changes,
            width,
            rejected,
            advertisement,
            fetch_head,
        })
    }
}

/// A remote URL as `From <url>` and `FETCH_HEAD` show it: no trailing slashes, no `.git`.
pub fn display_remote_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    trimmed.strip_suffix(".git").unwrap_or(trimmed).to_string()
}

fn fetch_head_description(src: &[u8], url: &str) -> String {
    if let Some(branch) = src.strip_prefix(b"refs/heads/") {
        format!("branch '{}' of {url}", String::from_utf8_lossy(branch))
    } else if let Some(tag) = src.strip_prefix(b"refs/tags/") {
        format!("tag '{}' of {url}", String::from_utf8_lossy(tag))
    } else if src == b"HEAD" {
        url.to_string()
    } else {
        format!("'{}' of {url}", String::from_utf8_lossy(src))
    }
}

/// What a push should do.
#[derive(Clone, Debug, Default)]
pub struct PushRequest {
    /// Refspecs to push, already expanded (`main` is `main:main`).
    pub refspecs: Vec<Refspec>,
    /// Push every branch (`--all`).
    pub all: bool,
    /// Push every tag (`--tags`).
    pub tags: bool,
    /// Treat every update as forced.
    pub force: bool,
    /// Delete the named remote refs.
    pub delete: bool,
    /// Change nothing.
    pub dry_run: bool,
    /// Do not run the `pre-push` hook (`--no-verify`).
    pub no_verify: bool,
}

/// What a push did.
#[derive(Clone, Debug)]
pub struct PushReport {
    /// The URL pushed to, as configured.
    pub url: String,
    /// The lines to print.
    pub changes: Vec<RefChange>,
    /// Updates the remote refused or Git refused locally.
    pub rejected: usize,
    /// Whether any rejection was a non-fast-forward.
    pub non_fast_forward: bool,
    /// Whether any rejection was because the remote has commits we have not fetched.
    pub fetch_first: bool,
    /// The remote ref names that were updated, with the new value.
    pub pushed: Vec<(BString, ObjectId)>,
    /// The `pre-push` hook refused: nothing was sent, and `changes` is not to be printed.
    pub hook_refused: bool,
}

impl Repository {
    /// Push to `remote` (a configured name or a URL).
    pub fn push_remote(&mut self, remote: &str, request: &PushRequest) -> Result<PushReport> {
        let config = self.remote_config(remote)?;
        let (url_text, tracking_specs) = match &config {
            Some(config) => {
                let url = config
                    .push_urls
                    .first()
                    .or_else(|| config.urls.first())
                    .ok_or_else(|| Error::Message(format!("remote '{remote}' has no url")))?;
                (url.to_string(), config.fetch.clone())
            }
            None => (remote.to_string(), Vec::new()),
        };
        let url = RemoteUrl::parse(BStr::new(url_text.as_bytes()))?;
        let mut transport = open_transport(&url)?;
        transport.configure_credentials(crate::remote::credential::CredentialSettings::for_url(
            self.config(),
            &url,
        )?);
        let advertisement = transport.connect(&url, self.format())?;
        let remote_value = |name: &[u8]| -> Option<ObjectId> {
            advertisement
                .refs
                .iter()
                .find(|r| r.name.as_slice() == name && !r.name.ends_with(b"^{}"))
                .map(|r| r.oid)
        };

        // Resolve what to send where.
        struct Plan {
            src: String,
            /// The local ref as `pre-push` is told about it: its full name when it is a ref.
            local_ref: String,
            dst: BString,
            new: Option<ObjectId>,
            force: bool,
        }
        let mut plans: Vec<Plan> = Vec::new();
        let mut specs = request.refspecs.clone();
        if request.all {
            for (name, _) in self.refs().list_branches()? {
                specs.push(Refspec {
                    force: false,
                    src: BString::from(name.as_bytes()),
                    dst: BString::from(name.as_bytes()),
                });
            }
        }
        if request.tags {
            for (name, _) in self.refs().list_tags()? {
                specs.push(Refspec {
                    force: false,
                    src: BString::from(name.as_bytes()),
                    dst: BString::from(name.as_bytes()),
                });
            }
        }
        // The matching refspec `:` stands for every local branch the remote has under the same name.
        if !request.delete {
            let mut expanded = Vec::with_capacity(specs.len());
            for spec in specs {
                if !spec.src.is_empty() || !spec.dst.is_empty() {
                    expanded.push(spec);
                    continue;
                }
                for (name, _) in self.refs().list_branches()? {
                    let short = name
                        .as_bytes()
                        .strip_prefix(b"refs/heads/")
                        .unwrap_or(name.as_bytes());
                    let on_remote = format!("refs/heads/{}", short.as_bstr());
                    if remote_value(on_remote.as_bytes()).is_some() {
                        expanded.push(Refspec {
                            force: spec.force,
                            src: BString::from(name.as_bytes()),
                            dst: BString::from(name.as_bytes()),
                        });
                    }
                }
            }
            specs = expanded;
        }
        for spec in &specs {
            if request.delete || spec.src.is_empty() {
                let dst = qualify_remote_ref(&spec.dst, &advertisement, None);
                plans.push(Plan {
                    src: String::new(),
                    local_ref: "(delete)".to_string(),
                    dst,
                    new: None,
                    force: true,
                });
                continue;
            }
            let src_text = spec.src.to_string();
            let oid = crate::revparse::resolve(
                self.format(),
                self.refs(),
                self.odb(),
                BStr::new(&spec.src[..]),
            )
            .map_err(|_| Error::Message(format!("src refspec {src_text} does not match any")))?;
            let src_ref = self
                .refs()
                .resolve_short(BStr::new(&spec.src[..]))
                .ok()
                .map(|r| r.as_bytes().to_vec());
            let dst_short = if spec.dst.is_empty() {
                spec.src.clone()
            } else {
                spec.dst.clone()
            };
            // `HEAD` pushes the current branch under its own name.
            let dst_short = if dst_short.as_slice() == b"HEAD" {
                match self.head()?.branch() {
                    Some(b) => BString::from(b.short_name().as_bytes()),
                    None => {
                        return Err(Error::Message(
                            "You are not currently on a branch.".to_string(),
                        ));
                    }
                }
            } else {
                dst_short
            };
            let dst = qualify_remote_ref(&dst_short, &advertisement, src_ref.as_deref());
            let local_ref = match &src_ref {
                Some(full) if src_text != "HEAD" => String::from_utf8_lossy(full).into_owned(),
                _ => src_text.clone(),
            };
            plans.push(Plan {
                src: src_text,
                local_ref,
                dst,
                new: Some(oid),
                force: spec.force || request.force,
            });
        }

        // Git reports and sends the refs ordered by their destination name.
        plans.sort_by(|a, b| a.dst.cmp(&b.dst));
        let mut changes = Vec::new();
        let mut updates = Vec::new();
        // Which plans `pre-push` is told about: those that will be sent, not the ones already up
        // to date or refused as non-fast-forward.
        let mut offered = vec![true; plans.len()];
        let mut rejected = 0usize;
        let mut non_ff = false;
        let mut fetch_first = false;
        let abbrev = |oid: &ObjectId| oid.to_string()[..7].to_string();
        for (index, plan) in plans.iter().enumerate() {
            let old = remote_value(&plan.dst);
            let to = short_name(&plan.dst);
            match (old, plan.new) {
                (Some(old), Some(new)) if old == new => {
                    offered[index] = false;
                    changes.push(RefChange {
                        flag: '=',
                        summary: "[up to date]".to_string(),
                        from: short_name(plan.src.as_bytes()),
                        to,
                        note: String::new(),
                    });
                }
                (None, Some(new)) => {
                    let what = if plan.dst.starts_with(b"refs/tags/") {
                        "[new tag]"
                    } else {
                        "[new branch]"
                    };
                    changes.push(RefChange {
                        flag: '*',
                        summary: what.to_string(),
                        from: short_name(plan.src.as_bytes()),
                        to,
                        note: String::new(),
                    });
                    updates.push(RefUpdate::create(plan.dst.clone(), new));
                }
                (Some(old), Some(new)) => {
                    let ff = self.odb().contains(&old)
                        && crate::remote::push_is_fast_forward(self.odb(), old, new)
                            .unwrap_or(false);
                    if ff {
                        changes.push(RefChange {
                            flag: ' ',
                            summary: format!("{}..{}", abbrev(&old), abbrev(&new)),
                            from: short_name(plan.src.as_bytes()),
                            to,
                            note: String::new(),
                        });
                        let mut update = RefUpdate::new(plan.dst.clone(), old, new);
                        update.force = plan.force;
                        updates.push(update);
                    } else if plan.force {
                        changes.push(RefChange {
                            flag: '+',
                            summary: format!("{}...{}", abbrev(&old), abbrev(&new)),
                            from: short_name(plan.src.as_bytes()),
                            to,
                            note: " (forced update)".to_string(),
                        });
                        let mut update = RefUpdate::new(plan.dst.clone(), old, new);
                        update.force = true;
                        updates.push(update);
                    } else {
                        offered[index] = false;
                        rejected += 1;
                        non_ff = true;
                        let known = self.odb().contains(&old);
                        fetch_first |= !known;
                        let reason = if known {
                            " (non-fast-forward)"
                        } else {
                            " (fetch first)"
                        };
                        changes.push(RefChange {
                            flag: '!',
                            summary: "[rejected]".to_string(),
                            from: short_name(plan.src.as_bytes()),
                            to,
                            note: reason.to_string(),
                        });
                    }
                }
                (Some(old), None) => {
                    changes.push(RefChange {
                        flag: '-',
                        summary: "[deleted]".to_string(),
                        from: String::new(),
                        to,
                        note: String::new(),
                    });
                    updates.push(RefUpdate::delete(plan.dst.clone(), old));
                }
                (None, None) => {
                    return Err(Error::Message(format!(
                        "unable to delete '{}': remote ref does not exist",
                        short_name(&plan.dst)
                    )));
                }
            }
        }

        // `pre-push` sees every ref that is about to change (not the ones already up to date),
        // after the remote has been asked what it has and before anything is sent.
        let mut hook_refused = false;
        if !request.no_verify {
            let zero = ObjectId::zero(self.format());
            let mut lines = Vec::new();
            for (plan, _) in plans.iter().zip(&offered).filter(|(_, offered)| **offered) {
                let old = remote_value(&plan.dst);
                lines.extend_from_slice(
                    format!(
                        "{} {} {} {}\n",
                        plan.local_ref,
                        plan.new.unwrap_or(zero),
                        plan.dst,
                        old.unwrap_or(zero)
                    )
                    .as_bytes(),
                );
            }
            hook_refused = crate::hooks::Hook::new(self, "pre-push")
                .arg(remote)
                .arg(&url_text)
                .stdin(lines)
                .keep_stdout()
                .run()
                .is_some_and(|code| code != 0);
        }

        let mut pushed = Vec::new();
        if hook_refused {
            updates.clear();
        }
        if !updates.is_empty() && !request.dry_run {
            let outcome = crate::remote::push(
                transport.as_mut(),
                &advertisement,
                &updates,
                self.odb(),
                self.refs(),
            )?;
            for status in &outcome.rejected {
                rejected += 1;
                // Point the printed line at the failure the remote gave.
                if let Some(change) = changes.iter_mut().find(|c| {
                    plans
                        .iter()
                        .any(|p| p.dst == status.name && short_name(&p.dst) == c.to)
                }) {
                    change.flag = '!';
                    change.summary = "[remote rejected]".to_string();
                    change.note = format!(" ({})", status.message);
                }
            }
            for update in &outcome.updated {
                pushed.push((update.name.clone(), update.new));
            }
        }
        let _ = transport.disconnect();

        // A successful push moves the matching remote-tracking ref, as if it had been fetched.
        if !request.dry_run {
            for (name, new) in &pushed {
                for spec in &tracking_specs {
                    if let Some(local) = spec.map(name) {
                        if local.is_empty() {
                            continue;
                        }
                        let tracking = RefName::from_bytes(&local)?;
                        if new.is_zero() {
                            let _ = self.refs().delete(&tracking);
                        } else {
                            let who = self.committer_signature()?;
                            self.set_ref_logged(
                                &tracking,
                                *new,
                                &who,
                                BStr::new("update by push"),
                            )?;
                        }
                    }
                }
            }
        }

        Ok(PushReport {
            url: url_text,
            changes,
            rejected,
            non_fast_forward: non_ff,
            fetch_first,
            pushed,
            hook_refused,
        })
    }
}

/// The full name on the remote for a push destination written as `main`, `v1` or `refs/…`.
fn qualify_remote_ref(
    short: &[u8],
    advertisement: &RefAdvertisement,
    src_ref: Option<&[u8]>,
) -> BString {
    if short.starts_with(b"refs/") {
        return BString::from(short);
    }
    for prefix in ["refs/heads/", "refs/tags/"] {
        let mut name = BString::from(prefix);
        name.extend_from_slice(short);
        if advertisement.refs.iter().any(|r| r.name == name) {
            return name;
        }
    }
    if src_ref.is_some_and(|s| s.starts_with(b"refs/tags/")) {
        let mut name = BString::from("refs/tags/");
        name.extend_from_slice(short);
        return name;
    }
    let mut name = BString::from("refs/heads/");
    name.extend_from_slice(short);
    name
}

/// What a clone should do.
#[derive(Clone, Debug, Default)]
pub struct CloneRequest {
    /// Create a bare repository.
    pub bare: bool,
    /// Mirror every ref.
    pub mirror: bool,
    /// The branch to check out instead of the remote's `HEAD`.
    pub branch: Option<String>,
    /// Name of the remote to create.
    pub origin: String,
    /// Fetch tags (default) or none.
    pub no_tags: bool,
}

/// The branch the remote's `HEAD` names, from the `symref` capability or by matching ids.
pub fn remote_head_branch(advertisement: &RefAdvertisement) -> Option<BString> {
    if let Some(value) = advertisement.capabilities.value(b"symref") {
        let text = value.to_string();
        // May be a space-separated list inside one value; take the HEAD entry.
        for part in text.split_whitespace() {
            if let Some(target) = part.strip_prefix("HEAD:") {
                return Some(BString::from(target));
            }
        }
    }
    let head = advertisement
        .refs
        .iter()
        .find(|r| r.name.as_slice() == b"HEAD")?;
    let mut candidates: Vec<&AdvertisedRef> = advertisement
        .refs
        .iter()
        .filter(|r| r.name.starts_with(b"refs/heads/") && r.oid == head.oid)
        .collect();
    candidates.sort_by_key(|r| {
        (
            r.name.as_slice() != b"refs/heads/main",
            r.name.as_slice() != b"refs/heads/master",
        )
    });
    candidates.first().map(|r| r.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;
    use std::process::{Command, Stdio};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8(out.stdout).unwrap()
    }

    /// Pack the closure of HEAD in `src`, leaving out every object id in `omit`.
    fn pack_without(src: &Path, omit: &[&str]) -> Vec<u8> {
        let listing = git(src, &["rev-list", "--objects", "HEAD"]);
        let ids: Vec<&str> = listing
            .lines()
            .filter_map(|l| l.split(' ').next())
            .filter(|id| !omit.contains(id))
            .collect();
        let mut child = Command::new("git")
            .args(["pack-objects", "--stdout"])
            .current_dir(src)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for id in &ids {
            writeln!(stdin, "{id}").unwrap();
        }
        drop(stdin);
        child.wait_with_output().unwrap().stdout
    }

    #[test]
    fn short_pack_fails_the_connectivity_check_and_a_full_pack_passes() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        git(src.path(), &["init", "-q"]);
        std::fs::create_dir(src.path().join("d")).unwrap();
        std::fs::write(src.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(src.path().join("d/b.txt"), "beta\n").unwrap();
        git(src.path(), &["add", "."]);
        git(src.path(), &["commit", "-q", "-m", "one"]);
        let head = git(src.path(), &["rev-parse", "HEAD"]);
        let blob = git(src.path(), &["rev-parse", "HEAD:d/b.txt"]);
        let head = ObjectId::from_hex(crate::ObjectFormat::Sha1, BStr::new(head.trim())).unwrap();

        git(dst.path(), &["init", "-q"]);
        let mut repo = Repository::discover(dst.path()).unwrap();

        repo.store_downloaded(&pack_without(src.path(), &[blob.trim()]))
            .unwrap();
        let err = repo
            .ensure_connected(&[head], &[], "https://example.com/r.git")
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "https://example.com/r.git did not send all necessary objects\n"
        );

        repo.store_downloaded(&pack_without(src.path(), &[]))
            .unwrap();
        repo.ensure_connected(&[head], &[], "https://example.com/r.git")
            .unwrap();
    }
}
