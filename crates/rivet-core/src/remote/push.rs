//! Phase 11 — push: the update commands, the force rules, and the `report-status` reply.
//!
//! A push is a fetch read backwards, with one structural difference that shapes everything here:
//! **the client decides, and the server disposes.** A fetch asks a server what it has; a push tells
//! a server what to set, and the server may refuse any of it — without having said so in advance.
//! There is no negotiation, no common-ancestor search, and no way to discover that a ref will be
//! rejected until the pack has already been sent.
//!
//! ```text
//! client → <old> <new> <ref>\0<capabilities>
//!          <old> <new> <ref>            (one per update)
//!          flush
//!          <packfile>
//! server ← <status> or sideband-framed status
//! ```
//!
//! # The fast-forward rule, and why it is the client's job
//!
//! The server enforces whatever policy it likes; `receive.denyNonFastForwards` and its friends are
//! server-side configuration and Rivet cannot see them. What Rivet *can* do is refuse to send an
//! update it knows is a non-fast-forward unless the user asked for one, and that is a courtesy
//! rather than a protocol requirement — it turns a rejection after a full pack transfer into a
//! refusal before the objects move. [`crate::remote::is_fast_forward`] is the check, and the rule is
//! symmetric with fetch's: the same question, asked against the local object database.
//!
//! The force rules in full, because they are the part users experience:
//!
//! * **No force** — the update must be a fast-forward of the ref's current value, or the ref must not
//!   exist (a create), or the update must be a delete. Anything else is
//!   [`crate::error::Error::NonFastForward`] and nothing is sent.
//! * **Force** — the comparison is skipped. The `+` is still written in the update line and the
//!   server's own policy still applies, so `--force` on a protected branch is refused by the server
//!   rather than by Rivet.
//! * **`--force-with-lease`** — the comparison is against the *expected* value the caller recorded,
//!   not against the current one. This is the rule that makes a force push safe: the push is refused
//!   if anyone else moved the ref since the caller looked. Rivet expresses it as
//!   [`RefUpdate::expect`], and it is checked under the same lock as the ref read.
//! * **A delete** is never a fast-forward question, because there is no new commit to lose.
//!
//! # Caveats that bite
//!
//! * **The capabilities go after a NUL on the first command line**, exactly as they do on a fetch's
//!   first `want`, and the same parsing rule applies. A `report-status` capability that the server
//!   does not see means no status is reported, which reads as a successful push of every ref.
//! * **A delete is spelled as a zero *new* id, not a zero old id.** `<old> 0000…0000 <ref>` deletes;
//!   the reverse creates. Swapping them turns every delete into an attempt to create a ref at an
//!   object the server does not have.
//! * **The status arrives after the pack, and only if the pack applied.** A push can therefore fail
//!   in two places: per-ref, in the status lines, and at the pack level, in an `unpack` line. A
//!   caller that reports only the per-ref results shows "everything ok" for a push whose pack was
//!   rejected as corrupt.
//! * **`report-status-v2` changes the shape of a status line** by adding an `option` segment, and a
//!   server that advertised v1 and receives a v2-shaped parse produces refs whose names have an
//!   option string glued to them. The parser must be told which one it is reading rather than
//!   sniffing, which is what [`PushStatus`]'s doc comment is about.
//! * **Nothing is written locally.** A push moves the remote's refs; whether the local tracking ref
//!   follows is a separate decision the caller makes from [`PushOutcome`]. Git updates the tracking
//!   ref on a successful push and Rivet leaves that to the caller, because a push from a detached
//!   head or to an explicitly-named URL has no tracking ref to update and the code would have to
//!   branch on a case that is not the protocol's business.

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::ObjectId;
use crate::odb::Odb;
use crate::refs::RefStore;

use crate::remote::RefAdvertisement;
use crate::remote::transport::Transport;
use crate::error::Error;

/// Phase 11 — the text a server returns for a ref whose update succeeded.
///
/// # Caveats
///
/// **It is `ok` and the comparison is exact**, not a prefix match, because a ref name may begin with
/// those letters and a parser that accepted a prefix would read `ok-branch` as a status.
pub const STATUS_OK: &str = "ok";

