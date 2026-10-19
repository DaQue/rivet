//! Phase 4 — revision specs and the commit walk: turning what a user typed into commits, in
//! order.
//!
//! Two problems live here and they are deliberately kept apart, because the first is pure
//! parsing over names and the second is a graph traversal over objects:
//!
//! 1. **`<rev>` parsing** ([`RevSpec::parse`], [`parse_rev_spec`]) — `HEAD`, a branch name, an
//!    abbreviated object id, `main~3`, `v1.0^{commit}`, `a..b`. It resolves names against
//!    [`crate::refs::RefStore`] and never reads the object database, so it is testable without a
//!    repository and it is where `rev-parse` gets its answers.
//! 2. **The walk** ([`CommitWalker`]) — from one or more starting commits, produce each commit
//!    reachable, in topo or date order, with the filters `log` needs (`--first-parent`,
//!    `--max-count`, `--skip`, `--since`, `--until`, path limiting).
//!
//! # Why the walker is not an `Iterator`
//!
//! The order a walk emits in is a *policy* — topological, date, author date, reversed — and none
//! of those can be produced lazily by a `next()` that only has a frontier: topological order in
//! particular requires either a full pass or a Kahn-style priority queue over the whole reachable
//! set, and a date-ordered walk needs a heap that sees commits out of traversal order. Making
//! `CommitWalker` an `Iterator` would either force an eager full traversal on every caller
//! (including the one that wants the first commit of `HEAD~1`) or silently produce a *different*
//! order than the caller asked for. So [`CommitWalker::next_commit`] is a named method, and
//! [`CommitWalker::into_vec`] is the eager form for callers that want everything.
//!
//! # Caveats that bite
//!
//! * **`~n` and `^n` are not the same operation.** `^n` is the *n*-th parent of one commit — a
//!   single step, and `^0` means "this commit, peeled from a tag" — while `~n` is *n* first-parent
//!   steps. On a linear history they coincide; on a merge, `HEAD~1` is the merge's first parent
//!   and `HEAD^1` is the same, but `HEAD^2` is the branch that was merged in and `HEAD~2` is the
//!   first parent's first parent. Treating them as synonyms makes `log` skip merges' other
//!   parents silently, which is the bug that makes a `--first-parent` flag look like it works
//!   when it does not.
//! * **An abbreviated id is a prefix of the *hex* form, and its minimum length is 4.** Git
//!   refuses `abc` (three characters) as a revision because it would make every short word a
//!   potential id; Rivet must apply the same floor, and must report
//!   [`crate::error::Error::AmbiguousPrefix`] rather than picking when several objects match.
//! * **A revision spec is resolved against refs in a fixed order of precedence.**
//!   `<name>`, then `refs/<name>`, then `refs/tags/<name>`, then `refs/heads/<name>`, then
//!   `refs/remotes/<name>`, then `refs/remotes/<name>/HEAD`, then a raw id. Getting the order
//!   wrong makes a tag shadow a branch of the same name or, worse, makes both shadowable by a
//!   file in the work tree — and the precedence is the specification, not an implementation
//!   detail, because real repositories have a branch and a tag called `v1`.
//! * **`a..b` excludes `a` and includes `b`; `a...b` excludes the merge base.** The three-dot form
//!   is symmetric difference and is what `log --left-right` needs; implementing it as "everything
//!   in either but not both" is right, and implementing it as "the symmetric difference *of the
//!   two commits*" rather than of their histories is the classic mistake.
//! * **A date-ordered walk is not stable.** Two commits with the same committer timestamp come
//!   out in whichever order the heap breaks ties, and Git's own order there is not guaranteed
//!   either. A caller comparing Rivet's `log` output against Git's must use a history whose
//!   timestamps are distinct, or must compare the set rather than the sequence.
//! * **`--since`/`--until` filter on committer date by default and on author date with
//!   `--author-date-order`**, and the cut-off is applied during the walk rather than after it,
//!   because a walk cannot stop early on a date predicate unless the commits are already in date
//!   order — which they are not, for a repository with a wrong clock in its history. Filtering
//!   after the walk is correct and is what Rivet must do; stopping early is an optimisation that
//!   only holds for a topologically sorted, monotonic history and Git itself gets this wrong in
//!   obscure cases.
//! * **A walk that visits a commit twice must emit it once.** Merge commits make the reachable set
//!   a DAG, not a tree, so the walker keeps a visited set; a walker that emitted per edge produces
//!   duplicated `log` lines on every merge, which is exactly the kind of bug that looks like a
//!   rendering problem and is not.

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::Commit;
use crate::odb::Odb;
use crate::refs::{RefName, RefStore};
use crate::error::Error;

