//! Phase 11 — fetch: negotiation on top of a [`Transport`], and the refs it moves afterwards.
//!
//! [`Transport::fetch`] knows how to ask a server for objects and write a pack. What it does not
//! know is *which* objects to ask for, how many rounds of `have` to send before giving up and saying
//! `done`, or what to do with the pack once it has arrived. That is this module, and it is the same
//! code for HTTP and SSH because the transport seam has already absorbed the difference.
//!
//! # The negotiation, in the shape it must have
//!
//! ```text
//! client → want <oid> <capabilities>
//!          want <oid>            (one per wanted tip)
//!          flush
//!          have <oid>            (up to 32 per round, newest first)
//!          flush
//! server ← ACK <oid> common | ACK <oid> ready | NAK
//! client → have … / flush      (repeat while the server has not said ready)
//!          done
//! server ← ACK <oid> | NAK
//!          <packfile>
//! ```
//!
//! The first `want` line carries the capabilities, space-separated after a NUL — not after a space.
//! That NUL is the whole reason the negotiation is written with [`crate::remote::PktLine`] rather
//! than with `write!`, and getting it wrong produces a server that reads `multi_ack` as part of an
//! object id.
//!
//! # Caveats that bite
//!
//! * **A fetch with nothing wanted is not a fetch.** When the remote is already an ancestor of a
//!   local ref, the correct behaviour is to move the tracking ref and fetch nothing: no connection,
//!   no `want`. An implementation that sent an empty `want` list gets a server error, and one that
//!   sent a `want` for an object it already had gets a full pack.
//! * **`have` lines are hints, not a set difference.** The server uses them to find the common
//!   ancestor; sending the full local history is legal and slow, and sending none is also legal and
//!   produces a pack containing everything. The strategy here is bounded rounds, newest commits
//!   first, which is what makes an incremental fetch small.
//! * **The pack is indexed before the refs move, and the refs move before anything is checked out.**
//!   That order is not negotiable: a crash between the index and the ref update leaves unreferenced
//!   objects, which is harmless, while the reverse order leaves refs pointing at objects that are not
//!   there, which is corruption. `fetch` never checks out at all — that is `pull`'s job and a
//!   different batch.
//! * **A thin pack must be completed against the local object database before it is indexed.** The
//!   server sends deltas whose bases it knows the client already has; [`crate::pack::fix_thin_pack`]
//!   is what turns that into a self-contained pack, and calling [`crate::pack::index_pack`] without
//!   it fails on the first delta whose base is missing.
//! * **Nothing here writes to the working tree or to a remote-tracking branch of a remote the user
//!   did not name.** `FetchOutcome` is a data structure describing what happened; applying it is the
//!   caller's decision, because a fetch during a `status` must not move `refs/remotes/origin/main`.
//!
//! # The refspec is deliberately absent
//!
//! Rivet's plan puts refspec parsing in the config batch. Until it exists, `fetch` expresses what to
//! do with the advertisement through [`FetchRefs`]: [`FetchRefs::Tracking`] is the behaviour of
//! `git fetch origin` with default configuration, and [`FetchRefs::Explicit`] is the escape hatch for
//! anything else. A fetch that needs a wildcard refspec is served by the mode; one that needs
//! something stranger waits for the refspec type, and adding it later does not change any signature in
//! this module.

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::ObjectId;
use crate::odb::Odb;
use crate::refs::RefStore;

use crate::remote::transport::Transport;
use crate::remote::url::RemoteUrl;
use crate::remote::{AdvertisedRef, RefAdvertisement};
use crate::object::ObjectKind;
use crate::error::Error;

/// Phase 11 — how many objects to put in one `have` line's round.
///
/// # Caveats
///
/// The value Git uses, and it is a latency-versus-rounds trade rather than a protocol limit: a round
/// costs one round trip, so more per round converges in fewer, and each `have` the server can match
/// ends the negotiation earlier. Sending everything at once is legal and is what a first fetch with
/// no local history degenerates to anyway.
pub const HAVES_PER_ROUND: usize = 32;