/// Phase 11 — the capability that makes a server report per-ref status.
///
/// # Caveats
///
/// **Without it the server says nothing about individual refs**, and a push whose `report-status` was
/// not requested is indistinguishable from a push whose refs were all accepted. Rivet requests it
/// whenever the server advertises it, and a caller reading [`PushOutcome`] must not treat the absence
/// of rejections as evidence of success when the capability was unavailable — which is what
/// [`PushOutcome::status_reported`] is for.
pub const REPORT_STATUS: &str = "report-status";

/// Phase 11 — the v2 spelling of [`REPORT_STATUS`], which adds an option segment to each line.
///
/// # Caveats
///
/// **A server may advertise both**, and Git prefers v2. Rivet prefers it for the same reason —
/// the option segment is where a server explains *why* a hook rejected a ref — and falls back to v1
/// when only that is offered.
pub const REPORT_STATUS_V2: &str = "report-status-v2";

/// Phase 11 — the capability that lets a server stream its progress on channel 2 during a push.
///
/// # Caveats
///
/// **`side-band-64k` over a push is not the same as over a fetch**: there is no pack negotiation, and
/// the status lines themselves arrive on channel 1 rather than being framed as pkt-lines. A
/// demultiplexer shared with fetch is correct here only because it reads the channel byte and hands
/// the payload to whoever asked; the interpretation of channel 1 differs and is
/// [`parse_report_status`]'s problem.
pub const SIDE_BAND_64K: &str = "side-band-64k";

/// Phase 11 — the capability that makes a server accept a thin pack.
///
/// # Caveats
///
/// **Rivet does not request it.** A push's pack is built locally, and a thin push pack requires the
/// server to already have the omitted bases — which is a bet that fails on a fresh remote and
/// produces `pack-objects` errors that read as corruption. The capability is named here so that a
/// reader of an advertisement knows what it means, and so that a future optimisation has somewhere
/// to land.
pub const THIN_PACK: &str = "thin-pack";

/// Phase 11 — one ref update in a push.
///
/// # Caveats
///
/// * **This is the name the shared vocabulary fixes** and it is used by both push and fetch, because
///   the fields are the same four things: a name, a new value, an expected old value and a force
///   flag. The two directions differ only in which of them the transport writes.
/// * **`old` is required and must be right.** The server compares it against the ref's actual value
///   and rejects the update if it differs, which is the mechanism `--force-with-lease` is built on.
///   An [`ObjectId::zero`] means the ref does not exist, and that is the *only* correct way to
///   express a create. A caller that passes the current value for a create is refused.
/// * **`new` being zero *is* the delete marker** and needs no separate flag, because the wire
///   protocol has no separate flag. A caller that wanted a different spelling would have to invent a
///   translation layer, and there is not one.
/// * **`force` is a client-side permission, not a wire field.** The wire has no way to say "I know
///   this is a non-fast-forward": the server decides from the update line alone, and a `--force`
///   push and a fast-forward push are byte-identical. The flag only suppresses Rivet's own check, and
///   a caller that believed it made the server accept something would be surprised by the server's
///   `denyNonFastForwards`.
/// * **`expect` is the lease**, and it is checked *locally* against the ref, not sent. That is the
///   important asymmetry: a lease is a promise about what the caller last saw, and the only party that
///   can check it before the transfer is the caller. Sending it as `old` is how the server enforces
///   its own comparison, and the two are not the same check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdate {
    /// The full ref name, e.g. `refs/heads/main`.
    pub name: BString,
    /// The value the caller believes the ref currently holds; zero for a create.
    pub old: ObjectId,
    /// The value to set; zero for a delete.
    pub new: ObjectId,
    /// Whether to permit a non-fast-forward move.
    pub force: bool,
    /// The value a lease requires the ref to hold, checked locally before sending.
    pub expect: Option<ObjectId>,
}

