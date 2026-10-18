//! Phase 9 — merge: finding the common ancestor, combining three trees, and reporting conflicts.
//!
//! A merge in Git is four separate problems that happen to be run in order, and conflating any
//! two of them is how a merge implementation ends up subtly wrong:
//!
//! 1. **The merge base** ([`merge_base`]) — the best common ancestor of two commits, found by
//!    walking the commit graph. This is a *graph* problem and it is the one with real algorithmic
//!    content: the answer is not "the first common ancestor you meet" but "a common ancestor that
//!    is not an ancestor of any other common ancestor", and there can legitimately be several.
//! 2. **The tree merge** ([`merge_trees`]) — a three-way walk of base, ours and theirs, deciding
//!    per path whether the change is one-sided, both-sided and identical, both-sided and
//!    different, or a delete/modify disagreement.
//! 3. **The content merge** ([`merge_content`]) — for a path both sides changed, a line-level
//!    three-way merge with conflict markers where the two changes overlap.
//! 4. **The state** ([`write_merge_state`], [`MergeState`]) — the files that make the merge
//!    resumable: `MERGE_HEAD`, `MERGE_MSG`, `ORIG_HEAD`, and the index's conflict stages.
//!
//! # The conflict exit contract
//!
//! Plan §4 Phase 5 fixes this, and it is the part a caller is most likely to get wrong:
//!
//! * **A merge with conflicts is not an error to roll back.** It returns
//!   [`crate::error::Error::MergeConflicts`] carrying the paths, having *already written* the
//!   conflict markers to the work tree, the conflict stages to the index, and `MERGE_HEAD` to the
//!   git directory. The state on disk is the intended output; a caller that caught the error and
//!   cleaned up would destroy the user's ability to resolve it.
//! * **`Status` shows the conflicted paths as unmerged**, from the index's stages, not from any
//!   in-memory list. A merge implementation that tracked its own conflict list would disagree
//!   with `status` the moment the user resolved one with `add`.
//! * **The merge is finished with `add` and `commit`**, not with a merge-specific command:
//!   `add` on a conflicted path drops its stages and writes a stage-0 entry, and `commit` refuses
//!   while any path still has stages. `commit` then writes a commit with two parents, having read
//!   `MERGE_HEAD`.
//! * **`merge --abort` restores both the index and the work tree** to `ORIG_HEAD`'s tree, and it
//!   must refuse — not guess — when there is no merge in progress or when the work tree has
//!   changes unrelated to the merge.
//!
//! # Caveats that bite
//!
//! * **`MergeBase` is a set, not an id.** Two branches can have several best common ancestors
//!   (the "criss-cross merge" case), and Git picks one for `recursive`'s first pass and then
//!   merges the bases together for the second. Rivet returns the set and picks one, and a caller
//!   that assumed uniqueness would silently produce a different tree than Git on a criss-cross
//!   history — a real repository and not a corner case.
//! * **A merge base is not "the newest common ancestor by date".** Commit dates are
//!   user-supplied and routinely wrong; the algorithm is by graph reachability. Sorting
//!   candidates by committer date is the classic wrong implementation and it produces a
//!   different (and worse) merge result rather than an error.
//! * **Three-way merge is not "apply the diff"**. It is a per-line decision over three inputs,
//!   and a hunk that both sides changed in the same way is *not* a conflict; a hunk that only one
//!   side changed is that side's version with no marker at all. The marker appears only where the
//!   two changes genuinely overlap, and "genuinely overlap" is the part that differs between
//!   merge algorithms.
//! * **Conflict markers are written with a fixed width of seven `=`**, and the labels are
//!   `<branch>` / `HEAD` / the other side's description. `merge.conflictStyle` changes the shape
//!   (see [`ConflictStyle`]), and the `zdiff3` style is not merely cosmetic — it narrows the
//!   conflicted region, so the same merge produces different *content* under different styles.
//! * **`merge` does not touch the work tree for paths it does not have to.** A path that only
//!   one side changed is written; a path neither side changed is not touched at all, which is
//!   what keeps a merge from rewriting the mtime of every file in the repository.
//! * **The index's conflict stages are the merge's record, and they are per path.** A path with
//!   stages 1/2/3 has a base, an ours and a theirs; a modify/delete conflict has only two of the
//!   three (there is no base entry when the path did not exist in the base, and no ours or
//!   theirs entry when that side deleted it). A consumer that assumed three entries per
//!   conflict reads past the end of the list.

