//! A read-only reader for git's `commit-graph` files (format version 1, single file or split chain).
//!
//! The graph answers "does this commit exist, what are its tree, parents, commit time and
//! generation number" without inflating the commit object. It is an optimisation, never a source
//! of truth: any file that does not pass the structural checks below is ignored as a whole, and
//! the caller falls back to the object database. Rivet never writes these files.

use std::path::Path;

use crate::hash::{ObjectFormat, ObjectId};

const PARENT_NONE: u32 = 0x7000_0000;
const EXTRA_EDGES: u32 = 0x8000_0000;
const LAST_EDGE: u32 = 0x8000_0000;

/// What the graph knows about one commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphCommit {
    /// The commit's root tree.
    pub tree: ObjectId,
    /// Its parents, in order.
    pub parents: Vec<ObjectId>,
    /// The committer time, in seconds since the epoch.
    pub commit_time: i64,
    /// The topological level: 1 for a root, else one more than the largest parent level.
    pub generation: u32,
}

/// One graph file: the offsets of the chunks inside `data`.
#[derive(Debug)]
struct Layer {
    data: Vec<u8>,
    count: usize,
    fanout: usize,
    oids: usize,
    commits: usize,
    edges: Option<(usize, usize)>,
    /// Number of commits in the layers below this one.
    base: usize,
}

/// A commit-graph, possibly split into a chain of layers (base first).
#[derive(Debug)]
pub struct CommitGraph {
    format: ObjectFormat,
    layers: Vec<Layer>,
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn be64(data: &[u8], at: usize) -> Option<u64> {
    data.get(at..at + 8)
        .map(|b| u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

impl Layer {
    /// Parse one file, or `None` if anything about its structure is off.
    fn parse(
        data: Vec<u8>,
        format: ObjectFormat,
        base: usize,
        expect_bases: usize,
    ) -> Option<Self> {
        let hash_len = format.len();
        if data.len() < 8 + 12 + hash_len || &data[..4] != b"CGPH" || data[4] != 1 {
            return None;
        }
        let hash_version = match format {
            ObjectFormat::Sha1 => 1,
            ObjectFormat::Sha256 => 2,
        };
        if data[5] != hash_version || usize::from(data[7]) != expect_bases {
            return None;
        }
        let chunks = usize::from(data[6]);
        let table = 8usize;
        let table_end = table + (chunks + 1) * 12;
        let body_end = data.len().checked_sub(hash_len)?;
        if table_end > body_end {
            return None;
        }
        let mut found: [Option<(usize, usize)>; 4] = [None; 4];
        let mut edges = None;
        let mut previous = None;
        for i in 0..chunks {
            let entry = table + i * 12;
            let id = &data[entry..entry + 4];
            let start = usize::try_from(be64(&data, entry + 4)?).ok()?;
            let end = usize::try_from(be64(&data, entry + 16)?).ok()?;
            if start < table_end || end < start || end > body_end {
                return None;
            }
            if previous.is_some_and(|p| start < p) {
                return None;
            }
            previous = Some(end);
            match id {
                b"OIDF" => found[0] = Some((start, end)),
                b"OIDL" => found[1] = Some((start, end)),
                b"CDAT" => found[2] = Some((start, end)),
                b"EDGE" => edges = Some((start, end)),
                _ => {}
            }
        }
        // The terminating table entry has id 0 and closes the last chunk.
        if data[table + chunks * 12..table + chunks * 12 + 4] != [0; 4] {
            return None;
        }
        let (fanout, fanout_end) = found[0]?;
        let (oids, oids_end) = found[1]?;
        let (commits, commits_end) = found[2]?;
        if fanout_end - fanout != 256 * 4 {
            return None;
        }
        let mut last = 0u32;
        for i in 0..256 {
            let value = be32(&data, fanout + i * 4)?;
            if value < last {
                return None;
            }
            last = value;
        }
        let count = last as usize;
        if oids_end - oids != count * hash_len || commits_end - commits != count * (hash_len + 16) {
            return None;
        }
        Some(Self {
            data,
            count,
            fanout,
            oids,
            commits,
            edges: edges.map(|(start, end)| (start, end - start)),
            base,
        })
    }

    fn oid_at(&self, format: ObjectFormat, at: usize) -> ObjectId {
        let len = format.len();
        let start = self.oids + at * len;
        ObjectId::from_bytes(format, &self.data[start..start + len]).expect("checked width")
    }

    /// The position of `oid` in this layer.
    fn find(&self, format: ObjectFormat, oid: &ObjectId) -> Option<usize> {
        let first = oid.as_bytes()[0] as usize;
        let low = if first == 0 {
            0
        } else {
            be32(&self.data, self.fanout + (first - 1) * 4)? as usize
        };
        let high = be32(&self.data, self.fanout + first * 4)? as usize;
        let len = format.len();
        let (mut lo, mut hi) = (low, high.min(self.count));
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let start = self.oids + mid * len;
            match self.data[start..start + len].cmp(oid.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }
}

impl CommitGraph {
    /// Open the graph of the object directory `objects`: `info/commit-graph`, or else the split
    /// chain in `info/commit-graphs/commit-graph-chain`. `None` when there is none or when it
    /// does not hold together.
    pub fn open(objects: &Path, format: ObjectFormat) -> Option<Self> {
        let info = objects.join("info");
        if let Ok(data) = std::fs::read(info.join("commit-graph")) {
            // A single file stands alone; a damaged one is not replaced by a chain.
            let layer = Layer::parse(data, format, 0, 0)?;
            return Some(Self {
                format,
                layers: vec![layer],
            });
        }
        let chain =
            std::fs::read_to_string(info.join("commit-graphs").join("commit-graph-chain")).ok()?;
        let hex_len = format.hex_len();
        let mut layers: Vec<Layer> = Vec::new();
        let mut base = 0usize;
        for line in chain.lines() {
            let line = line.trim();
            if line.len() != hex_len || !line.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let path = info
                .join("commit-graphs")
                .join(format!("graph-{}.graph", line.to_ascii_lowercase()));
            let layer = Layer::parse(std::fs::read(path).ok()?, format, base, layers.len())?;
            base += layer.count;
            layers.push(layer);
        }
        if layers.is_empty() {
            return None;
        }
        Some(Self { format, layers })
    }

    /// How many commits the graph lists.
    pub fn len(&self) -> usize {
        self.layers.iter().map(|l| l.count).sum()
    }

    /// Whether the graph lists no commits.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn locate(&self, oid: &ObjectId) -> Option<(usize, usize)> {
        self.layers
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, layer)| layer.find(self.format, oid).map(|at| (i, at)))
    }

    /// Whether the graph lists `oid` as a commit.
    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.locate(oid).is_some()
    }