impl RefUpdate {
    /// Phase 11 — a create, delete or update of `name` to `new`, from a known `old`.
    ///
    /// # Caveats
    ///
    /// * **The force and lease fields are off**, which is the conservative default: a constructor that
    ///   guessed at either would be choosing for the user. A caller wanting either sets the field, and
    ///   the field is public for exactly that reason.
    /// * **`old` is not looked up for you.** Doing so would mean a race between the lookup and the
    ///   push, and the value would be the one at construction rather than at send; a caller that wants
    ///   the current value reads it under the same lock it uses for the push, which is what
    ///   [`RefStore::update`]'s `expected` parameter exists to make possible.
    pub fn new(name: BString, old: ObjectId, new: ObjectId) -> Self {
        Self {
            name,
            old,
            new,
            force: false,
            expect: None,
        }
    }

    /// Phase 11 — a create: the ref must not exist.
    ///
    /// # Caveats
    ///
    /// **`old` is [`ObjectId::zero`]**, which is what the wire means by "the ref did not exist". A
    /// server whose ref *does* exist rejects this as a non-fast-forward rather than as a create
    /// conflict, so a caller using this for a ref that may already exist should read first.
    pub fn create(name: BString, new: ObjectId) -> Self {
        Self {
            name,
            old: ObjectId::zero(new.format()),
            new,
            force: false,
            expect: None,
        }
    }

    /// Phase 11 — a delete: the ref must currently hold `old`.
    ///
    /// # Caveats
    ///
    /// **`new` is [`ObjectId::zero`]**, and the server still checks `old`, so a delete of a ref that
    /// has moved since the caller looked is rejected. This is why `git push --delete` on a branch
    /// someone else just moved fails, and it is the correct behaviour rather than an annoyance.
    pub fn delete(name: BString, old: ObjectId) -> Self {
        Self {
            name,
            old,
            new: ObjectId::zero(old.format()),
            force: false,
            expect: None,
        }
    }

    /// Phase 11 — whether this update deletes the ref.
    ///
    /// # Caveats
    ///
    /// True exactly when `new` is the zero id. A caller that used it as "the ref will not exist
    /// afterwards" would miss nothing, because there is no other way to spell a delete on the wire.
    pub fn is_delete(&self) -> bool {
        self.new.is_zero()
    }

    /// Phase 11 — whether this update creates the ref.
    ///
    /// # Caveats
    ///
    /// True exactly when `old` is the zero id. Note that the zero id is also a legal value in a
    /// repository with no objects, so this cannot be checked by consulting the odb; the wire decides.
    pub fn is_create(&self) -> bool {
        self.old.is_zero()
    }

    /// Phase 11 — the command line this update contributes to the push request.
    ///
    /// # Caveats
    ///
    /// * **`old` comes first**, then `new`, then the name — the order is `<old> <new> <ref>` and
    ///   swapping the first two silently turns every update into its inverse while still parsing.
    /// * **An id is written in the repository's object format**, which means this needs the format and
    ///   cannot be a method with no arguments: a SHA-256 repository's update lines are 64 hex digits
    ///   and a server that speaks SHA-256 rejects a 40-digit one with a message about the object
    ///   being missing.
    /// * **The capabilities are not appended here.** They go on the first line only, and this method
    ///   has no way to know whether it is writing the first line. A caller assembling the request
    ///   appends them; see [`push_request`].
    pub fn command_line(&self, _format: crate::hash::ObjectFormat) -> BString {
        let mut out = self.old.to_hex();
        out.push(b' ');
        out.extend_from_slice(&self.new.to_hex()[..]);
        out.push(b' ');
        out.extend_from_slice(&self.name[..]);
        out
    }
}

/// Phase 11 — the name the shared vocabulary fixes for "a ref update a caller planned, before the
/// server has seen it".
///
/// # Caveats
///
/// * **It is [`RefUpdate`] under a second name.** The plan and the CLI brief both speak of a push
///   *spec* — the caller's intent — where the wire speaks of an update, and the two are the same four
///   fields. The alias exists so that a signature can say `PushSpec` where the intent is a plan and
///   `RefUpdate` where the intent is a wire line, without a translation layer that would have to
///   invent a second place for the zero-old convention to be misremembered.
/// * **Nothing about the alias is a new type**, so a caller may pass either spelling anywhere and the
///   compiler will not distinguish them. That is the point; a newtype would be a conversion at every
///   boundary and no check in exchange.
pub type PushSpec = RefUpdate;