/// Phase 11 — how many rounds of `have` to send before sending `done` regardless.
///
/// # Caveats
///
/// **The bound is a strategy limit, not a protocol one** — a client may keep offering until the
/// server says ready. It exists because a server that never finds a common ancestor (a rewritten
/// history, an unrelated repository) will happily answer `NAK` forever, and without a cap the fetch
/// hangs instead of falling back to a full pack. Git uses a similar cap and the effect is the same:
/// the pack gets bigger, and the fetch finishes.
pub const MAX_NEGOTIATION_ROUNDS: usize = 10;

/// Phase 11 — what a fetch is asked to obtain.
///
/// # Caveats
///
/// * **`wants` are tips, not a transitive closure.** Naming a commit asks for everything reachable
///   from it that the server believes the client lacks; naming an object that is not a commit is
///   legal and asks for that one object's closure, which for a blob is itself.
/// * **An empty `wants` is a complete instruction**, not an error: it means "negotiate and discover
///   there is nothing to do", which is how a fetch of an up-to-date remote-tracking ref is expressed
///   without a special case in the caller.
/// * **The `depth`-style shallow options are absent on purpose.** Rivet does not do shallow or
///   partial clones, and a field that existed and was refused would be worse than one that is not
///   there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchSpec {
    /// The object ids the caller wants, in the order they should be requested.
    pub wants: Vec<ObjectId>,
    /// Whether to request `thin-pack`, which lets the server omit bases it believes the client has.
    pub thin: bool,
    /// Whether to request `ofs-delta`, which makes delta bases file offsets rather than object ids.
    pub ofs_delta: bool,
    /// Whether to request `include-tag`, which makes the server add annotated tags that point into
    /// the pack it is sending.
    pub include_tag: bool,
    /// Whether to ask for `side-band-64k`, which multiplexes progress onto channel 2.
    pub sideband: bool,
}

/// Phase 11 — the capabilities a caller permits the negotiation to use.
///
/// # Caveats
///
/// * **A capability is used only if the server advertised it *and* the caller allowed it**, and the
///   intersection is computed in [`negotiate`]. The defaults allow everything Rivet speaks, so a
///   caller that cares about exactly one of them — a test, or a clone that must avoid thin packs —
///   can turn the rest off without building the `want` line by hand.
/// * **`multi_ack_detailed` and `multi_ack` are separate flags because they are separate
///   capabilities** with different replies, and a server that has only the older one still uses the
///   older reply shape. See [`crate::remote::can_use_multi_ack`].
/// * **`no_progress` is not here.** It suppresses a display, and this layer does not draw one; the
///   CLI's decision to show progress belongs to the CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchCapabilities {
    /// Whether `multi_ack` may be used.
    pub multi_ack: bool,
    /// Whether `multi_ack_detailed` may be used, which is preferred when the server has it.
    pub multi_ack_detailed: bool,
    /// Whether `side-band-64k` may be used, which is preferred over `side-band`.
    pub side_band_64k: bool,
    /// Whether the older `side-band` may be used if `side-band-64k` is absent.
    pub side_band: bool,
    /// Whether `thin-pack` may be requested.
    pub thin_pack: bool,
    /// Whether `ofs-delta` may be requested.
    pub ofs_delta: bool,
    /// Whether `include-tag` may be requested.
    pub include_tag: bool,
    /// Whether `agent` may be sent.
    pub agent: bool,
}

impl Default for FetchCapabilities {
    /// Phase 11 — everything Rivet speaks, allowed.
    ///
    /// # Caveats
    ///
    /// **The default is "use what the server offers"**, which is what a user expects from a fetch and
    /// what makes the flags useful only as a restriction. A default that disabled something would
    /// make the common path slow for a reason the caller never stated.
    fn default() -> Self {
        FetchCapabilities {
            side_band_64k: true,
            side_band: true,
            thin_pack: true,
            ofs_delta: true,
            include_tag: true,
            agent: true,
            multi_ack: true,
            multi_ack_detailed: true,
        }
    }
}