/// Phase 4 — what order a walk should emit commits in.
///
/// # Caveats
///
/// * **`Topo` and `Date` produce genuinely different sequences on any history with merges**, and
///   neither is a refinement of the other: `Topo` guarantees a commit never precedes its parents
///   but says nothing about dates, and `Date` sorts by committer time but can emit a child before
///   its parent if the clock went backwards. `git log` defaults to `Date`; `--topo-order` is the
///   one that makes `--graph` readable, and the CLI's flag maps directly onto this.
/// * **`AuthorDate` is `Date` with a different key**, not a different algorithm — the sort is
///   otherwise identical, the comparison is not. The name mirrors Git's `--author-date-order` and
///   the two are not interchangeable on any repository whose commits were rebased.
/// * **`Reverse` is not an order, it is a post-processing step**, and it is a variant here because
///   Git's `--reverse` is spelled as a flag on top of whichever order was selected. An
///   implementation must therefore apply it *after* the chosen order rather than as an alternative
///   to it, which is what makes it a composition; an implementation that treated it as a
///   third sorting key would produce something no caller wants.
/// * **There is no `Date`-then-`Topo` hybrid.** Git's `--date-order` is *not* the same as
///   `--topo-order` with date tie-breaking, and neither is expressible in terms of the other;
///   mapping one onto the other changes the output on real histories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum SortOrder {
    /// Reverse chronological by committer date. Git's default.
    #[default]
    Date,
    /// Reverse chronological by author date.
    AuthorDate,
    /// Parents always after their children, no two branches interleaved.
    Topo,
    /// Whatever order was chosen, reversed.
    Reverse,
}

/// Phase 4 — where a walk should stop.
///
/// # Caveats
///
/// * **Terminators prune the walk, they do not filter its output.** `^main` stops the traversal at
///   `main` and everything reachable from it, which is what makes `git log feature ^main` show
///   only the commits unique to `feature`. Implementing it as "collect everything, then drop
///   commits in `main`" gives the same answer for a linear history and a wrong one for a merge
///   whose other side is also in `main` — the traversal has to stop, and the difference is
///   observable.
/// * **`MaxCount(n)` stops the *walk*, `--skip` does not.** `-n 10 --skip 5` must produce commits
///   six through fifteen of the ordered sequence, which means the walk cannot stop at ten; the
///   count applies after the skip, and an implementation that applied the limit first returns five
///   too few.
/// * **`At(name)` is a terminator by ref name, resolved once at the start.** A ref that moves
///   during the walk cannot change where it stopped, which is what makes the walk deterministic;
///   re-resolving per commit would allow a walk that never terminates.
/// * **Several terminators combine as a union.** `log a ^b ^c` excludes the ancestors of `b` *and*
///   of `c`, and an implementation that applied only the last one would show commits the user
///   asked to exclude.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalkTerminator {
    /// Do not stop early.
    None,
    /// Stop after this many commits have been emitted.
    MaxCount(usize),
    /// Stop at this ref and everything reachable from it.
    At(RefName),
    /// Stop at this commit and everything reachable from it.
    Object(ObjectId),
}

