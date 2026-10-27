//! A port of the part of git's revision walk (`revision.c`) that `rev-list` ordering depends on.
//!
//! The order git prints commits in is a by-product of its algorithm, not a property of the graph:
//! a date-sorted work list where parents are inserted after equal dates, `limit_list` with its
//! clock-skew slop whenever a range, `--boundary` or a topological order is involved, and
//! `sort_in_topological_order` with a LIFO or a date priority queue. This module follows those
//! functions step by step so the same inputs give the same list, ties and skew included.
//!
//! Only what `rev-list` needs is here: no pathspec simplification, no `--ancestry-path`, no
//! cherry-pick marks.

use std::collections::{BinaryHeap, HashMap, HashSet};

use anyhow::Result;
use rivet_core::object::Commit;
use rivet_core::{ObjectId, ObjectKind, Repository};

/// How many extra commits `limit_list` inspects after everything left looks uninteresting.
const SLOP: i32 = 5;

/// The topological order requested with `--topo-order` / `--date-order` / `--author-date-order`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopoOrder {
    /// `--topo-order`: depth-first, a LIFO of commits whose children are all emitted.
    Graph,
    /// `--date-order`: a priority queue on committer date.
    CommitDate,
    /// `--author-date-order`: a priority queue on author date.
    AuthorDate,
}

/// What to walk.
#[derive(Clone, Debug, Default)]
pub struct WalkSpec {
    /// Starting points in command-line order, each flagged as uninteresting (`^X`) or not.
    pub tips: Vec<(ObjectId, bool)>,
    /// Follow only the first parent of each commit.
    pub first_parent: bool,
    /// A topological order, if one was asked for.
    pub topo: Option<TopoOrder>,
    /// `--since`: commits older than this are not shown.
    pub max_age: Option<i64>,
    /// `--until`: commits newer than this are not shown.
    pub min_age: Option<i64>,
}

/// One commit of the walk with the facts the ordering needs.
struct Node {
    parents: Vec<ObjectId>,
    date: i64,
    author_date: i64,
}

const SEEN: u8 = 1;
const UNINTERESTING: u8 = 2;
const ADDED: u8 = 4;

struct Walk<'a> {
    repository: &'a Repository,
    nodes: HashMap<ObjectId, Node>,
    flags: HashMap<ObjectId, u8>,
    first_parent: bool,
}