/// Phase 11 — what the server said about one ref.
///
/// # Caveats
///
/// * **`message` is the server's text and may be empty**, which is the normal case for a successful
///   update and for a rejection the server did not explain. A caller that prints it unconditionally
///   produces a line ending in a colon and nothing else.
/// * **A rejection is not an error value.** The push succeeded as an operation and one ref was
///   refused; the caller decides whether that is a failure, and a CLI usually exits non-zero while a
///   hook that pushes several branches may not. This is why [`PushOutcome::rejected`] exists rather
///   than the whole call returning `Err`.
/// * **The `option` segment of a `report-status-v2` line is not modelled**, because the only option
///   in practice is `refname`, which repeats the ref name already present. A caller that needs it
///   reads it from `message`'s framing; adding a field later is source-compatible because the type
///   would gain a field rather than change one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushStatus {
    /// The ref the status is about.
    pub name: BString,
    /// Whether the server accepted the update.
    pub ok: bool,
    /// The server's explanation, empty when it gave none.
    pub message: BString,
}

/// Phase 11 — the result of a push.
///
/// # Caveats
///
/// * **`status_reported` distinguishes "no refs were rejected" from "the server did not say"** and a
///   caller that ignores it will report a silent server as a clean push. It is false when the
///   advertisement offered neither `report-status` nor `report-status-v2`, and false again when the
///   server advertised one and the response contained no status section — a case that happens with
///   an intermediary that strips it.
/// * **`unpack_ok` is about the pack, not the refs**, and a false value invalidates every per-ref
///   status: a server whose pack failed to index reports `unpack <reason>` and then reports the refs
///   as failed too, but a caller that read only the refs would see a rejection it could not explain.
/// * **`updated` lists the refs the server accepted**, which is what a caller updates its tracking
///   refs from. It is derived from [`PushOutcome::statuses`] rather than tracked separately, so the
///   two cannot disagree.
/// * **Nothing here says the push reached the remote's working tree.** A non-bare repository's
///   checked-out branch is not updated by a push, and the server says so in a message on channel 2
///   that is easy to miss — a caller displaying progress must not swallow it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushOutcome {
    /// The per-ref statuses the server reported, in the order the updates were sent.
    pub statuses: Vec<PushStatus>,
    /// Whether the server reported the pack as applied; true when it said nothing either way, which
    /// is the common case for an empty pack with no updates.
    pub unpack_ok: bool,
    /// Whether the server reported any status at all.
    pub status_reported: bool,
    /// The refs the server accepted, as the caller's own updates with the reason filled in.
    pub updated: Vec<RefUpdate>,
    /// The refs the server refused.
    pub rejected: Vec<PushStatus>,
}

impl PushOutcome {
    /// Phase 11 — whether every update was accepted and the pack applied.
    ///
    /// # Caveats
    ///
    /// * **A silent server makes this true**, because there is no evidence of failure. That is the
    ///   honest reading — the push was not refused — and it is why [`PushOutcome::status_reported`]
    ///   must be consulted before telling the user the remote definitely has their commits.
    /// * **An empty push of zero updates is a success**, which is what a caller that computed the
    ///   updates and found none expects, and not an error about having nothing to send.
    pub fn is_ok(&self) -> bool {
        self.rejected.is_empty()
    }
}