use bstr::BString;

use crate::hash::ObjectId;

pub mod base;
pub mod content;
pub mod state;
pub mod tree;

pub use base::{MAX_MERGE_BASE_STEPS, MergeBase, merge_base, merge_bases};
pub use content::{ContentMerge, merge_content};
pub use state::{
    MERGE_HEAD, MERGE_MSG, MergeState, ORIG_HEAD, abort_merge, clear_merge_state,
    merge_in_progress, read_merge_state, write_merge_state, write_orig_head,
};
pub use tree::{
    merge_path, merge_trees, merge_worktree_paths, write_conflict_stages, write_merged_worktree,
};

/// Phase 9 — which side of a merge a value came from.
///
/// # Caveats
///
/// * **`Ours` is `HEAD` — the branch being merged *into*** — and `Theirs` is the commit being
///   merged in, which is the opposite of what a reader of the command `git merge feature`
///   expects when they think of "theirs" as "the other branch's changes". The naming is Git's,
///   and every user-facing message inherits the confusion; get it backwards and `--ours` /
///   `--theirs` in a resolution checkout silently take the wrong side.
/// * **Neither side is privileged in a content merge**, despite the names: a conflict is
///   symmetric, and the marker order (`ours` first) is a convention of the output rather than a
///   statement about which change is preferred.
/// * **A merge *result* belongs to neither side.** The merged tree is a new tree; the only places
///   these variants appear are conflict markers, the index's stages 2 and 3, and the messages
///   that explain a conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MergeSide {
    /// `HEAD`: the commit being merged into.
    Ours,
    /// The commit named on the command line, being merged in.
    Theirs,
}

/// Phase 9 — what kind of disagreement a conflicted path represents.
///
/// # Caveats
///
/// * **The variant determines which index stages exist**, and a consumer that assumed all three
///   would read a stage that is not there: [`ConflictKind::AddAdd`] has no stage 1,
///   [`ConflictKind::ModifyDelete`] has no stage 3, [`ConflictKind::DeleteModify`] has no stage
///   2. See [`Conflict`]'s `base`/`ours`/`theirs` options.
/// * **[`ConflictKind::Content`] is the only kind that writes conflict markers.** The others are
///   resolved by choosing a whole version — or by the user deciding to delete or keep the file —
///   and writing `<<<<<<<` into a file whose other side is absent is meaningless. `git checkout
///   --ours <path>` is the resolution for those, and it is why `status` prints them differently.
/// * **[`ConflictKind::Binary`] is a content conflict Rivet cannot merge textually.** Markers in a
///   binary file would corrupt it, so the file is left as one side's content (Git's rule: ours,
///   unless the other side is the one that changed) and the conflict is recorded in the index.
///   This is the case a user hits with an image or an `.xlsx` and it must not produce garbage.
/// * **[`ConflictKind::Submodule`] is a gitlink disagreement.** The two sides name different
///   commits in another repository; neither is looked up, and the resolution is a choice rather
///   than a merge. There is no content and no marker.
/// * **[`ConflictKind::Mode`] is a mode disagreement with equal content** — a file that one side
///   made executable and the other removed the executable bit from. Git treats it as a conflict
///   with a single entry at stage 2 and 3 and identical blobs, which is why the marker machinery
///   must not run for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConflictKind {
    /// Both sides changed the same lines of a text file.
    Content,
    /// Both sides created the path, with different content.
    AddAdd,
    /// We modified it, they deleted it.
    ModifyDelete,
    /// We deleted it, they modified it.
    DeleteModify,
    /// Both sides changed the mode and the content differs.
    Mode,
    /// Both sides changed the content and one side is not text.
    Binary,
    /// Both sides moved the gitlink to a different commit.
    Submodule,
}