impl Walk<'_> {
    fn load(&mut self, oid: ObjectId) -> Result<()> {
        if self.nodes.contains_key(&oid) {
            return Ok(());
        }
        let object = self.repository.odb().read_typed(&oid, ObjectKind::Commit)?;
        let commit = Commit::parse(self.repository.format(), object.data())?;
        self.nodes.insert(
            oid,
            Node {
                parents: self.repository.odb().visible_parents(&oid, &commit),
                date: commit.committer.when,
                author_date: commit.author.when,
            },
        );
        Ok(())
    }

    fn flag(&self, oid: &ObjectId) -> u8 {
        self.flags.get(oid).copied().unwrap_or(0)
    }

    fn set(&mut self, oid: ObjectId, bits: u8) {
        *self.flags.entry(oid).or_insert(0) |= bits;
    }

    fn date(&self, oid: &ObjectId) -> i64 {
        self.nodes[oid].date
    }

    /// `commit_list_insert_by_date`: after every entry that is not older.
    fn insert_by_date(&self, list: &mut Vec<ObjectId>, oid: ObjectId) {
        let date = self.date(&oid);
        let at = list
            .iter()
            .position(|other| self.date(other) < date)
            .unwrap_or(list.len());
        list.insert(at, oid);
    }

    /// `mark_parents_uninteresting`: marks the parents of an already-parsed commit, and of those
    /// parents that were parsed too.
    fn mark_parents_uninteresting(&mut self, oid: ObjectId) {
        let mut stack = vec![oid];
        while let Some(current) = stack.pop() {
            let Some(node) = self.nodes.get(&current) else {
                continue;
            };
            let parents = node.parents.clone();
            for parent in parents {
                if self.flag(&parent) & UNINTERESTING != 0 {
                    continue;
                }
                self.set(parent, UNINTERESTING);
                // A parent only has its own parents known if it was parsed before.
                if self.nodes.contains_key(&parent) {
                    stack.push(parent);
                }
            }
        }
    }

    /// `process_parents`: enqueue the parents of `oid` by date.
    fn process_parents(&mut self, oid: ObjectId, list: &mut Vec<ObjectId>) -> Result<()> {
        if self.flag(&oid) & ADDED != 0 {
            return Ok(());
        }
        self.set(oid, ADDED);
        let parents = self.nodes[&oid].parents.clone();
        if self.flag(&oid) & UNINTERESTING != 0 {
            for parent in parents {
                self.set(parent, UNINTERESTING);
                self.load(parent)?;
                if !self.nodes[&parent].parents.is_empty() {
                    self.mark_parents_uninteresting(parent);
                }
                if self.flag(&parent) & SEEN != 0 {
                    continue;
                }
                self.set(parent, SEEN);
                self.insert_by_date(list, parent);
            }
            return Ok(());
        }
        for parent in parents {
            self.load(parent)?;
            if self.flag(&parent) & SEEN == 0 {
                self.set(parent, SEEN);
                self.insert_by_date(list, parent);
            }
            if self.first_parent {
                break;
            }
        }
        Ok(())
    }

    fn everybody_uninteresting(&self, list: &[ObjectId]) -> bool {
        list.iter().all(|oid| self.flag(oid) & UNINTERESTING != 0)
    }

    /// `limit_list`: walk everything reachable, dropping what is excluded, and return the
    /// interesting commits in the order they were found.
    fn limit_list(
        &mut self,
        mut original: Vec<ObjectId>,
        max_age: Option<i64>,
        min_age: Option<i64>,
    ) -> Result<Vec<ObjectId>> {
        let mut slop = SLOP;
        let mut date = i64::MAX;
        let mut out = Vec::new();
        while !original.is_empty() {
            let oid = original.remove(0);
            if let Some(max_age) = max_age
                && self.date(&oid) < max_age
            {
                self.set(oid, UNINTERESTING);
            }
            self.process_parents(oid, &mut original)?;
            if self.flag(&oid) & UNINTERESTING != 0 {
                self.mark_parents_uninteresting(oid);
                // `still_interesting`
                slop = if original.is_empty() {
                    0
                } else if date <= self.date(&original[0])
                    || !self.everybody_uninteresting(&original)
                {
                    SLOP
                } else {
                    slop - 1
                };
                if slop > 0 {
                    continue;
                }
                break;
            }
            if let Some(min_age) = min_age
                && self.date(&oid) > min_age
            {
                continue;
            }
            date = self.date(&oid);
            out.push(oid);
        }
        Ok(out)
    }

    /// `sort_in_topological_order`.
    fn topo_sort(&self, list: Vec<ObjectId>, order: TopoOrder) -> Vec<ObjectId> {
        let mut indegree: HashMap<ObjectId, u32> = list.iter().map(|oid| (*oid, 1)).collect();
        for oid in &list {
            for parent in &self.nodes[oid].parents {
                if let Some(count) = indegree.get_mut(parent)
                    && *count > 0
                {
                    *count += 1;
                }
            }
        }
        let key = |oid: &ObjectId| match order {
            TopoOrder::AuthorDate => self.nodes[oid].author_date,
            _ => self.nodes[oid].date,
        };
        // The queue is a LIFO for `--topo-order` and a max-heap on the date, oldest insertion
        // first among equals, for the date orders.
        let mut stack: Vec<ObjectId> = Vec::new();
        let mut heap: BinaryHeap<(i64, std::cmp::Reverse<u64>, ObjectId)> = BinaryHeap::new();
        let mut counter = 0u64;
        let mut put =
            |oid: ObjectId,
             stack: &mut Vec<ObjectId>,
             heap: &mut BinaryHeap<(i64, std::cmp::Reverse<u64>, ObjectId)>| {
                match order {
                    TopoOrder::Graph => stack.push(oid),
                    _ => {
                        heap.push((key(&oid), std::cmp::Reverse(counter), oid));
                        counter += 1;
                    }
                }
            };
        for oid in &list {
            if indegree[oid] == 1 {
                put(*oid, &mut stack, &mut heap);
            }
        }
        if order == TopoOrder::Graph {
            // The tips are shown in the order the traversal produced them.
            stack.reverse();
        }
        let mut out = Vec::with_capacity(list.len());
        loop {
            let next = match order {
                TopoOrder::Graph => stack.pop(),
                _ => heap.pop().map(|(_, _, oid)| oid),
            };
            let Some(oid) = next else { break };
            for parent in &self.nodes[&oid].parents {
                let Some(count) = indegree.get_mut(parent) else {
                    continue;
                };
                if *count == 0 {
                    continue;
                }
                *count -= 1;
                if *count == 1 {
                    put(*parent, &mut stack, &mut heap);
                }
            }
            indegree.insert(oid, 0);
            out.push(oid);
        }
        out
    }
}

