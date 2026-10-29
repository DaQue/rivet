//! A read-only reader for git's `multi-pack-index` (format version 1): the single file, or an
//! incremental chain under `multi-pack-index.d/`.
//!
//! It maps an object id to a pack and an offset with one binary search instead of one per pack.
//! Like the commit-graph it is advisory: a file that fails the structural checks or whose trailer
//! is not the hash of its body is ignored, and Rivet never writes it.

use std::path::Path;

use crate::hash::{ObjectFormat, ObjectId};

/// The parsed table of contents of a `multi-pack-index`.
#[derive(Debug)]
pub struct MultiPackIndex {
    format: ObjectFormat,
    /// The layer below this one in an incremental chain.
    base: Option<Box<MultiPackIndex>>,
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
    /// Open the index of the pack directory `pack_dir`: `multi-pack-index`, or else the chain in
    /// `multi-pack-index.d/multi-pack-index-chain`; `None` when absent or damaged.
    pub fn open(pack_dir: &Path, format: ObjectFormat) -> Option<Self> {
        if let Ok(data) = std::fs::read(pack_dir.join("multi-pack-index")) {
            return Self::parse(data, format, None);
        }
        let dir = pack_dir.join("multi-pack-index.d");
        let chain = std::fs::read_to_string(dir.join("multi-pack-index-chain")).ok()?;
        let hex_len = format.hex_len();
        let mut top: Option<Self> = None;
        for line in chain.lines() {
            let line = line.trim();
            if line.len() != hex_len || !line.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let path = dir.join(format!(
                "multi-pack-index-{}.midx",
                line.to_ascii_lowercase()
            ));
            let data = std::fs::read(path).ok()?;
            top = Some(Self::parse(data, format, top.map(Box::new))?);
        }
        top
    }

    /// Whether `data` ends with the hash of what comes before it.
    fn checksum_ok(data: &[u8], format: ObjectFormat) -> bool {
        let Some(body) = data.len().checked_sub(format.len()) else {
            return false;
        };
        let mut hasher = format.hasher();
        hasher.update(&data[..body]);
        hasher
            .finish()
            .is_ok_and(|sum| sum.as_bytes() == &data[body..])
    }

    /// One layer; `base` is the layer parsed just before. (The header's count of base files is
    /// not consulted: git does not either.)
    fn parse(data: Vec<u8>, format: ObjectFormat, base: Option<Box<Self>>) -> Option<Self> {
        if !Self::checksum_ok(&data, format) {
            return None;
        }
        let hash_len = format.len();
        let hash_version = match format {
            ObjectFormat::Sha1 => 1,
            ObjectFormat::Sha256 => 2,
        };
        if data.len() < 12 + 12 + hash_len
            || &data[..4] != b"MIDX"
            || data[4] != 1
            || data[5] != hash_version
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
            base,
            data,
            pack_names,
            count,
            fanout,
            oids,
            offsets,
            large_offsets: large,
        })
    }

    /// The `.idx` file names of the packs this layer of the index names.
    pub fn pack_names(&self) -> &[String] {
        &self.pack_names
    }

    /// The `.idx` file names of every pack the index covers, lower layers first.
    pub fn all_pack_names(&self) -> Vec<&str> {
        let mut names = self
            .base
            .as_ref()
            .map_or_else(Vec::new, |b| b.all_pack_names());
        names.extend(self.pack_names.iter().map(String::as_str));
        names
    }

    /// How many objects the index lists.
    pub fn len(&self) -> usize {
        self.count + self.base.as_ref().map_or(0, |b| b.len())
    }

    /// Whether the index lists no objects.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
        self.find(oid).is_some() || self.base.as_ref().is_some_and(|b| b.contains(oid))
    }

    /// The `.idx` name of the pack holding `oid` and the object's offset in it.
    pub fn lookup(&self, oid: &ObjectId) -> Option<(&str, u64)> {
        let Some(at) = self.find(oid) else {
            return self.base.as_ref()?.lookup(oid);
        };
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
        // The pack id counts in this layer's own pack names.
        Some((self.pack_names.get(pack)?, offset))
    }
}

/// What `fsck` reports about `pack/multi-pack-index`.
#[derive(Debug)]
pub struct MidxVerification {
    /// The lines git writes to standard error.
    pub lines: Vec<String>,
    /// The exit status contribution: 1 for a damaged file, 128 when git dies.
    pub status: i32,
}