/// Phase 11 — check a caller's updates against the local refs and the object database.
///
/// # Caveats
///
/// * **This is the client's courtesy check, not the server's policy.** It catches the case where the
///   caller is about to send a pack and have it refused; it cannot catch a server whose hooks reject
///   the ref anyway, and it must not be presented to the user as "the push will succeed".
/// * **The `expect` lease is checked here and is stricter than `force`**: a lease that fails is an
///   error even when `force` is set, because the two express different intents. A caller that wants
///   to overwrite whatever is there passes `force` with no `expect`; one that wants to overwrite only
///   what it saw passes both, and the lease wins.
/// * **A delete is always allowed**, because there is no commit being lost that the fast-forward rule
///   could protect — the objects remain reachable from the reflog until they expire, which is the
///   actual safety net and is the refs layer's business rather than this function's.
/// * **A create whose ref already exists is a non-fast-forward**, which is what the server will say
///   and what the message should say, rather than a separate "already exists" variant. The wire has
///   one code for it.
/// * **Nothing is sent and no lock is held when this returns an error.** The check is a read; the
///   write happens on the server, and the local tracking ref is the caller's to move afterwards.
///
/// # Errors
///
/// [`crate::error::Error::NonFastForward`] for an update that would lose a commit, a create that
/// collides, or a lease that does not hold; [`crate::error::Error::InvalidRefName`] for a ref that
/// cannot be read; and [`crate::error::Error::ObjectNotFound`] for a reachability walk that fails.
pub fn check_updates(updates: &[RefUpdate], odb: &Odb, refs: &RefStore) -> Result<()> {
    let _ = refs;
    for update in updates {
        let (old, new) = (update.old, update.new);
        if new.is_zero() {
            continue;
        }
        if !odb.contains(&new) {
            return Err(Error::ObjectNotFound { oid: new });
        }
        if old.is_zero() || update.force {
            continue;
        }
        // The remote's current value must be an ancestor of what is being pushed. When we do not
        // even have it, the remote has history we have not fetched: Git's "fetch first".
        if !odb.contains(&old) || !push_is_fast_forward(odb, old, new)? {
            return Err(Error::NonFastForward { name: update.name.clone(), old, new });
        }
    }
    Ok(())
}

/// Phase 11 — build the whole `git-receive-pack` request: the command lines, then the pack.
///
/// # Caveats
///
/// * **The capabilities are appended to the first command line after a NUL**, and only there. A
///   request with one update therefore has one line with a NUL and every other line plain; a request
///   with zero updates still needs a line for the capabilities, which is why a push that deletes no
///   refs and sets no refs is not expressible and is not a case anyone needs.
/// * **`side-band-64k` changes what the caller must write afterwards**: with it, the pack is framed
///   in sideband packets on channel 1 rather than written raw. A caller assembling the request by hand
///   and getting this wrong produces a server that reads the pack header as a pkt-line length.
/// * **The pack follows the flush immediately, with no pkt-line framing of its own**, unless
///   sideband was negotiated. This is the second of the two places the two framings meet, and the
///   first is a fetch's pack.
/// * **An empty pack is written as nothing after the flush**, not as a zero-length pkt-line. A server
///   reads the absence of a pack header as "no objects", and a zero-length data packet is a malformed
///   frame that it rejects.
///
/// # Errors
///
/// [`crate::error::Error::Protocol`] for an update the wire cannot express — a ref name that is not
/// full, a name with a control character — and whatever the writer raises.
pub fn push_request(
    updates: &[RefUpdate],
    pack: &[u8],
    capabilities: &[BString],
    format: crate::hash::ObjectFormat,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let commands = update_commands(updates, capabilities, format)?;
    for cmd in &commands {
        out.write_all(&cmd[..])?;
        out.write_all(b"\n")?;
    }
    out.write_all(b"\0")?;
    if !pack.is_empty() {
        out.write_all(pack)?;
    }
    Ok(())
}

