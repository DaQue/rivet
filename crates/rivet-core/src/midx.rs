//! A read-only reader for git's `multi-pack-index` (format version 1, one file).
//!
//! It maps an object id to a pack and an offset with one binary search instead of one per pack.
//! Like the commit-graph it is advisory: a file that fails the structural checks is ignored, and
//! Rivet never writes it. An incremental chain (`multi-pack-index.d/`) is not read.

use std::path::Path;

use crate::hash::{ObjectFormat, ObjectId};

/// The parsed table of contents of a `multi-pack-index`.
#[derive(Debug)]
pub struct MultiPackIndex {
    format: ObjectFormat,
    data: Vec<u8>,
    pack_names: Vec<String>,
    count: usize,
    fanout: usize,
    oids: usize,
    offsets: usize,
    large_offsets: Option<(usize, usize)>,
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn be64(data: &[u8], at: usize) -> Option<u64> {
    data.get(at..at + 8)
        .map(|b| u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

impl MultiPackIndex {
    /// Open `multi-pack-index` in the pack directory `pack_dir`; `None` when absent or damaged.
    pub fn open(pack_dir: &Path, format: ObjectFormat) -> Option<Self> {
        let data = std::fs::read(pack_dir.join("multi-pack-index")).ok()?;
        let hash_len = format.len();
        let hash_version = match format {
            ObjectFormat::Sha1 => 1,
            ObjectFormat::Sha256 => 2,
        };
        if data.len() < 12 + 12 + hash_len
            || &data[..4] != b"MIDX"
            || data[4] != 1
            || data[5] != hash_version
            || data[7] != 0
        {
            return None;
        }
        let chunks = usize::from(data[6]);
        let pack_count = be32(&data, 8)? as usize;
        let table = 12usize;
        let table_end = table + (chunks + 1) * 12;
        let body_end = data.len().checked_sub(hash_len)?;
        if table_end > body_end {
            return None;
        }
        let mut names = None;
        let mut fanout = None;
        let mut oids = None;
        let mut offsets = None;
        let mut large = None;
        for i in 0..chunks {
            let entry = table + i * 12;
            let id = &data[entry..entry + 4];
            let start = usize::try_from(be64(&data, entry + 4)?).ok()?;
            let end = usize::try_from(be64(&data, entry + 16)?).ok()?;
            if start < table_end || end < start || end > body_end {
                return None;
            }
            match id {
                b"PNAM" => names = Some((start, end)),
                b"OIDF" => fanout = Some((start, end)),
                b"OIDL" => oids = Some((start, end)),
                b"OOFF" => offsets = Some((start, end)),
                b"LOFF" => large = Some((start, end - start)),
                _ => {}
            }
        }
        let (names_start, names_end) = names?;
        let (fanout, fanout_end) = fanout?;
        let (oids, oids_end) = oids?;
        let (offsets, offsets_end) = offsets?;
        if fanout_end - fanout != 1024 {
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
        if oids_end - oids != count * hash_len || offsets_end - offsets != count * 8 {
            return None;
        }
        let pack_names: Vec<String> = data[names_start..names_end]
            .split(|&b| b == 0)
            .take(pack_count)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect();
        if pack_names.len() != pack_count || pack_names.iter().any(String::is_empty) {
            return None;
        }
        Some(Self {
            format,
            data,
            pack_names,
            count,
            fanout,
            oids,
            offsets,
            large_offsets: large,
        })
    }

    /// The `.idx` file names of the packs the index covers.
    pub fn pack_names(&self) -> &[String] {
        &self.pack_names
    }

    /// How many objects the index lists.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether the index lists no objects.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn find(&self, oid: &ObjectId) -> Option<usize> {
        let first = oid.as_bytes()[0] as usize;
        let low = if first == 0 {
            0
        } else {
            be32(&self.data, self.fanout + (first - 1) * 4)? as usize
        };
        let high = be32(&self.data, self.fanout + first * 4)? as usize;
        let len = self.format.len();
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

    /// Whether the index lists `oid`.
    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.find(oid).is_some()
    }

    /// The `.idx` name of the pack holding `oid` and the object's offset in it.
    pub fn lookup(&self, oid: &ObjectId) -> Option<(&str, u64)> {
        let at = self.find(oid)?;
        let pack = be32(&self.data, self.offsets + at * 8)? as usize;
        let raw = be32(&self.data, self.offsets + at * 8 + 4)?;
        let offset = if raw & 0x8000_0000 != 0 {
            let (start, size) = self.large_offsets?;
            let slot = (raw & 0x7fff_ffff) as usize;
            if (slot + 1) * 8 > size {
                return None;
            }
            be64(&self.data, start + slot * 8)?
        } else {
            u64::from(raw)
        };
        Some((self.pack_names.get(pack)?, offset))
    }
}