/// Phase 11 — what to do with the refs an advertisement offers.
///
/// # Caveats
///
/// * **[`FetchRefs::Tracking`] is `git fetch origin`'s behaviour**: for each `refs/heads/*` the
///   remote advertises, write `refs/remotes/<remote>/<branch>`, and for `refs/tags/*` write the tag
///   as-is. It is the mode the CLI uses when the user names a remote, and it needs the remote's name
///   to build the tracking ref.
/// * **[`FetchRefs::None`] stores the objects and moves no refs**, which is what a caller that
///   intends to inspect or merge manually wants, and what [`FetchOutcome::pack`] alone is for.
/// * **[`FetchRefs::Explicit`] is a list, not a refspec**, for the same reason the module docs give:
///   the wildcard grammar belongs to the config batch. A caller with a real refspec expands it before
///   calling, and the expansion is then visible in [`FetchOutcome`] rather than hidden in a parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchRefs {
    /// Write nothing; the caller reads [`FetchOutcome::pack`] and decides.
    None,
    /// Write `refs/remotes/<remote>/…` for branches and `refs/tags/…` for tags, as `git fetch` does.
    Tracking {
        /// The remote's name, as it appears in `[remote "<name>"]` and in the tracking ref.
        remote: BString,
    },
    /// Write exactly these refs, by name.
    Explicit(Vec<FetchRefUpdate>),
}

/// Phase 11 — one ref the caller asked to be written after a fetch.
///
/// # Caveats
///
/// * **`force` is per-update and defaults to false.** A non-fast-forward update without it is
///   [`crate::error::Error::NonFastForward`], which is the same rule push applies and the reason
///   both live in this module's vocabulary rather than in the transport's.
/// * **`expect` is the value the caller believes the ref currently holds**, and it is checked under
///   the same lock as the write. Passing `None` means "I did not look", which is honest and races; it
///   is what `fetch` does for a tracking ref it intends to overwrite, because a fetch is the thing
///   that decides what the remote has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchRefUpdate {
    /// The full ref name to write, e.g. `refs/remotes/origin/main`.
    pub name: BString,
    /// The object id to point it at.
    pub new: ObjectId,
    /// The value it must currently hold, if the caller checked.
    pub expect: Option<ObjectId>,
    /// Whether a non-fast-forward move is permitted.
    pub force: bool,
}

/// Phase 11 — everything a fetch needs besides a transport.
///
/// # Caveats
///
/// * **`format` must match the repository's**, and a mismatch with the remote is refused before any
///   object is exchanged — see the transport seam's caveat. A caller that left it at the default
///   would fetch SHA-1 objects into a SHA-256 repository and only notice at the first hash check.
/// * **`progress` receives human text and is called on the thread that runs the fetch.** It is not
///   buffered or rate-limited here, so a consumer that writes to a terminal should debounce; a
///   consumer that collects into a `String` will hold every byte the server sent. `None` means the
///   progress channel is read and dropped, which is required — a demultiplexer that did not read
///   channel 2 would deadlock on a full pipe.
/// * **`dry_run` performs the negotiation and downloads nothing**, which the protocol cannot express
///   and this layer implements by not asking: a dry run's `wants` are computed and returned and
///   [`Transport::fetch`] is not called.
pub struct FetchOptions {
    /// The object-format the local repository uses.
    pub format: crate::hash::ObjectFormat,
    /// Which capabilities the negotiation may use.
    pub capabilities: FetchCapabilities,
    /// What to do with the advertisement's refs afterwards.
    pub refs: Option<FetchRefs>,
    /// Where progress text goes; `None` discards it.
    pub progress: Option<Box<dyn FnMut(&[u8])>>,
    /// Whether to negotiate without downloading.
    pub dry_run: bool,
    /// Whether to fix a thin pack and index it, or hand the raw pack to the caller.
    pub index: bool,
}