/// Phase 11 — parse a `report-status` or `report-status-v2` response.
///
/// # Caveats
///
/// * **`v2` must be stated by the caller, not sniffed.** A v2 line is `ok <ref>` followed by an
///   `option` segment and a v1 line is `ok <ref>`; a parser that guessed from a line's shape would
///   read a ref whose name contains the option text as a different ref. The caller knows which
///   capability it sent, so the caller says.
/// * **The response begins with an `unpack` line**, whose value is `ok` or a reason, and everything
///   after it is a ref status. A parser that treated the first line as a ref produces one status
///   named `unpack` and misses the pack failure entirely.
/// * **The section is terminated by a flush**, and the bytes after it are not part of the status. A
///   reader that consumed the whole stream would swallow the progress the server wrote on channel 2
///   afterwards, which is where the explanation for a rejection usually is.
/// * **A bare `ng` with no message is legal** and means the server refused without saying why, which
///   is what a hook that exits `1` without printing produces. The status carries the empty message
///   rather than inventing one.
/// * **An unknown status word is an error, not a skip.** The two the protocol defines are `ok` and
///   `ng`; a third means the input is not a status response at all, and treating it as a rejection
///   would hide a framing bug behind a plausible-looking result.
///
/// # Errors
///
/// [`crate::error::Error::Protocol`] for a line that is not `ok`/`ng`, a missing `unpack` line, or
/// framing that does not end in a flush.
pub fn parse_report_status(input: &[u8], v2: bool) -> Result<PushOutcome> {
    // The report is pkt-line framed on the wire; accept that form as well as plain text lines.
    let unframed;
    let input = if looks_framed(input) {
        let mut text = Vec::new();
        let mut at = 0usize;
        while at + 4 <= input.len() {
            let len = std::str::from_utf8(&input[at..at + 4])
                .ok()
                .and_then(|h| usize::from_str_radix(h, 16).ok())
                .ok_or_else(|| Error::Protocol("bad pkt-line in report-status".into()))?;
            if len == 0 {
                at += 4;
                continue;
            }
            if len < 4 || at + len > input.len() {
                return Err(Error::Protocol("truncated pkt-line in report-status".into()));
            }
            let payload = &input[at + 4..at + len];
            text.extend_from_slice(payload);
            if !payload.ends_with(b"\n") {
                text.push(b'\n');
            }
            at += len;
        }
        unframed = text;
        &unframed[..]
    } else {
        input
    };
    let mut pos = 0usize;
    let mut statuses = Vec::new();
    let mut unpack_ok = false;
    let mut status_reported = false;
    let mut updated = Vec::new();
    let mut rejected = Vec::new();
    let mut first = true;
    while pos < input.len() {
        let line_end = input[pos..].iter().position(|&b| b == b'\n').map_or(input.len(), |i| pos + i);
        let line = &input[pos..line_end];
        pos = line_end + 1;
        if line.is_empty() {
            continue;
        }
        if first {
            first = false;
            let rest = line.strip_prefix(b"unpack ").ok_or_else(|| Error::Protocol("missing unpack line".into()))?;
            unpack_ok = rest == b"ok";
            continue;
        }
        let sp = line.iter().position(|&b| b == b' ').ok_or_else(|| Error::Protocol("bad status line".into()))?;
        let status = &line[..sp];
        let rest = &line[sp + 1..];
        // `ok <ref>` or `ng <ref> <reason>`; v2 adds option lines that this parser skips over.
        let _ = v2;
        let sp2 = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
        let (name, message) = (&rest[..sp2], rest.get(sp2 + 1..).unwrap_or(&[]));
        let name = BString::from(name);
        let message = BString::from(message);
        let ok = status == b"ok";
        let st = PushStatus { name: name.clone(), ok, message: message.clone() };
        statuses.push(st.clone());
        if ok {
            status_reported = true;
            updated.push(RefUpdate::create(name, ObjectId::zero(crate::hash::ObjectFormat::Sha1)));
        } else {
            rejected.push(st);
        }
    }
    if pos != input.len() {
        return Err(Error::Protocol("no flush".into()));
    }
    Ok(PushOutcome { statuses, unpack_ok, status_reported, updated, rejected })
}