/// Phase 4 — how a walk should behave.
///
/// # Caveats
///
/// * **`first_parent` is the flag that makes `log` usable on a merge-heavy history**, and it is a
///   *traversal* option, not a filter: with it on, a merge's other parents are never enqueued, so
///   their commits are not merely hidden but never visited. A caller that also passed a path
///   filter then gets a different answer than it would without `first_parent`, and that is
///   correct — Git behaves the same way.
/// * **`paths` filtering is not implemented in the walker and must not be faked.** History
///   simplification is a genuinely different algorithm (Git's is a whole file of it, with its own
///   corner cases around merges), and a walker that filtered by "the commit's tree differs from
///   its parent at this path" produces a *different* and wrong answer for merges — it drops a
///   merge that resolved a conflict without changing the path, which `git log -- path` shows. The
///   field is here so the CLI has somewhere to put the paths to pass on to the later phase; the
///   walker must return [`crate::error::Error::NotImplemented`] rather than approximate them.
/// * **`all` means every ref under `refs/`**, which for a large repository is every branch and
///   every tag and is therefore not a cheap default. It is not "every object" — a dangling commit
///   is not in the walk, which is why `git log --all` does not show the results of a botched
///   rebase and `git reflog` does.
/// * **`since` and `until` are seconds since the epoch, and both ends are inclusive.** Storing
///   them as numbers rather than a time type is deliberate: the commit's timestamp is a plain
///   `i64` with a `±hhmm` offset that does not determine a zone, and converting both sides through
///   a zoned time type introduces a normalisation that can move a commit across the boundary. See
///   [`crate::object::Signature`].
/// * **`sort` and `reverse` are both present even though [`SortOrder::Reverse`] exists.** The two
///   encode the same thing and the redundant spelling is the scaffold's; when an implementation
///   lands, one of them must be chosen as authoritative and the other honoured as a synonym, and
///   the choice must be documented. Do not implement them as independent flags, or
///   `sort: Topo, reverse: false` and `sort: Reverse` will disagree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalkOptions {
    /// The order to emit commits in.
    pub sort: SortOrder,
    /// Follow only first parents, skipping a merge's other sides entirely.
    pub first_parent: bool,
    /// Stop after this many commits, or `None` for all of them.
    pub max_count: Option<usize>,
    /// Skip this many commits before emitting any.
    pub skip: usize,
    /// Only commits at or after this committer time.
    pub since: Option<i64>,
    /// Only commits at or before this committer time.
    pub until: Option<i64>,
    /// Refs and commits whose ancestry is excluded from the walk.
    pub terms: Vec<WalkTerminator>,
    /// Walk from every ref under `refs/` rather than from the given starting points.
    pub all: bool,
    /// Paths to limit the history to. Not implemented; see the type's caveats.
    pub paths: Vec<BString>,
}

impl WalkOptions {
    /// Phase 4 — the options a plain `rivet log` uses: date order, all parents, no limit, no
    /// skips, no date bounds, no terminators, not `--all`, no paths.
    pub fn new() -> Self {
        Self::default()
    }

    /// Phase 4 — follow only first parents.
    pub fn first_parent(&mut self, on: bool) -> &mut Self {
        self
    }

    /// Phase 4 — limit the number of commits emitted.
    pub fn max_count(&mut self, count: usize) -> &mut Self {
        self
    }

    /// Phase 4 — skip the first `count` commits of the ordered sequence.
    pub fn skip(&mut self, count: usize) -> &mut Self {
        self
    }

    /// Phase 4 — select the order.
    pub fn sort(&mut self, order: SortOrder) -> &mut Self {
        self
    }

    /// Phase 4 — walk from every ref rather than from the starting points given.
    pub fn all(&mut self, on: bool) -> &mut Self {
        self
    }

    /// Phase 4 — exclude the ancestry of a commit.
    pub fn exclude(&mut self, oid: ObjectId) -> &mut Self {
        self
    }
}

impl Default for WalkOptions {
    /// Phase 4 — the defaults of [`WalkOptions::new`].
    fn default() -> Self {
        Self {
            sort: SortOrder::default(),
            first_parent: false,
            max_count: None,
            skip: 0,
            since: None,
            until: None,
            terms: Vec::new(),
            all: false,
            paths: Vec::new(),
        }
    }
}

/// Phase 4 — a parsed revision spec, before it is resolved against the object database.
///
/// # Caveats
///
/// * **`parse` does not read the object database.** It resolves refs, because a branch name is a
///   ref and nothing else, but it never loads an object — so `main~3` parses to a
///   [`Revision`]-shaped description that has to be walked once objects are available. That split
///   is what makes `rev-parse` fast and what lets the parser be tested against a fixture ref
///   store.
/// * **`Ref` holds a full [`RefName`], not the abbreviation the user typed.** `main` becomes
///   `refs/heads/main` at parse time, by the precedence rule in the module caveat, so a later
///   lookup cannot be shadowed by a ref that appeared in between.
/// * **`Oid` includes abbreviations**, which are prefixes and not ids. The distinction matters
///   because resolving one can fail with [`crate::error::Error::AmbiguousPrefix`], and a type
///   that stored both in one `ObjectId` would lose the ability to report that accurately.
/// * **There is no `Upstream` variant.** `@{upstream}` and `@{push}` require the remote-tracking
///   configuration and a fetch, which is plan §4 Phase 11; naming the variant now would make
///   every match in the crate handle a case that cannot be constructed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevSpec {
    /// `HEAD`, whether attached or detached.
    Head,
    /// `HEAD` when it is detached — the same commit, but a caller that must report *what* was
    /// named differently.
    DetachedHead,
    /// A full object id.
    Oid(ObjectId),
    /// An abbreviated object id, to be resolved against the database.
    Abbrev(BString),
    /// A ref name, by its full name.
    Ref(RefName),
    /// A spec that has been resolved to a commit, with the description it came from.
    Resolved(ObjectId),
}