impl Default for FetchOptions {
    /// Phase 11 — the options a plain `git fetch origin` implies.
    ///
    /// # Caveats
    ///
    /// * **`format` defaults to SHA-1** rather than to anything derived from a repository, because
    ///   this type has no repository. A caller that leaves it alone on a SHA-256 repository gets an
    ///   [`crate::error::Error::ObjectFormatMismatch`] from [`negotiate`] — the loud failure — rather
    ///   than a hash comparison that fails later on an object id of the wrong width.
    /// * **`index` defaults to true**, so a caller that constructs the options and forgets the field
    ///   gets objects in the odb rather than a pack in memory. The in-memory variant is the unusual
    ///   one and is opted into explicitly.
    /// * **`refs` defaults to `None`**, which stores objects and moves nothing. That is the safe
    ///   default — a caller must name a remote before Rivet will write a tracking ref — and it is the
    ///   opposite of what the CLI passes, which is deliberate.
    fn default() -> Self {
        Self {
            format: crate::hash::ObjectFormat::Sha1,
            capabilities: FetchCapabilities::default(),
            refs: None,
            progress: None,
            dry_run: false,
            index: true,
        }
    }
}

impl std::fmt::Debug for FetchOptions {
    /// Phase 11 — a `Debug` that omits the progress sink.
    ///
    /// # Caveats
    ///
    /// A closure has no useful `Debug`, and deriving one would make every struct that embeds
    /// `FetchOptions` fail to derive it too. Printing the other fields keeps a diagnostic useful
    /// without pretending the sink is printable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FetchOptions { format: ")
            .and_then(|_| self.format.fmt(f))
            .and_then(|_| f.write_str(", capabilities: "))
            .and_then(|_| self.capabilities.fmt(f))
            .and_then(|_| f.write_str(", refs: "))
            .and_then(|_| self.refs.fmt(f))
            .and_then(|_| f.write_str(", dry_run: "))
            .and_then(|_| self.dry_run.fmt(f))
            .and_then(|_| f.write_str(", index: "))
            .and_then(|_| self.index.fmt(f))
            .and_then(|_| f.write_str(" }"))
    }
}

/// Phase 11 — what the negotiation decided, before anything was written.
///
/// # Caveats
///
/// * **`common` is the server's answer, not the client's belief.** A server that found no common
///   ancestor says so by sending `NAK` to every round; `common` is then empty and `full_pack` is
///   true, and a caller that shows the user "already up to date" because the want list was small has
///   conflated the two.
/// * **`negotiation_rounds` is how many `have` rounds were sent**, which is the number that explains
///   a slow fetch. It is not the number of round trips, which also includes the advertisement and
///   the pack.
/// * **`used_capabilities` is what was actually put on the first `want` line**, after intersecting
///   with the advertisement. A caller diagnosing "why did this not use side-band" reads it rather
///   than the server's advertisement, because the answer is usually that the caller disabled it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Negotiation {
    /// The wants that were (or would be) sent.
    pub wants: Vec<ObjectId>,
    /// The object ids the server reported as common.
    pub common: Vec<ObjectId>,
    /// Whether the server signalled `ready`, or ran out of rounds.
    pub ready: bool,
    /// How many `have` rounds were sent.
    pub negotiation_rounds: usize,
    /// Whether the server had nothing in common and the pack is the full closure.
    pub full_pack: bool,
    /// The capabilities as they were written on the first `want` line.
    pub used_capabilities: Vec<BString>,
}

/// Phase 11 — the result of a fetch.
///
/// # Caveats
///
/// * **This is the name the shared vocabulary fixes**, and it is the only thing a caller gets back:
///   the negotiation's description, the refs that were written, and — when
///   [`FetchOptions::index`] was off — the raw pack. With `index` on, the pack has been written into
///   the object database and `pack` is `None`, because keeping a second copy in memory for a large
///   fetch is the difference between working and not.
/// * **`pack_checksum` identifies what was stored**, which is what a caller compares against the
///   server's advertisement when it wants evidence the transfer was complete. It is `None` when
///   nothing was fetched, which is not the same as a pack of zero objects.
/// * **`updated` lists the refs that actually moved.** A tracking ref that was already at the right
///   value is not in it, and neither is one the caller declined to write — so a caller that reports
///   "fetched 0 refs" from an empty advertisement is reading it correctly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchOutcome {
    /// What the negotiation decided.
    pub negotiation: Negotiation,
    /// The refs written, with their old and new values.
    pub updated: Vec<FetchRefUpdate>,
    /// The raw pack, present only when the caller asked not to index it.
    pub pack: Option<Vec<u8>>,
    /// The checksum of the pack that was stored, if one was.
    pub pack_checksum: Option<ObjectId>,
    /// The number of objects the pack contained, if it was indexed.
    pub object_count: Option<u32>,
}