/// Phase 9 — how conflict markers should be written.
///
/// # Caveats
///
/// * **This is `merge.conflictStyle` and it changes the file's bytes, not just its rendering.**
///   `diff3` and `zdiff3` insert a `|||||||` section carrying the base version, so the same merge
///   produces a different file — and one that a later `add` will stage, hashing differently. A
///   caller that treated this as a display option would produce a commit that does not match the
///   one Git produces from the same merge.
/// * **[`ConflictStyle::Zd3`] narrows the conflicted region** rather than only adding the base
///   section: it trims lines common to both sides from the ends of the conflict, so the marker
///   blocks are smaller and a resolution is easier to read. Its output is not a superset of
///   [`ConflictStyle::Merge`]'s.
/// * **The default is [`ConflictStyle::Merge`] and it is spelled `merge` in config.** A caller
///   reading a config value passes `zdiff3`, not `zd3`; Git's spelling and the Rust variant name
///   differ, and the mapping lives where the config is read (the CLI), not here.
/// * **Nothing else about the markers is configurable.** The seven-character width and the
///   `<<<<<<< ` prefix are fixed by the format and by `git apply`'s conflict handling; a
///   "wider markers" option would produce a file no tool could resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum ConflictStyle {
    /// `<<<<<<< ours` / `=======` / `>>>>>>> theirs`. Git's default.
    #[default]
    Merge,
    /// Adds a `||||||| base` section between ours and the separator.
    Diff3,
    /// `diff3` with the common lines trimmed from the ends of the conflict.
    Zd3,
}

/// Phase 9 — the line ranges the two sides' conflicting changes cover.
///
/// # Caveats
///
/// * **The indices are into the *base* file and the two sides are described in its coordinates**
///   in the `diff3` styles, which is what makes the base section printable. In
///   [`ConflictStyle::Merge`] the ranges are still recorded — the marker text does not show them —
///   so a resolver can reconstruct the alignment.
/// * **A side's length can be zero**, which means that side deleted the region rather than
///   replacing it. That is a real case (one side deletes a line, the other modifies a different
///   line) and it is not a conflict unless the regions overlap; a zero-length range that reaches
///   here is a conflict because it did overlap.
/// * **The ranges are half-open** — `start` is the first line and `start + len` is one past the
///   last — matching the line indices everywhere else in the crate, and *not* matching a hunk
///   header's 1-based inclusive spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct ConflictHunk {
    /// First line index in ours.
    pub ours_start: usize,
    /// Number of lines ours covers.
    pub ours_len: usize,
    /// First line index in theirs.
    pub theirs_start: usize,
    /// Number of lines theirs covers.
    pub theirs_len: usize,
    /// First line index in the base.
    pub base_start: usize,
    /// Number of lines the base covers.
    pub base_len: usize,
}

/// Phase 9 — one conflicted path.
///
/// # Caveats
///
/// * **`base`, `ours` and `theirs` are all optional and that is not a defect**; see
///   [`ConflictKind`]. A modify/delete conflict has a base and an ours and no theirs, which is
///   exactly what the index's stages will hold.
/// * **`hunks` is empty for every kind but [`ConflictKind::Content`] and
///   [`ConflictKind::Binary`]**, and empty for a binary conflict too — the binary case has no
///   line structure to describe. A caller rendering conflict hunks must branch on `kind` first.
/// * **`path` is the path the conflict is *reported under***, which for a delete/modify is the
///   name the file had. The work tree may or may not have a file there afterwards depending on
///   which side kept it, and the honest source for that is the work tree, not this struct.
/// * **`message` is optional and usually absent.** Git writes a `CONFLICT (content): ...`
///   advisory to stderr rather than into the repository; a merge driver may leave something more
///   useful, and the field is here so that a driver's message has somewhere to go without a
///   new error variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The path the conflict is reported under.
    pub path: BString,
    /// What kind of disagreement it is.
    pub kind: ConflictKind,
    /// The base version's id, when the path existed in the base.
    pub base: Option<ObjectId>,
    /// Our version's id, when our side has the path.
    pub ours: Option<ObjectId>,
    /// Their version's id, when their side has the path.
    pub theirs: Option<ObjectId>,
    /// The conflicting regions, for a content conflict.
    pub hunks: Vec<ConflictHunk>,
    /// An optional advisory message from a merge driver.
    pub message: Option<BString>,
}