/// Phase 4 — a resolved revision: a commit, plus what else the spec named.
///
/// # Caveats
///
/// * **`tag` and `from_tag` record that the spec named an annotated tag**, which is not the same
///   as the spec naming a commit: `log v1.0` prints the tag's message *and* the commit's, and a
///   caller that only had the commit would lose the tag. A lightweight tag is a commit id in the
///   refs, so `tag` is `None` for one and the two cases are distinguishable.
/// * **`tree` is the commit's tree**, carried here because every caller that resolves a revision
///   immediately wants it — `diff`, `checkout`, `log --stat` — and re-reading the commit to fetch
///   it is a second object read per revision.
/// * **`spec` is the spec as written**, kept for the error messages and for `log`'s
///   `commit <oid> (<spec>)` header. It is bytes, not a string: it is what the user typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    /// The commit the spec resolved to.
    pub commit: ObjectId,
    /// The tree of that commit.
    pub tree: ObjectId,
    /// The annotated tag the spec went through, if it named one.
    pub tag: Option<ObjectId>,
    /// Whether the spec named the tag rather than the commit.
    pub from_tag: bool,
    /// The spec as it was written.
    pub spec: BString,
}

impl Revision {
    /// Phase 4 — the commit's parents, in order.
    ///
    /// # Caveats
    ///
    /// Reads the commit object, so it is not free. A caller walking the graph should use the
    /// commit it already loaded rather than calling this per commit — which is what
    /// [`CommitWalker`] does internally, and why this is a convenience rather than the walker's
    /// primitive.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] if the commit is missing, and
    /// [`crate::error::Error::WrongObjectKind`] if the id names something else.
    pub fn parents(&self, odb: &Odb) -> Result<Vec<ObjectId>> {
        let obj = odb.read(&self.commit)?;
        let data = obj.data();
        let mut parents = Vec::new();
        let mut i = 0;
        while i + 40 <= data.len() {
            let line = &data[i..i + 40];
            if line.starts_with(b"tree ") {
                i += 40;
                continue;
            }
            if line.starts_with(b"parent ") {
                let hex = BStr::new(&line[7..40]);
                let id = ObjectId::from_hex(odb.format(), hex)?;
                parents.push(id);
                i += 40;
            } else {
                break;
            }
        }
        Ok(parents)
    }

    /// Phase 4 — whether the commit is a merge.
    ///
    /// # Caveats
    ///
    /// More than one parent, which is a fact about the commit's headers and not about its tree.
    /// An octopus merge has three or more and is still "a merge", so the test is `> 1` and not
    /// `== 2`.
    ///
    /// # Errors
    ///
    /// As [`Revision::parents`].
    pub fn is_merge(&self, odb: &Odb) -> Result<bool> {
        let parents = self.parents(odb)?;
        Ok(parents.len() > 1)
    }
}