/// Phase 11 — run a fetch against a transport.
///
/// # Caveats
///
/// * **The caller has already connected.** `transport` must have had its [`Transport::connect`]
///   called, because the negotiation needs the advertisement's capabilities and the URL's
///   object-format check has already happened there. This function is given the advertisement rather
///   than re-reading it, which also means one connection per fetch rather than two.
/// * **The pack is fixed and indexed before the refs move**, per the module caveat. With
///   [`FetchOptions::index`] on, the objects are in the odb by the time this returns and
///   [`FetchOutcome::pack`] is `None`.
/// * **A `dry_run` returns the negotiation with no pack and moves no refs**, and it does not call
///   [`crate::remote::Transport::fetch`] at all — so it is cheap and it is still a network round trip
///   for the advertisement.
/// * **Progress is drained even when the caller supplied no sink**, because channel 2 is part of the
///   stream and an unread sideband is a deadlock rather than a cosmetic omission.
/// * **`refs` writes go through [`RefStore::update`]**, which is where the fast-forward rule and the
///   `expected` check live. This function does not decide either one itself, so that a fetch and a
///   push cannot disagree about what a non-fast-forward means.
///
/// # Errors
///
/// [`crate::error::Error::Protocol`] for a malformed negotiation or advertisement,
/// [`crate::error::Error::ObjectFormatMismatch`], [`crate::error::Error::NonFastForward`] from a ref
/// update the caller did not force, [`crate::error::Error::ThinPack`] if a thin pack cannot be fixed,
/// and [`crate::error::Error::Transport`] for the network.
pub fn fetch(
    transport: &mut dyn Transport,
    advertisement: &RefAdvertisement,
    spec: &FetchSpec,
    options: &FetchOptions,
    odb: &Odb,
    refs: &RefStore,
) -> Result<FetchOutcome> {
    todo!()
}

/// Phase 11 — decide which capabilities to use and which objects to want.
///
/// # Caveats
///
/// * **This is the pure half of a fetch** and it is separate so that it can be tested without a
///   server: the intersection of [`FetchCapabilities`] with the advertisement, the deduplication of
///   the want list, and the ordering of the haves are all decided here and none of them touch a
///   socket.
/// * **A want that the advertisement does not contain is an error**, not a silent omission. The
///   server would refuse it anyway, but the refusal arrives as an `ERR` line after the pack has
///   started, which is a worse message than the one this can produce with both ids in hand.
/// * **Duplicates are removed and order is preserved**, because the server is allowed to send an
///   `ERR` for a repeated want and a caller assembling wants from two refs hits that immediately.
/// * **The haves are taken from the local refs**, newest first, using commit dates where they are
///   available — an unsorted have list makes the server's ancestor walk slow rather than wrong, which
///   is why this is a caveat and not a rule.
///
/// # Errors
///
/// [`crate::error::Error::Protocol`] for a want the advertisement does not offer, and
/// [`crate::error::Error::ObjectFormatMismatch`] if the advertisement's format is not the one the
/// caller is fetching into.
pub fn negotiate(
    advertisement: &RefAdvertisement,
    spec: &FetchSpec,
    capabilities: &FetchCapabilities,
    odb: &Odb,
    refs: &RefStore,
    format: crate::hash::ObjectFormat,
) -> Result<Negotiation> {
    let advertised_ids: Vec<ObjectId> = advertisement
    .refs
    .iter()
    .map(|r| r.oid)
    .collect();
    if !advertised_ids.iter().any(|id| id.format() == format) {
        return Err(Error::ObjectFormatMismatch {
            expected: format,
            found: format,
        });
    }
    let mut wants = Vec::new();
    for id in &spec.wants {
        if !advertised_ids.iter().any(|a| a == id) {
            return Err(Error::Protocol(format!("want {id} not advertised")));
        }
        if !wants.iter().any(|w| w == id) {
            wants.push(*id);
        }
    }
    let haves = crate::fetch::local_haves(odb, refs)?;
    let mut used = Vec::new();
    if capabilities.multi_ack {
        used.push(BString::from("multi_ack"));
    }
    if capabilities.multi_ack_detailed {
        used.push(BString::from("multi_ack_detailed"));
    }
    if capabilities.side_band_64k {
        used.push(BString::from("side-band-64k"));
    }
    if capabilities.side_band {
        used.push(BString::from("side-band"));
    }
    if capabilities.thin_pack {
        used.push(BString::from("thin-pack"));
    }
    if capabilities.ofs_delta {
        used.push(BString::from("ofs-delta"));
    }
    if capabilities.include_tag {
        used.push(BString::from("include-tag"));
    }
    if capabilities.agent {
        used.push(BString::from("agent"));
    }
    Ok(Negotiation {
        wants,
        common: Vec::new(),
        ready: true,
        negotiation_rounds: 0,
        full_pack: false,
        used_capabilities: used,
    })
}

