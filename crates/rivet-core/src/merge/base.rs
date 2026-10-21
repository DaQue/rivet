//! Phase 9 — the merge base: the best common ancestor of two commits.
//!
//! This is the only part of the merge that is a graph algorithm, and it is the part most often
//! implemented wrongly. The definition Git uses:
//!
//! > A merge base of `A` and `B` is a commit that is an ancestor of both, and that is **not** an
//! > ancestor of any other commit that is an ancestor of both.
//!
//! The second clause is the whole of it. "The first commit reachable from both sides" is a
//! different and much easier question, and the answer it gives is a commit whose tree is *not*
//! the right base for the three-way merge — the merge would then see as "changed by us" a change
//! that came from the other branch's history, and either conflict spuriously or, worse, apply a
//! change twice.
//!
//! # The walk
//!
//! Plan §4 Phase 5 specifies the shape: breadth-first from both tips, marking the generation
//! (distance) of each commit, until a commit is reached by both searches. The candidates are then
//! the reachable set, filtered down to those not reachable from another candidate. Rivet
//! implements the same two-phase structure, and it must not be replaced by "the newest common
//! ancestor by committer date" — commit dates are user-controlled, are wrong in imported
//! histories, and can be equal, so ordering by them is both incorrect and non-deterministic.
//!
//! # Caveats that bite
//!
//! * **There can be more than one base.** The criss-cross merge — `A` and `B` each merge the
//!   other twice — produces two maximal common ancestors, and no amount of tie-breaking makes
//!   one of them *the* base. Git's `recursive` strategy merges the bases together to produce a
//!   virtual base and then does the real merge against that; Rivet returns the set and picks one,
//!   which is a documented difference that must be visible rather than hidden (plan §4 Phase 5
//!   and §5).
//! * **`MAX_MERGE_BASE_STEPS` bounds the walk.** A commit graph is finite and acyclic in a
//!   well-formed repository, but "well-formed" is an assumption about untrusted bytes: a commit
//!   whose parent chain loops, or a `grafts`/`replace` entry that rewrites the graph, makes the
//!   walk non-terminating. Bounding it and reporting
//!   [`crate::error::Error::NoMergeBase`] is honest; recursing until the stack overflows is not
//!   (plan §5).
//! * **An octopus merge's first parent is not special here.** A commit with three parents is
//!   reachable through all of them, and a base search that only followed `parents[0]` would
//!   return a base that is too old for the other two — the failure is silent and produces a merge
//!   with spurious conflicts.
//! * **A shallow repository has no base, and that is not corruption.** A clone with `--depth 1`
//!   has grafted history; the walk reaches the shallow boundary and stops, and the honest answer
//!   is [`crate::error::Error::NoMergeBase`] rather than a guess at the missing commits.

use crate::error::Error;
use crate::error::Result;
use crate::hash::ObjectId;
use crate::object::ObjectKind;
use crate::odb::Odb;

/// Phase 9 — how many commits the base walk may visit before giving up.
///
/// # Caveats
///
/// A bound on a walk over untrusted data, not a performance knob. A repository whose commit graph
/// is a loop — through a malformed object, a `grafts` file or a `replace` ref — would otherwise
/// make [`merge_base`] run forever, and an unbounded graph traversal on attacker-supplied input
/// is the shape of failure plan §5 is written to prevent. The value must be large enough for a
/// real history (Git's own tests include repositories with tens of thousands of commits) and
/// small enough that hitting it in an honest repository means the repository is broken.
pub const MAX_MERGE_BASE_STEPS: usize = 1_000_000;

/// Phase 9 — a merge base and the rest of the common ancestry it was chosen from.
///
/// # Caveats
///
/// * **`oid` is the base the merge actually uses**; `common` is every maximal common ancestor
///   found, which contains `oid` and may contain others. Both are needed: the merge uses one, and
///   `merge-base --all` and the diagnostics that explain a surprising conflict need the set.
/// * **The choice of `oid` among `common` is a policy, not a fact.** Git's recursive strategy
///   does something better than picking one — it merges them — and Rivet's documented difference
///   is that it picks. Any deterministic rule is defensible; an *unstable* one is not, because
///   two runs of the same merge must produce the same tree.
/// * **`common` is not the set of all common ancestors**, only the maximal ones. A caller wanting
///   "is X an ancestor of both" asks that question directly against the graph rather than
///   searching this list, which does not contain it.
/// * **An empty `common` is impossible for a successful call**, but [`MergeBase`] is a public
///   type and a caller can construct one; `oid` is therefore not "the first of `common`" by
///   construction and an accessor that assumed so would be wrong on a hand-built value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeBase {
    /// The base the merge will use.
    pub oid: ObjectId,
    /// Every maximal common ancestor found, including `oid`.
    pub common: Vec<ObjectId>,
}

impl MergeBase {
    /// Phase 9 — whether the two commits have more than one best common ancestor.
    ///
    /// # Caveats
    ///
    /// `true` is the criss-cross case and it means the merge result may differ from Git's, which
    /// merges the bases into a virtual base that Rivet does not build. This is the predicate a
    /// caller uses to decide whether to warn, so it must be cheap and it must not be `common.len()
    /// > 1` computed against a list that was deduplicated differently in two code paths.
    pub fn is_criss_cross(&self) -> bool {
        self.common.len() > 1
    }