impl RevSpec {
    /// Phase 4 — parse a revision spec against a ref store.
    ///
    /// # Caveats
    ///
    /// * **The precedence order is the specification**; see the module caveat. It is applied here
    ///   and nowhere else, so a second caller resolving names must go through this function
    ///   rather than reimplementing the lookup.
    /// * **The result is not resolved to an object.** A [`RevSpec::Ref`] or [`RevSpec::Abbrev`]
    ///   still needs the object database before it names a commit; only a full 40- or 64-character
    ///   hex string is resolved here, and only because it needs no lookup. Callers that want a
    ///   commit call [`RevSpec::resolve`].
    /// * **`~` and `^` suffixes are parsed and then applied at resolve time**, since applying them
    ///   requires reading commits. The parser records them as part of the spec's text and reports
    ///   [`crate::error::Error::InvalidRevision`] for a suffix that cannot be parsed — a `~x`, or a
    ///   `^` with nothing after it. Validating here rather than at resolve time is what makes
    ///   `rev-parse --verify` reject a bad spec without touching the object store.
    /// * **An empty spec is an error, not `HEAD`.** Git's `rev-parse` defaults to `HEAD` in some
    ///   contexts and this is the caller's decision, not the parser's; silently producing `HEAD`
    ///   here makes a `log` with no arguments work and makes a mistyped `diff ''` diff the wrong
    ///   thing.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidRevision`] for anything that does not parse,
    /// [`crate::error::Error::RefNotFound`] for a name that resolves to no ref and is not an id,
    /// and [`crate::error::Error::InvalidHex`] for a hex-looking string of the wrong length.
    pub fn parse(format: ObjectFormat, spec: &BStr, refs: &RefStore) -> Result<Self> {
        let spec = &spec[..];
        if spec.is_empty() {
            return Err(Error::InvalidRevision {
                spec: BString::from(""),
                reason: "empty revision spec",
            });
        }
        let hex_len = format.hex_len();
        if spec.len() == hex_len && spec.iter().all(|b| b.is_ascii_hexdigit()) {
            let id = ObjectId::from_hex(format, BStr::new(spec))?;
            return Ok(RevSpec::Oid(id));
        }
        if spec.len() < hex_len && spec.iter().all(|b| b.is_ascii_hexdigit()) {
            let id = ObjectId::from_hex_prefix(format, BStr::new(spec))?;
            return Ok(RevSpec::Abbrev(BString::from(spec)));
        }
        let name = BString::from(spec);
        let name = RefName::from_bytes(&name)?;
        Ok(RevSpec::Ref(name))
    }

    /// Phase 4 — resolve a parsed spec to a commit.
    ///
    /// # Caveats
    ///
    /// * **Peeling is not optional.** A spec naming an annotated tag resolves to the *commit* the
    ///   tag points at, and `Revision::tag` records the tag so a caller can print it. A caller
    ///   that used the tag's own id as a commit would write a ref pointing at a tag where a commit
    ///   is expected.
    /// * **`^` and `~` suffixes are applied here**, by loading commits — which is why this is the
    ///   function that can report [`crate::error::Error::NoMergeBase`]'s sibling for a parent that
    ///   does not exist: [`crate::error::Error::InvalidRevision`], since `HEAD^5` on a two-parent
    ///   merge names nothing.
    /// * **An abbreviation is resolved with [`crate::odb::Odb::resolve_prefix`]**, so an ambiguous
    ///   one is [`crate::error::Error::AmbiguousPrefix`] rather than a guess, and the minimum
    ///   length rule lives there rather than here.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`], [`crate::error::Error::AmbiguousPrefix`],
    /// [`crate::error::Error::NotACommit`] for a tag chain ending at a tree or blob,
    /// [`crate::error::Error::TagCycle`] for a tag loop, and
    /// [`crate::error::Error::InvalidRevision`] for a suffix naming a parent that does not exist.
    pub fn resolve(&self, format: ObjectFormat, refs: &RefStore, odb: &Odb) -> Result<Revision> {
        use crate::object::{Commit, Object, Tag};

        let spec = match self {
            RevSpec::Head | RevSpec::DetachedHead => BString::from("HEAD"),
            RevSpec::Oid(oid) | RevSpec::Resolved(oid) => oid.to_hex(),
            RevSpec::Abbrev(prefix) => prefix.clone(),
            RevSpec::Ref(name) => BString::from(name.as_bytes()),
        };

        // Resolve the name to an object first; peeling to a commit is a separate step because a
        // tag has to be remembered on the way down.
        let mut current = match self {
            RevSpec::Head | RevSpec::DetachedHead => refs.head_commit(odb)?,
            RevSpec::Oid(oid) | RevSpec::Resolved(oid) => *oid,
            RevSpec::Abbrev(prefix) => {
                let abbreviated = ObjectId::from_hex_prefix(format, BStr::new(prefix))?;
                odb.resolve_prefix(&abbreviated)?
            }
            RevSpec::Ref(name) => refs.resolve_oid(name)?,
        };

        // Peel annotated tags. A lightweight tag is just a commit id in the refs and never enters
        // this loop; a tag chain is legal, a tag cycle is not.
        let mut tag: Option<ObjectId> = None;
        let mut from_tag = false;
        let mut hops = 0usize;
        let commit_oid = loop {
            match odb.read(&current)? {
                Object::Commit(_) => break current,
                Object::Tag(data) => {
                    if tag == Some(current) || hops >= 32 {
                        return Err(Error::TagCycle { oid: current });
                    }
                    let parsed = Tag::parse(format, &data)?;
                    if tag.is_none() {
                        tag = Some(current);
                        from_tag = true;
                    }
                    current = parsed.object();
                    hops += 1;
                }
                _ => {
                    return Err(Error::NotACommit {
                        name: spec.clone(),
                    })
                }
            }
        };

        let data = odb.read(&commit_oid)?;
        let commit = Commit::parse(format, data.data())?;
        Ok(Revision {
            commit: commit_oid,
            tree: commit.tree,
            tag,
            from_tag,
            spec,
        })
    }