impl Conflict {
    /// Phase 9 — whether resolving this conflict means deleting the work tree file.
    ///
    /// # Caveats
    ///
    /// A delete/modify conflict is *resolvable* by deleting, and the resolver's default for it is
    /// not obvious — this answers "is that a legal resolution", not "is that what should happen".
    /// The caller decides, and the answer here is that a modify/delete with the modification on
    /// our side is deletable and one with the deletion on our side is not (we would be deleting
    /// their change, which is the merge's whole content).
    pub fn resolvable_by_deleting(&self) -> bool {
        self.kind == ConflictKind::ModifyDelete && self.ours.is_some()
    }

    /// Phase 9 — whether this conflict is one a `--ours` checkout resolves.
    ///
    /// # Caveats
    ///
    /// True for every kind where our side has a version to check out, which is all of them except
    /// [`ConflictKind::DeleteModify`] — there our side deleted the path, so there is nothing to
    /// check out and the resolution is to remove it instead.
    pub fn resolvable_with_ours(&self) -> bool {
        self.kind != ConflictKind::DeleteModify
    }

    /// Phase 9 — the index stages this conflict occupies, as `(stage, id)` pairs.
    ///
    /// # Caveats
    ///
    /// * **The order is stage order (1, 2, 3) and missing stages are skipped**, so a
    ///   delete/modify yields two pairs and an add/add yields two (stages 2 and 3, with no base).
    ///   A caller writing the index from this gets the stages right by construction; one that
    ///   assumed three pairs writes a base entry that does not exist.
    /// * **The ids are the pre-merge versions, not the merge result.** Nothing here is written to
    ///   the object database by this function; the caller has already stored whatever blobs the
    ///   file merge produced.
    pub fn stages(&self) -> Vec<(crate::index::Stage, ObjectId)> {
        let mut out = Vec::new();
        if let Some(id) = self.base {
            out.push((crate::index::Stage::from_bits(1).expect("stage"), id));
        }
        if let Some(id) = self.ours {
            out.push((crate::index::Stage::from_bits(2).expect("stage"), id));
        }
        if let Some(id) = self.theirs {
            out.push((crate::index::Stage::from_bits(3).expect("stage"), id));
        }
        out
    }
}

/// Phase 9 — how a merge should behave.
///
/// # Caveats
///
/// * **`find_renames` is accepted and not implemented** (plan §4 Phase 8 puts rename detection
///   after the merge path works). With it `false` — the only behaviour Rivet has — a rename is a
///   delete plus an add, which means a merge *across* a rename conflicts where Git would follow
///   the rename and merge the content cleanly. That is a real behavioural difference on real
///   histories and it must be documented for users rather than silently accepted.
/// * **`conflict_style` changes what is written to disk**, per [`ConflictStyle`]; it is not a
///   display option and a caller must not pass it speculatively.
/// * **`diff_algorithm` is threaded through to the content merge's line diff**, and a histogram
///   selection must fail with [`crate::error::Error::NotImplemented`] before any file is
///   touched — a merge that failed halfway through would leave the index in a state that neither
///   `merge --abort` nor a re-run could describe.
/// * **`no_ff` is the fast-forward switch**, and `false` is *not* "always create a merge commit":
///   Git fast-forwards by default when the merge base is one of the two commits, and `no_ff`
///   forces a real merge commit there instead. A caller that reads it as "allow fast-forward"
///   has it exactly backwards.
/// * **There is no strategy option.** Git's `ort`/`recursive`/`ours`/`subtree` strategies select
///   different *algorithms*; Rivet has one, and naming the others as variants before they exist
///   would make every caller handle cases that would have to be [`crate::error::Error::NotImplemented`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeOptions {
    /// Attempt to detect renames. Not implemented.
    pub find_renames: bool,
    /// How conflict markers are written.
    pub conflict_style: ConflictStyle,
    /// Which line diff the content merge uses.
    pub diff_algorithm: crate::diff::DiffAlgorithm,
    /// Create a merge commit even when a fast-forward would be possible.
    pub no_ff: bool,
}