/// Phase 11 — turn an advertisement into the wants a fetch of a remote-tracking ref needs.
///
/// # Caveats
///
/// * **Every advertised branch head is wanted, not just the default one**, because `git fetch origin`
///   means "bring me every branch". A caller that wants one branch passes a [`FetchSpec`] with one
///   want and does not call this.
/// * **A peeled tag contributes its peeled object id, not the tag object**, when the tag is
///   annotated and `include_tag` was not requested — asking for the tag object and getting its
///   referent is the difference between a fetch that terminates and one that asks for an object the
///   server will not send under a thin pack.
/// * **A ref the server advertises with an all-zero id is skipped**, which is how a deletion is
///   announced and is not something to want.
pub fn wants_for_tracking(advertisement: &RefAdvertisement) -> Vec<ObjectId> {
    todo!()
}

/// Phase 11 — compute the local refs to offer as `have` lines, newest first.
///
/// # Caveats
///
/// * **The result is a hint set and may be truncated**; a caller that sends all of it on a large
///   repository makes the first round enormous. [`HAVES_PER_ROUND`] is the consumer's bound, and this
///   function returns the whole ordered list so that the round loop can take slices.
/// * **Only commits are offered.** A blob's id as a `have` tells the server nothing about ancestry
///   and costs a line; Git filters the same way, and the filter is by object type, not by ref name.
/// * **A ref pointing at an object the local odb does not have is skipped**, which can happen after
///   a filesystem-level restore and would otherwise make the server's reply reference an object the
///   client cannot read.
pub fn local_haves(odb: &Odb, refs: &RefStore) -> Result<Vec<ObjectId>> {
    let mut haves = Vec::new();
    for oid in odb.oids().expect("reading local oids") {
        if !odb.contains(&oid) {
            continue;
        }
        let (kind, _) = odb.read_header(&oid).expect("reading object header");
        if kind == ObjectKind::Commit {
            haves.push(oid);
        }
    }
    haves.reverse();
    Ok(haves)
}

/// Phase 11 — write the refs an advertisement implies, for a tracking fetch.
///
/// # Caveats
///
/// * **The ref name is built from the remote's name**, `refs/remotes/<remote>/<short>`, exactly as
///   `git fetch` builds it. A remote named `origin` therefore writes `refs/remotes/origin/main`,
///   and a caller that wanted `refs/heads/main` updated has asked for the wrong mode.
/// * **Tags are written to `refs/tags/<name>` and are never forced by this function.** A tag object
///   is immutable by convention; overwriting one silently is how a fetch destroys a release, and the
///   correct behaviour when a remote moves a tag is to report it rather than to follow. A caller that
///   wants the new value passes it explicitly through [`FetchRefs::Explicit`] with `force`.
/// * **Nothing is written for a ref already at the right value**, so a fetch that changed nothing
///   performs no writes and takes no locks — which matters on a shared repository more than the saved
///   syscall suggests.
/// * **The head's symbolic target is not written as a symbolic ref here.** Setting
///   `refs/remotes/<remote>/HEAD` is a separate, optional step in Git (`git remote set-head`), and
///   conflating it with a fetch would make a fetch change which branch `origin/HEAD` names.
///
/// # Errors
///
/// [`crate::error::Error::NonFastForward`] from [`RefStore::update`] for a branch update that would
/// lose a commit, and [`crate::error::Error::InvalidRefName`] for a ref name the store cannot write.
pub fn write_tracking_refs(
    advertisement: &RefAdvertisement,
    remote: &BStr,
    refs: &RefStore,
) -> Result<Vec<FetchRefUpdate>> {
    todo!()
}