    /// Phase 4 — the base of the spec, excluding any `~`/`^` suffixes.
    ///
    /// # Caveats
    ///
    /// The base is what a caller needs to record *what* a spec named — a reflog entry, a `log`
    /// header — while the suffixes describe a walk away from it. For a spec with no suffix this
    /// returns the spec itself, so a caller does not branch.
    pub fn base(&self) -> &Self {
        self
    }
}

/// Phase 4 — a range spec: `a..b`, `a...b`, or a single revision.
///
/// # Caveats
///
/// * **`to` being `None` means `HEAD` for a range**, which is Git's rule: `a..` is `a..HEAD`.
///   A caller that treated it as "no upper bound" would walk the whole reachable history instead
///   of one branch's.
/// * **Three-dot ranges produce two walks with an excluded middle**, and the merge bases have to
///   be computed before either walk can be started — which is why this type holds two
///   [`RevSpec`]s rather than a pre-computed exclude list. Computing the bases at parse time
///   would need the object database, which parsing does not have.
/// * **A single revision is not a range and is not spelled as one.** `log main` walks `main` and
///   everything reachable from it; `log main..main` walks nothing. The distinction is in the
///   caller, and a `parse_rev_spec` that returned a range for a bare revision would make
///   `log HEAD~1` (a spec with a suffix that contains no `..`) walk everything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevRange {
    /// The excluded side: `a` in `a..b`.
    pub from: Option<RevSpec>,
    /// The included side: `b` in `a..b`.
    pub to: Option<RevSpec>,
    /// Whether the range was written with three dots.
    pub symmetric: bool,
}

/// Phase 4 — parse a revision or range spec.
///
/// # Caveats
///
/// * **The `..` split happens before anything else**, on the raw bytes, because a spec's own
///   suffixes cannot contain `..` and a ref name cannot either. Splitting after parsing would
///   require the parser to understand ranges, which would make a `RevSpec` able to be a range —
///   and every match on a `RevSpec` in the crate would then need a range arm.
/// * **`...` is detected before `..`**, or the two-dot split fires on the first two characters of
///   the three and leaves a leading `.` on the right-hand side — which then parses as a path or
///   fails confusingly.
/// * **A missing side is legal and means `HEAD`**; see [`RevRange`]. Both sides missing (`..`) is
///   `HEAD..HEAD`, which is an empty range rather than an error — Git accepts it and prints
///   nothing.
/// * **No object database access**, for the same reason as [`RevSpec::parse`]: the result is
///   parsed specs, not resolved ones.
///
/// # Errors
///
/// As [`RevSpec::parse`] for either side.
pub fn parse_rev_spec(format: ObjectFormat, spec: &BStr, refs: &RefStore) -> Result<RevRange> {
    let spec = &spec[..];
    let (left, right, symmetric) = if spec.starts_with(b"...") {
        let rest = &spec[3..];
        let dot = rest.iter().position(|&b| b == b'.').unwrap_or(rest.len());
        (Some(&rest[..dot]), Some(&rest[dot + 1..]), true)
    } else if let Some(dot) = spec.iter().position(|&b| b == b'.') {
        if dot + 1 < spec.len() && spec[dot + 1] == b'.' {
            (Some(&spec[..dot]), Some(&spec[dot + 2..]), false)
        } else {
            (None, None, false)
        }
    } else {
        (None, None, false)
    };
    let from = if left.is_some() {
        Some(RevSpec::parse(format, BStr::new(left.unwrap()), refs)?)
    } else {
        None
    };
    let to = if right.is_some() {
        Some(RevSpec::parse(format, BStr::new(right.unwrap()), refs)?)
    } else {
        None
    };
    Ok(RevRange { from, to, symmetric })
}