    fn oid_at_global(&self, position: usize) -> Option<ObjectId> {
        let layer = self.layers.iter().rev().find(|l| l.base <= position)?;
        let at = position - layer.base;
        (at < layer.count).then(|| layer.oid_at(self.format, at))
    }

    /// What the graph records about `oid`, or `None` when it is not listed (or its parent
    /// pointers do not resolve, which means the file is damaged and must not be trusted).
    pub fn commit(&self, oid: &ObjectId) -> Option<GraphCommit> {
        let (layer_index, at) = self.locate(oid)?;
        let layer = &self.layers[layer_index];
        let hash_len = self.format.len();
        let row = layer.commits + at * (hash_len + 16);
        let tree = ObjectId::from_bytes(self.format, &layer.data[row..row + hash_len]).ok()?;
        let first = be32(&layer.data, row + hash_len)?;
        let second = be32(&layer.data, row + hash_len + 4)?;
        let packed = be32(&layer.data, row + hash_len + 8)?;
        let low = be32(&layer.data, row + hash_len + 12)?;
        let commit_time = (i64::from(packed & 3) << 32) | i64::from(low);
        let generation = packed >> 2;

        let mut parents = Vec::new();
        if first != PARENT_NONE {
            parents.push(self.oid_at_global(first as usize)?);
        }
        if second != PARENT_NONE {
            if second & EXTRA_EDGES == 0 {
                parents.push(self.oid_at_global(second as usize)?);
            } else {
                let (start, size) = layer.edges?;
                let mut i = (second & !EXTRA_EDGES) as usize;
                loop {
                    let word = be32(&layer.data, start + i.checked_mul(4)?)?;
                    if (i + 1) * 4 > size {
                        return None;
                    }
                    parents.push(self.oid_at_global((word & !LAST_EDGE) as usize)?);
                    if word & LAST_EDGE != 0 {
                        break;
                    }
                    i += 1;
                }
            }
        }
        Some(GraphCommit {
            tree,
            parents,
            commit_time,
            generation,
        })
    }

    /// The generation number of `oid`, if the graph lists it.
    pub fn generation(&self, oid: &ObjectId) -> Option<u32> {
        let (layer_index, at) = self.locate(oid)?;
        let layer = &self.layers[layer_index];
        let hash_len = self.format.len();
        let row = layer.commits + at * (hash_len + 16);
        Some(be32(&layer.data, row + hash_len + 8)? >> 2)
    }
}