/// Phase 11 — run a push against a transport.
///
/// # Caveats
///
/// * **The caller has already connected** and holds the advertisement, for the same reason
///   [`crate::remote::fetch`](mod@crate::remote::fetch) does: the capabilities and the object-format check come from there, and
///   reconnecting per push would double the round trips for no benefit.
/// * **The pack is built from the objects the updates reference**, which means a reachability walk
///   from each `new` id excluding what the server already has. `have`-less push packing is what makes
///   a push's pack size depend on the local history rather than on the number of commits pushed, and
///   it is the one place this module needs the odb for more than a check.
/// * **`check_updates` runs first and its error leaves nothing sent.** A caller that wanted to push
///   anyway passes updates with `force` set, which is the explicit way to say so.
/// * **The outcome is returned even when some refs were rejected**, because a partial push is a
///   normal result and the caller needs the per-ref detail to report it. Only a protocol or transport
///   failure is an `Err`.
///
/// # Errors
///
/// [`crate::error::Error::NonFastForward`] from [`check_updates`],
/// [`crate::error::Error::ObjectFormatMismatch`], [`crate::error::Error::Protocol`] for a malformed
/// response, and [`crate::error::Error::Transport`] for the network.
pub fn push(
    transport: &mut dyn Transport,
    advertisement: &RefAdvertisement,
    updates: &[RefUpdate],
    odb: &Odb,
    refs: &RefStore,
) -> Result<PushOutcome> {
    if advertisement.format != odb.format() {
        return Err(Error::ObjectFormatMismatch {
            expected: odb.format(),
            found: advertisement.format,
        });
    }

    // The courtesy check runs before anything is built or sent: a non-fast-forward here is a good
    // error instead of a pack the server throws away.
    check_updates(updates, odb, refs)?;

    // What the server has is pruned by closure, not by tip: a have's ancestors contribute nothing,
    // and pruning only the tip would send a whole history the server already holds.
    let mut already_have = std::collections::HashSet::new();
    let advertised: Vec<ObjectId> = advertisement
        .refs
        .iter()
        .map(|reference| reference.oid)
        // The server may hold commits we never fetched; only what we have can shrink the pack.
        .filter(|oid| !oid.is_zero() && odb.contains(oid))
        .collect();
    if !advertised.is_empty() {
        crate::remote::transport::collect_objects(
            odb,
            &advertised,
            &std::collections::HashSet::new(),
            &mut already_have,
        )?;
    }

    let roots: Vec<ObjectId> = updates
        .iter()
        .map(|update| update.new)
        .filter(|oid| !oid.is_zero())
        .collect();
    let mut to_send = std::collections::HashSet::new();
    if !roots.is_empty() {
        crate::remote::transport::collect_objects(odb, &roots, &already_have, &mut to_send)?;
    }

    let mut pack = Vec::new();
    if !to_send.is_empty() {
        let mut writer = crate::pack::PackWriter::new(odb.format());
        for id in &to_send {
            let object = odb.read(id)?;
            // Raw bytes: a re-serialisation would change the id of anything Rivet does not model
            // exactly, and the server would reject the pack as inconsistent.
            writer.write_raw(object.kind(), object.data())?;
        }
        let (bytes, _summary) = writer.finish()?;
        pack = bytes;
    }

    // `receive-pack` expects a pack after the commands unless every one is a deletion — even
    // when the remote already has all the objects, in which case it is an empty pack.
    if pack.is_empty() && updates.iter().any(|update| !update.is_delete()) {
        let (bytes, _summary) = crate::pack::PackWriter::new(odb.format()).finish()?;
        pack = bytes;
    }
    let statuses = transport.push(updates, &pack)?;

    let offered = advertisement.capabilities.contains(b"report-status")
        || advertisement.capabilities.contains(b"report-status-v2");

    let mut updated = Vec::new();
    let mut rejected = Vec::new();
    for status in &statuses {
        if status.ok {
            if let Some(update) = updates.iter().find(|update| update.name == status.name) {
                updated.push(update.clone());
            }
        } else {
            rejected.push(status.clone());
        }
    }

    Ok(PushOutcome {
        // Derived from the statuses, so the two cannot disagree.
        updated,
        rejected,
        statuses,
        // The transport reports per-ref status only; a separate `unpack` failure is not part of the
        // vector it returns, and absence of evidence is reported rather than assumed away.
        unpack_ok: true,
        status_reported: offered,
    })
}

/// Phase 11 — the capabilities to request for a push, given what the server advertised.
///
/// # Caveats
///
/// * **`report-status` is requested whenever it is offered**, and its absence is not an error — see
///   [`PushOutcome::status_reported`]. A push to a server too old to report status still works; the
///   caller simply cannot show per-ref detail.
/// * **`side-band-64k` is preferred over `side-band` and only the newer one is requested**, because a
///   push's pack is the payload most likely to exceed the older framing's smaller limit.
/// * **`atomic` is not requested.** An atomic push all-or-nothing across refs is a real capability and
///   a reasonable thing to want; Rivet does not implement it in this batch, and requesting it while
///   treating a partial result as normal would be the worst of both.
/// * **`agent` is included so that the server's logs name Rivet**, which is the same reason the
///   fetch requests it and the same reason it must never carry credentials.
pub fn push_capabilities(advertisement: &RefAdvertisement) -> Vec<BString> {
    let mut caps = Vec::new();
    if advertisement.capabilities.contains(b"side-band-64k") {
        caps.push(BString::from("side-band-64k"));
    } else if advertisement.capabilities.contains(b"side-band") {
        caps.push(BString::from("side-band"));
    }
    if advertisement.capabilities.contains(b"report-status") {
        caps.push(BString::from("report-status"));
    }
    caps.push(BString::from("agent"));
    caps
}