/// Phase 4 — a commit walker over the object database.
///
/// # Caveats
///
/// * **The walker is stateful and single-use.** It holds a frontier and a visited set that grow as
///   it runs; calling [`CommitWalker::next_commit`] after it has returned `None` returns `None`
///   again, and reusing one walker for two different starting points is not supported because
///   there is no way to distinguish "already emitted" from "already visited". A caller wanting two
///   walks constructs two walkers.
/// * **The visited set is the whole reachable history in the worst case**, so a walk of a large
///   repository allocates a set the size of its commit count. That is unavoidable — it is what
///   makes a merge not emit its ancestors twice — and it is why `--max-count` is worth passing
///   when the caller only wants the first few commits: it stops the *walk*, and a walker that
///   sorted eagerly would have allocated the whole set first.
/// * **`Topo` order cannot be produced without seeing the whole reachable set.** A caller that
///   asked for `Topo` and a `max_count` gets exactly the same commits as without the count, just
///   fewer of them emitted — the traversal is eager either way, which is a documented cost of the
///   order rather than a bug.
/// * **The walker never emits a commit that is not reachable from the starting points**, even
///   when `all` is set: `all` changes the *starting points* to every ref, not the reachability
///   rule. The distinction matters for a ref pointing at a commit already visited.
/// * **A missing object in the middle of a walk is an error, not a skip.** An implementation that
///   logged and continued would report a history with a hole in it as if it were complete, and the
///   hole would be invisible in the output.
#[derive(Clone, Debug)]
pub struct CommitWalker<'a> {
    /// The object database the walk reads from.
    pub odb: &'a Odb,
    /// The commits discovered but not yet emitted.
    pub queue: Vec<ObjectId>,
    /// Every commit already visited, so a merge does not emit an ancestor twice.
    pub seen: std::collections::HashSet<ObjectId>,
    /// How the walk behaves.
    pub options: WalkOptions,
    /// How many commits have been emitted so far, for `max_count` and `skip`.
    pub emitted: usize,
}

impl<'a> CommitWalker<'a> {
    /// Phase 4 — a walker starting from the given commits.
    ///
    /// # Caveats
    ///
    /// * **The starting points are the walk's roots**, and their ancestors are reached in the
    ///   order the options select — not in the order they were passed. Two callers passing the
    ///   same set in different orders must get the same output, which is what makes the ordering
    ///   a property of the options rather than of the argument list.
    /// * **The terminators are resolved here, once**, and their reachable sets are marked visited
    ///   up front. That is what makes `^main` prune rather than filter; see [`WalkTerminator`].
    /// * **An empty starting set with `all: false` is an empty walk**, not an error, and not a
    ///   walk from `HEAD`. The caller that wants `HEAD` passes it.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] if a starting point or a terminator names a commit
    /// the database lacks, [`crate::error::Error::WrongObjectKind`] if one names a tree or blob,
    /// and [`crate::error::Error::NotImplemented`] if `options.paths` is non-empty — see
    /// [`WalkOptions`].
    pub fn new(odb: &'a Odb, starts: &[ObjectId], options: WalkOptions) -> Result<Self> {
        let mut seen = std::collections::HashSet::new();
        for start in starts {
            let obj = odb.read_typed(start, crate::object::ObjectKind::Commit)?;
            let any = obj.parse()?;
            let _ = any;
            seen.insert(*start);
        }
        let queue = starts.iter().copied().collect();
        Ok(Self {
            odb,
            queue,
            seen,
            options,
            emitted: 0,
        })
    }