/// `fsck`'s look at the single `multi-pack-index` file (`display` is the path as git words it);
/// `None` when there is none. Content checks beyond the structure and the trailer are not made.
pub fn verify(pack_dir: &Path, format: ObjectFormat, display: &str) -> Option<MidxVerification> {
    let data = std::fs::read(pack_dir.join("multi-pack-index")).ok()?;
    let hash_len = format.len();
    // Git loads the file three times before it reports that it could not parse it, then once more
    // for the verification itself.
    let rejected = |message: String| {
        Some(MidxVerification {
            lines: vec![
                message.clone(),
                message.clone(),
                message.clone(),
                "error: multi-pack-index file exists, but failed to parse".to_string(),
                message,
            ],
            status: 1,
        })
    };
    let fatal = |message: String| {
        Some(MidxVerification {
            lines: vec![format!("fatal: {message}")],
            status: 128,
        })
    };
    if data.len() < 12 + hash_len {
        return rejected(format!(
            "error: multi-pack-index file {display} is too small"
        ));
    }
    let signature = be32(&data, 0).unwrap_or(0);
    if signature != 0x4d49_4458 {
        return fatal(format!(
            "multi-pack-index signature 0x{signature:08x} does not match signature 0x{:08x}",
            0x4d49_4458u32
        ));
    }
    if data[4] != 1 {
        return fatal(format!(
            "multi-pack-index version {} not recognized",
            data[4]
        ));
    }
    let hash_version = match format {
        ObjectFormat::Sha1 => 1u8,
        ObjectFormat::Sha256 => 2,
    };
    if data[5] != hash_version {
        return rejected(format!(
            "error: multi-pack-index hash version {} does not match version {}",
            data[5], hash_version
        ));
    }
    let chunks = usize::from(data[6]);
    let limit = (data.len() - hash_len) as u64;
    let mut found: Vec<([u8; 4], u64, u64)> = Vec::new();
    for i in 0..chunks {
        let entry = 12 + i * 12;
        let (Some(id), Some(offset), Some(next)) = (
            be32(&data, entry),
            be64(&data, entry + 4),
            be64(&data, entry + 16),
        ) else {
            return rejected(format!(
                "error: multi-pack-index file {display} is too small"
            ));
        };
        if id == 0 {
            return rejected(
                "error: terminating chunk id appears earlier than expected".to_string(),
            );
        }
        if next < offset || next > limit {
            return rejected(format!(
                "error: improper chunk offset(s) {offset:x} and {next:x}"
            ));
        }
        found.push((id.to_be_bytes(), offset, next));
    }
    let chunk = |name: &[u8; 4]| {
        found
            .iter()
            .find(|(id, _, _)| id == name)
            .map(|(_, start, end)| (*start as usize, *end as usize))
    };
    if chunk(b"PNAM").is_none() {
        return fatal("multi-pack-index required pack-name chunk missing or corrupted".to_string());
    }
    let Some((fanout, fanout_end)) = chunk(b"OIDF").filter(|(s, e)| e - s == 1024) else {
        return fatal(
            "multi-pack-index required OID fanout chunk missing or corrupted".to_string(),
        );
    };
    let _ = fanout_end;
    let mut count = 0usize;
    for i in 0..256usize {
        let value = be32(&data, fanout + i * 4).unwrap_or(0);
        if i > 0 && (value as usize) < count {
            return Some(MidxVerification {
                lines: vec![
                    format!(
                        "error: oid fanout out of order: fanout[{}] = {} > {} = fanout[{}]",
                        i - 1,
                        count,
                        value,
                        i
                    ),
                    "fatal: multi-pack-index required OID fanout chunk missing or corrupted"
                        .to_string(),
                ],
                status: 128,
            });
        }
        count = value as usize;
    }
    if !chunk(b"OIDL").is_some_and(|(s, e)| e - s == count * hash_len) {
        return fatal(
            "multi-pack-index required OID lookup chunk missing or corrupted".to_string(),
        );
    }
    if !chunk(b"OOFF").is_some_and(|(s, e)| e - s == count * 8) {
        return fatal(
            "multi-pack-index required object offsets chunk missing or corrupted".to_string(),
        );
    }
    if !MultiPackIndex::checksum_ok(&data, format) {
        return Some(MidxVerification {
            lines: vec!["incorrect checksum".to_string()],
            status: 1,
        });
    }
    Some(MidxVerification {
        lines: Vec::new(),
        status: 0,
    })
}