/// Phase 11 — the local ref name a remote branch is tracked under.
///
/// # Caveats
///
/// * **The input is the remote's ref name, e.g. `refs/heads/main`, and the output is
///   `refs/remotes/origin/main`.** A caller that passed the short name gets the short name
///   embedded, which is a bug the shape of the function cannot prevent — hence this caveat rather
///   than a fix.
/// * **A ref outside `refs/heads/` and `refs/tags/` returns `None`**, because Git does not track
///   `refs/pull/*` and `refs/notes/*` by default and inventing a mapping for them would write refs
///   the user did not ask for.
/// * **The remote's name is not validated for path traversal here**; it comes from the config, and
///   [`crate::remote::RemoteUrl::parse`] is where a hostile remote name would be caught. This
///   function assumes a name of the shape the config allows.
pub fn tracking_ref_name(remote: &BStr, advertised: &BStr) -> Option<BString> {
    let prefix = advertised.strip_prefix(b"refs/heads/").or_else(|| advertised.strip_prefix(b"refs/tags/"))?;
    Some(BString::from(format!("refs/remotes/{}/{:?}", remote, prefix)))
}

/// Phase 11 — the remote an advertisement's refs would be tracked under, if the URL names one.
///
/// # Caveats
///
/// * **This is a guess used for display**, derived from the URL's last path component, and it is not
///   how the config names a remote. A caller that knows the remote's name passes it; a caller that
///   only has a URL gets `origin`-style text that may not match any `[remote]` section, and the
///   documentation says so rather than letting a caller believe it is authoritative.
/// * **A URL ending in `.git` has that stripped**, matching Git's default `origin` derivation, but a
///   URL with no path component returns `None` rather than an empty name.
pub fn guess_remote_name(url: &RemoteUrl) -> Option<BString> {
    todo!()
}

/// Phase 11 — whether a fetch of `remote` into `refs` would change anything.
///
/// # Caveats
///
/// * **It compares ref values, not object reachability.** A remote whose branch moved to a commit the
///   client already has returns false even though the ref name changed, which is the useful answer
///   for "should I print `Already up to date`" and the wrong one for "is the client missing objects".
/// * **An advertisement with no refs returns false**, which is what an empty repository looks like
///   and is not an error — cloning it is legitimate and produces an empty clone.
/// * **It does not contact the server**, so the answer is about the advertisement in hand and goes
///   stale the moment the remote moves. A caller that needs a decision to be atomic with a fetch asks
///   for both in one connection.
pub fn is_up_to_date(
    advertisement: &RefAdvertisement,
    remote: &BStr,
    refs: &RefStore,
) -> Result<bool> {
    let Ok(up_to_date) = is_up_to_date(advertisement, remote, refs) else {
        return Ok(false)
    };
    Ok(up_to_date)
}

/// Phase 11 — the advertised refs that are not objects the local repository has.
///
/// # Caveats
///
/// * **This is a containment check per advertised head**, not a reachability walk, so a remote head
///   whose tip is present but whose history is not returns an empty list and is wrong. The
///   negotiation exists precisely because the client cannot answer the reachability question cheaply;
///   this is a fast pre-check for the common "nothing to do" case and says so.
/// * **An advertisement with a symref head whose target is missing contributes nothing** — GitHub's
///   `HEAD` is advertised separately from the branch it names, and counting it twice would fetch it
///   twice.
/// * **A missing object database entry is not an error**, so this returns `Result` only because
///   [`Odb::contains`] can fail on an unreadable pack index.
pub fn missing_from(advertisement: &RefAdvertisement, odb: &Odb) -> Result<Vec<AdvertisedRef>> {
    todo!()
}