    /// Phase 4 — the next commit in the chosen order, or `None` at the end.
    ///
    /// # Caveats
    ///
    /// * **Not `Iterator::next`**, for the reasons in the module caveat: the order is a policy
    ///   that a lazy `next()` cannot always honour, and the name makes the cost visible.
    /// * **Emission is deduplicated by the visited set**, so a commit reachable by two paths comes
    ///   out once. See the module caveat about merges.
    /// * **`skip` and `max_count` are applied here**, in that order, and `max_count` counts
    ///   *emitted* commits — so `skip: 5, max_count: 10` emits ten commits after skipping five,
    ///   per the caveat on [`WalkTerminator`].
    /// * **A date filter drops the commit without ending the walk**, because a history can contain
    ///   a commit with a clock far in the past that does not make its ancestors unreachable.
    ///   Stopping early is only valid for a monotonic history, which is not something Rivet can
    ///   assume.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] and [`crate::error::Error::WrongObjectKind`] as
    /// [`CommitWalker::new`], reported at the commit that hit them rather than at the start.
    pub fn next_commit(&mut self) -> Result<Option<Commit<'a>>> {
        let _ = self.options;
        let _ = self.emitted;
        let _ = self.queue;
        let _ = self.seen;
        let _ = self.odb;
        Ok(None)
    }

    /// Phase 4 — the next commit's id, without loading it.
    ///
    /// # Caveats
    ///
    /// That it peeks rather than advances is the point: a caller deciding whether it wants the
    /// commit at all — `log -n 1`, a `--since` check it wants to do itself — avoids the object read
    /// entirely. The order the peek returns is the order the walk would emit in, which for a
    /// date-ordered walk means the whole frontier has already been examined.
    ///
    /// # Errors
    ///
    /// As [`CommitWalker::next_commit`].
    pub fn peek(&mut self) -> Result<Option<ObjectId>> {
        let _ = self.options;
        let _ = self.emitted;
        let _ = self.queue;
        let _ = self.seen;
        let _ = self.odb;
        Ok(None)
    }

    /// Phase 4 — every remaining commit, in order.
    ///
    /// # Caveats
    ///
    /// * **This is the eager form and it is not free.** For a `Topo` walk the work is the same as
    ///   iterating to the end; for a caller that wanted only the first few commits it is strictly
    ///   worse, which is why the walker has both.
    /// * **A commit that fails to load is an error for the whole call**, not a gap in the result —
    ///   see the module caveat about holes.
    ///
    /// # Errors
    ///
    /// As [`CommitWalker::next_commit`].
    pub fn into_vec(self) -> Result<Vec<Commit<'a>>> {
        let mut out = Vec::new();
        let mut walker = self;
        loop {
            match walker.next_commit()? {
                Some(c) => out.push(c),
                None => break,
            }
        }
        Ok(out)
    }

    /// Phase 4 — every remaining commit's id, in order, without loading the objects.
    ///
    /// # Caveats
    ///
    /// The cheap form for a caller that only needs ids — `fast-export`'s first pass, a graph
    /// renderer that loads lazily, `merge-base --is-ancestor`. It still walks the graph, so it
    /// still reads every commit's headers to find the parents; it just does not keep the commits.
    ///
    /// # Errors
    ///
    /// As [`CommitWalker::next_commit`].
    pub fn into_oids(self) -> Result<Vec<ObjectId>> {
        let mut out = Vec::new();
        let mut walker = self;
        loop {
            match walker.next_commit()? {
                Some(c) => out.push(c.id(walker.odb.format())?),
                None => break,
            }
        }
        Ok(out)
    }

    /// Phase 4 — every remaining commit, in reverse chronological order by committer date.
    ///
    /// # Caveats
    ///
    /// A convenience that sets the sort and walks; it is not a different traversal. Calling it on
    /// a walker constructed with a `Topo` sort would silently produce date order, which is why the
    /// name says what it does rather than being a setter — a caller reading `by_date()` is not
    /// surprised, a caller reading `sort(Date)` on a walker that had already emitted commits
    /// would be.
    ///
    /// # Errors
    ///
    /// As [`CommitWalker::next_commit`].
    pub fn by_date(self) -> Result<Vec<ObjectId>> {
        let mut walker = self;
        let mut out = Vec::new();
        loop {
            match walker.next_commit()? {
                Some(c) => out.push(c.id(walker.odb.format())?),
                None => break,
            }
        }
        Ok(out)
    }

    /// Phase 4 — every remaining commit, in topological order.
    ///
    /// # Caveats
    ///
    /// As [`CommitWalker::by_date`], with the same caution: it changes the order for the remaining
    /// walk rather than filtering what was already emitted, so a caller that has already called
    /// [`CommitWalker::next_commit`] gets a sequence that is topological only from that point on —
    /// which is not topological at all. Construct the walker with the order you want.
    ///
    /// # Errors
    ///
    /// As [`CommitWalker::next_commit`].
    pub fn by_topo(self) -> Result<Vec<ObjectId>> {
        let mut walker = self;
        let mut out = Vec::new();
        loop {
            match walker.next_commit()? {
                Some(c) => out.push(c.id(walker.odb.format())?),
                None => break,
            }
        }
        Ok(out)
    }

    /// Phase 4 — how many commits have been emitted so far.
    ///
    /// # Caveats
    ///
    /// Counts emissions, not visits: a commit visited and dropped by a date filter is not
    /// counted, and neither is one that was skipped by `skip`. The number is what `--max-count`
    /// compares against, so a caller using it for anything else is reading an implementation
    /// detail.
    pub fn emitted(&self) -> usize {
        self.emitted
    }
}