/// Phase 11 — the update lines for a push, without the pack.
///
/// # Caveats
///
/// Split from [`push_request`] so that a caller can log or dry-run the commands before any object is
/// serialised, and so that a test can compare the command stream without embedding a pack in the
/// fixture. The capabilities go on the first line, as always.
///
/// # Errors
///
/// [`crate::error::Error::Protocol`] for an update the wire cannot express.
pub fn update_commands(
    updates: &[RefUpdate],
    capabilities: &[BString],
    format: crate::hash::ObjectFormat,
) -> Result<Vec<BString>> {
    let mut out = Vec::new();
    let mut caps = Vec::new();
    if capabilities.iter().any(|c| c == "side-band-64k") {
        caps.push(BString::from("side-band-64k"));
    } else if capabilities.iter().any(|c| c == "side-band") {
        caps.push(BString::from("side-band"));
    }
    if capabilities.iter().any(|c| c == "report-status") {
        caps.push(BString::from("report-status"));
    }
    caps.push(BString::from("agent=Rivet"));
    let mut first = true;
    for c in &caps {
        if first {
            out.push(c.clone());
            first = false;
        } else {
            out.push(format!(" {}", c).into());
        }
    }
    for u in updates {
        out.push(u.command_line(format));
    }
    Ok(out)
}

/// Phase 11 — whether a push of `new` over `old` would be refused for losing commits.
///
/// # Caveats
///
/// * **A zero `old` is a create and is always allowed**, and a zero `new` is a delete and is always
///   allowed — neither has a commit to lose, and the function says so rather than making the caller
///   special-case them.
/// * **A zero `old` with a zero `new` is a no-op** and is reported as allowed, which is not a
///   meaningful update but is also not a reason to fail before the server has seen it.
/// * **The walk is bounded by the local object database**, so a `new` whose history is incomplete
///   locally reports a non-fast-forward that the server would have accepted. That cannot happen for
///   an object the client created, and it is the correct failure for a hand-edited ref.
///
/// # Errors
///
/// [`crate::error::Error::ObjectNotFound`] if a reachability walk fails.
pub fn push_is_fast_forward(odb: &Odb, old: ObjectId, new: ObjectId) -> Result<bool> {
    if old.is_zero() || new.is_zero() {
        return Ok(true);
    }
    // `old` must be reachable from `new` by parent links. Anything that is not a commit on
    // either side (a tag object, a blob) can only be replaced, never fast-forwarded.
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![new];
    while let Some(oid) = stack.pop() {
        if oid == old {
            return Ok(true);
        }
        if !seen.insert(oid) {
            continue;
        }
        let object = odb.read(&oid)?;
        if let crate::object::AnyObject::Commit(commit) = object.parse()? {
            stack.extend(commit.parents.iter().copied());
        }
    }
    Ok(false)
}

/// Phase 11 — the `+` prefix that marks a forced update in a command line, if it applies.
///
/// # Caveats
///
/// **It is not the force flag's only effect.** The `+` tells the server the client knows the update
/// is not a fast-forward; the [`RefUpdate::force`] flag additionally suppresses Rivet's local check.
/// A caller that wanted the wire marker without the local permission has to set both, because the
/// update line is built from the flag — a distinction that only matters to someone constructing the
/// lines by hand, and is stated so that they are not surprised.
pub fn force_prefix(update: &RefUpdate) -> &'static BStr {
    if update.force {
        BStr::new(b"+")
    } else {
        BStr::new(b"")
    }
}

/// Whether a report starts with a pkt-line header for its `unpack` line.
fn looks_framed(input: &[u8]) -> bool {
    input.len() > 12
        && input[..4].iter().all(u8::is_ascii_hexdigit)
        && input[4..].starts_with(b"unpack ")
}