impl MergeOptions {
    /// Phase 9 — the options a plain `rivet merge <commit>` uses: no rename detection, `merge`
    /// conflict style, Myers line diff, fast-forward allowed.
    pub fn new() -> Self {
        Self {
            find_renames: false,
            conflict_style: ConflictStyle::Merge,
            diff_algorithm: crate::diff::DiffAlgorithm::Myers,
            no_ff: false,
        }
    }

    /// Phase 9 — set the conflict marker style.
    pub fn conflict_style(&mut self, style: ConflictStyle) -> &mut Self {
        self.conflict_style = style;
        self
    }

    /// Phase 9 — force a merge commit even when a fast-forward is possible.
    pub fn no_ff(&mut self, on: bool) -> &mut Self {
        self.no_ff = on;
        self
    }
}

impl Default for MergeOptions {
    /// Phase 9 — the defaults of [`MergeOptions::new`].
    fn default() -> Self {
        Self {
            conflict_style: ConflictStyle::default(),
            no_ff: false,
            ..Default::default()
        }
    }
}

/// Phase 9 — what happened to one path in a tree merge.
///
/// # Caveats
///
/// * **The result id is `None` for a path the merge deletes**, which is a legal outcome of a
///   three-way merge and not an error. A caller that unwrapped it would panic on every merge that
///   removes a file.
/// * **A clean path's result is a real object**, either one of the inputs (when only one side
///   changed) or a newly written blob (when both sides changed the same way, or when a content
///   merge combined them without conflict). The merge writes it to the database before returning
///   it here, so a caller that discards the outcome has still written objects — which is
///   harmless, because objects are content-addressed and unreferenced ones are collected later.
/// * **`kind` is `None` for a clean path**, which is what distinguishes "merged without
///   conflict" from "resolved by taking one side"; the latter never appears here, because taking
///   a side is what the user does *after* a conflict, not something the merge does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathMerge {
    /// The path.
    pub path: BString,
    /// The resulting blob id, or `None` when the merge deletes the path.
    pub result: Option<ObjectId>,
    /// The conflict kind, or `None` when the path merged cleanly.
    pub kind: Option<ConflictKind>,
}

/// Phase 9 — the result of merging three trees.
///
/// # Caveats
///
/// * **`tree` is `None` when there were conflicts**, because a tree cannot represent a path with
///   three versions. There is no partial tree to hand back: the caller gets the conflict list and
///   the index stages, and the tree is written after the user resolves. A caller that expected a
///   best-effort tree would be getting one that, if committed, silently drops a side.
/// * **`paths` contains every path the merge considered a change** — clean and conflicted alike —
///   and `conflicts` is the subset that conflicted. A caller that treated `paths` as "what needs
///   committing" gets the right answer; one that treated it as "what the user must resolve" does
///   not, and that is what `conflicts` is for.
/// * **`clean` is `conflicts.is_empty()`** and is stored rather than derived so that a caller
///   branching on it does not have to scan a potentially long list to ask the common question.
///   Keep the two consistent on every construction.
/// * **An empty merge is a clean merge.** Two identical trees produce no paths, no conflicts and
///   a tree equal to both inputs — which is what makes `merge` on an already-merged branch a
///   no-op rather than an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeMerge {
    /// Whether the merge completed without conflicts.
    pub clean: bool,
    /// Every path the merge changed or considered, sorted.
    pub paths: Vec<PathMerge>,
    /// The conflicted paths, sorted, with their content conflicts' hunks.
    pub conflicts: Vec<Conflict>,
    /// The merged tree's id, or `None` when conflicts prevented one from being written.
    pub tree: Option<ObjectId>,
}

/// Phase 9 — the outcome of a merge, under the name the scaffold's shared vocabulary uses.
///
/// # Caveats
///
/// An alias and nothing more: there is exactly one merge result type, and this exists because
/// the brief names it `MergeOutcome` while the operation that produces it is a tree merge. Do not
/// add a second struct behind this name — two types meaning "the result of a merge" is exactly
/// the parallel-type problem the brief's shared vocabulary is meant to prevent.
pub type MergeOutcome = TreeMerge;