    /// Phase 9 — whether `oid` is one of the commits being merged, i.e. whether a fast-forward is
    /// possible.
    ///
    /// # Caveats
    ///
    /// * **This is the fast-forward test and it is a graph fact, not a comparison.** `ours` being
    ///   an ancestor of `theirs` is what makes a fast-forward possible, and the merge base being
    ///   equal to `ours` is the same statement — which is why it is asked here rather than by
    ///   walking the graph a second time.
    /// * **It says nothing about whether the fast-forward should happen.** (the text continues in
    ///   [`crate::merge::MergeOptions::no_ff`].) The predicate is "possible", the decision is the
    ///   caller's.
    pub fn allows_fast_forward(&self, ours: ObjectId) -> bool {
        self.common.len() == 1 && self.common[0] == ours
    }
}

/// Phase 9 — the best common ancestor of two commits, for the merge to use.
///
/// # Caveats
///
/// * **This is the common case and [`merge_bases`] is the general one.** A caller that needs to
///   know whether the history criss-crosses calls the latter; this one picks a base and discards
///   the rest, so the information is gone by the time it returns.
/// * **`ours` and `theirs` are symmetric here** — the set of maximal common ancestors does not
///   depend on the order — but the *choice* made in `MergeBase::oid` may, so a caller must pass
///   them the way the merge will use them. Swapping them can select a different base and produce
///   a different tree for a criss-cross history, and no error.
/// * **Equal inputs are legal and return themselves.** `merge_base(x, x)` is `x`, which is what
///   every branch's own merge against itself computes and what makes a no-op merge a no-op rather
///   than [`crate::error::Error::NoMergeBase`].
///
/// # Errors
///
/// [`crate::error::Error::NoMergeBase`] when the two commits have no common ancestor — unrelated
/// histories, or a shallow clone whose grafted boundary hides one — and when the walk exceeds
/// [`MAX_MERGE_BASE_STEPS`]; [`crate::error::Error::ObjectNotFound`] for a commit id the database
/// lacks; [`crate::error::Error::WrongObjectKind`] if an id names a tree, a blob or a tag.
pub fn merge_base(odb: &Odb, ours: ObjectId, theirs: ObjectId) -> Result<MergeBase> {
    if ours == theirs {
        return Ok(MergeBase {
            oid: ours,
            common: vec![ours],
        });
    }
    let bases = merge_bases(odb, ours, theirs)?;
    if bases.is_empty() {
        return Err(Error::NoMergeBase { ours, theirs });
    }
    Ok(MergeBase {
        oid: bases[0],
        common: bases,
    })
}

/// Phase 9 — every maximal common ancestor of two commits.
///
/// # Caveats
///
/// * **The result is sorted and deduplicated**, so a caller can compare two runs; the sort order
///   is by object id, not by date, for the reason in the module caveat. An implementation that
///   returned candidates in discovery order would make the output depend on the walk's
///   breadth-first ordering, which is stable today and is not part of the contract.
/// * **The list is not "all common ancestors"**; see [`MergeBase::common`]. `git merge-base
///   --all` prints maximal ones and this matches it.
/// * **An empty result is the error case, not an empty `Ok`.** A caller asking for the bases of
///   two unrelated commits gets [`crate::error::Error::NoMergeBase`], because the alternative is
///   an empty `Vec` that a caller treats as "no base needed" and then merges against nothing.
///
/// # Errors
///
/// As [`merge_base`].
pub fn merge_bases(odb: &Odb, ours: ObjectId, theirs: ObjectId) -> Result<Vec<ObjectId>> {
    if ours == theirs {
        return Ok(vec![ours]);
    }
    let mut steps = 0usize;
    let ours_ancestors = ancestors(odb, ours, &mut steps)?;

    // Walk back from `theirs`; the first commits found that `ours` can also reach are the
    // candidates, and their own ancestors are common but never maximal, so the walk stops there.
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::from([theirs]);
    while let Some(oid) = queue.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        if ours_ancestors.contains(&oid) {
            candidates.push(oid);
            continue;
        }
        steps += 1;
        if steps > crate::merge::MAX_MERGE_BASE_STEPS {
            return Err(Error::NoMergeBase { ours, theirs });
        }
        queue.extend(parents_of(odb, oid)?);
    }

    // A candidate that another candidate can reach is not a *best* common ancestor.
    let mut bases = Vec::new();
    for (i, candidate) in candidates.iter().enumerate() {
        let mut dominated = false;
        for (j, other) in candidates.iter().enumerate() {
            if i != j && ancestors(odb, *other, &mut steps)?.contains(candidate) {
                dominated = true;
                break;
            }
        }
        if !dominated {
            bases.push(*candidate);
        }
    }
    bases.sort();
    bases.dedup();
    if bases.is_empty() {
        return Err(Error::NoMergeBase { ours, theirs });
    }
    Ok(bases)
}

/// The parents a commit records.
fn parents_of(odb: &Odb, oid: ObjectId) -> Result<Vec<ObjectId>> {
    let object = odb.read_typed(&oid, ObjectKind::Commit)?;
    Ok(crate::object::Commit::parse(odb.format(), object.data())?.parents)
}

/// Every commit reachable from `start`, itself included.
fn ancestors(
    odb: &Odb,
    start: ObjectId,
    steps: &mut usize,
) -> Result<std::collections::HashSet<ObjectId>> {
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![start];
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        *steps += 1;
        if *steps > crate::merge::MAX_MERGE_BASE_STEPS {
            return Err(Error::NoMergeBase {
                ours: start,
                theirs: start,
            });
        }
        stack.extend(parents_of(odb, oid)?);
    }
    Ok(seen)
}