/// Walk the history described by `spec` and return the interesting commits in git's order, with
/// the `--since`/`--until` window applied. Output-time filters (parent counts, `--grep`, …) are
/// the caller's.
pub fn walk(repository: &Repository, spec: &WalkSpec) -> Result<Walked> {
    let mut walk = Walk {
        repository,
        nodes: HashMap::new(),
        flags: HashMap::new(),
        first_parent: spec.first_parent,
    };
    // `prepare_revision_walk`: every tip once, in command-line order, then a stable date sort.
    let mut list: Vec<ObjectId> = Vec::new();
    let mut limited = spec.topo.is_some();
    for (oid, uninteresting) in &spec.tips {
        walk.load(*oid)?;
        if *uninteresting {
            walk.set(*oid, UNINTERESTING);
            walk.mark_parents_uninteresting(*oid);
            limited = true;
        }
        if walk.flag(oid) & SEEN == 0 {
            walk.set(*oid, SEEN);
            list.push(*oid);
        }
    }
    list.sort_by_key(|oid| std::cmp::Reverse(walk.date(oid)));

    let commits = if limited {
        let limited = walk.limit_list(list, spec.max_age, spec.min_age)?;
        match spec.topo {
            Some(order) => walk.topo_sort(limited, order),
            None => limited,
        }
        .into_iter()
        .filter(|oid| walk.flag(oid) & UNINTERESTING == 0)
        .collect()
    } else {
        // `get_revision_1` without `limited`: pop, skip what is too old without following its
        // parents, follow the parents of the rest.
        let mut out = Vec::new();
        while !list.is_empty() {
            let oid = list.remove(0);
            if let Some(max_age) = spec.max_age
                && walk.date(&oid) < max_age
            {
                continue;
            }
            walk.process_parents(oid, &mut list)?;
            if let Some(min_age) = spec.min_age
                && walk.date(&oid) > min_age
            {
                continue;
            }
            out.push(oid);
        }
        out
    };
    let parents = commits
        .iter()
        .map(|oid| (*oid, walk.nodes[oid].parents.clone()))
        .collect();
    Ok(Walked {
        commits,
        parents,
        parsed: walk.nodes.keys().copied().collect(),
    })
}

/// The outcome of [`walk`].
pub struct Walked {
    /// The interesting commits, in order.
    pub commits: Vec<ObjectId>,
    /// Every listed commit's full parent list.
    pub parents: HashMap<ObjectId, Vec<ObjectId>>,
    /// Every commit the walk parsed (as opposed to merely named as a parent).
    pub parsed: HashSet<ObjectId>,
}

/// `sort_in_topological_order` on an arbitrary list of commits (what git applies to the boundary
/// commits of `--boundary`): no parent comes before a child that is in the list, ties broken as the
/// chosen order says.
///
/// Git only knows the date and the parents of commits it has *parsed*; a commit that was named as
/// a parent but never walked (a second parent under `--first-parent`) counts as dated 0 with no
/// parents (its author date is still readable), which is what `parsed` lets this reproduce.
pub fn sort_topologically(
    repository: &Repository,
    list: Vec<ObjectId>,
    order: TopoOrder,
    parsed: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>> {
    let mut walk = Walk {
        repository,
        nodes: HashMap::new(),
        flags: HashMap::new(),
        first_parent: false,
    };
    for oid in &list {
        walk.load(*oid)?;
        if !parsed.contains(oid) {
            // Unparsed: no committer date and no parents, but the author date is read from the
            // object itself when git records it for `--author-date-order`.
            let node = walk.nodes.get_mut(oid).expect("just loaded");
            node.parents.clear();
            node.date = 0;
        }
    }
    Ok(walk.topo_sort(list, order))
}
