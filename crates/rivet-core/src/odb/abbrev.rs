//! Object-name abbreviation: git's `find_unique_abbrev` over the loose and packed stores.
//!
//! The question is "how many leading hex digits does this id need so that no *other* object
//! shares them?". Answering it never needs the whole database: in a sorted pack index the only
//! ids that can share the longest prefix with `oid` are its two neighbours, and among loose
//! objects they all live in the single `objects/xx/` directory of `oid`'s first byte.

use std::fs;

use crate::error::Result;
use crate::hash::ObjectId;
use crate::odb::Odb;

/// git's `FALLBACK_DEFAULT_ABBREV`: the shortest length the automatic rule ever picks.
const DEFAULT_ABBREV: usize = 7;

/// git's hard lower bound for an explicit length (`MINIMUM_ABBREV`).
pub const MINIMUM_ABBREV: usize = 4;

/// Leading hex digits two ids have in common.
fn shared_nibbles(a: &ObjectId, b: &ObjectId) -> usize {
    let mut count = 0;
    for (x, y) in a.as_bytes().iter().zip(b.as_bytes()) {
        if x == y {
            count += 2;
        } else {
            if x >> 4 == y >> 4 {
                count += 1;
            }
            break;
        }
    }
    count
}

impl Odb {
    /// An estimate of how many objects the database holds, the way git makes it: every pack's
    /// exact count plus 256 times the loose objects found in the one bucket `objects/17/`.
    fn approximate_object_count(&self) -> usize {
        let packed: usize = self.packs.iter().map(|pack| pack.len()).sum();
        let sample = self.loose_bucket(0x17).len();
        packed + sample * 256
    }

    /// Loose ids whose first byte is `byte`. A missing or unreadable bucket is an empty one.
    fn loose_bucket(&self, byte: u8) -> Vec<ObjectId> {
        let dir = self.loose.objects_dir().join(format!("{byte:02x}"));
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let rest = self.format.hex_len() - 2;
        let mut ids = Vec::new();
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.len() != rest || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let full = format!("{byte:02x}{name}");
            if let Ok(oid) = ObjectId::from_hex(self.format, full.as_bytes().into()) {
                ids.push(oid);
            }
        }
        ids
    }

    /// The length git picks when nothing asks for one: about as many hex digits as make a
    /// collision unlikely for this many objects, never fewer than seven.
    ///
    /// Computed once per `Odb` — it is an estimate, and git too decides it once per process.
    pub fn default_abbrev_len(&self) -> usize {
        if let Some(len) = self.abbrev_len.get() {
            return len;
        }
        let count = self.approximate_object_count();
        let bits = (usize::BITS - count.leading_zeros()) as usize;
        let len = bits
            .div_ceil(2)
            .max(DEFAULT_ABBREV)
            .min(self.format.hex_len());
        self.abbrev_len.set(Some(len));
        len
    }

    /// Hex digits `oid` needs: at least `minimum` (clamped to `4..=hex_len`) and enough that no
    /// other object in the database starts with the same digits.
    ///
    /// Asks only the sorted neighbours of `oid` in each pack and the one loose bucket it would be
    /// in, so the cost does not grow with the number of objects in the repository.
    pub fn unique_abbrev_len(&self, oid: &ObjectId, minimum: usize) -> usize {
        let hex_len = self.format.hex_len();
        let mut need = minimum.clamp(MINIMUM_ABBREV, hex_len);
        let mut consider = |other: &ObjectId| {
            if other != oid {
                need = need.max(shared_nibbles(oid, other) + 1);
            }
        };
        for pack in &self.packs {
            let index = pack.index().as_index();
            // Lower bound: the first entry >= oid.
            let (mut low, mut high) = (0, index.len());
            while low < high {
                let mid = low + (high - low) / 2;
                match index.entry(mid) {
                    Some(entry) if entry.oid < *oid => low = mid + 1,
                    _ => high = mid,
                }
            }
            if low > 0
                && let Some(entry) = index.entry(low - 1)
            {
                consider(&entry.oid);
            }
            for at in [low, low + 1] {
                if let Some(entry) = index.entry(at) {
                    consider(&entry.oid);
                }
            }
        }
        for other in self.loose_bucket(oid.as_bytes()[0]) {
            consider(&other);
        }
        need.min(hex_len)
    }

    /// `oid` as lowercase hex cut to the shortest unambiguous length of at least `minimum`
    /// digits. Pass [`Odb::default_abbrev_len`] for git's automatic length.
    ///
    /// # Errors
    ///
    /// Never today; the signature is a [`Result`] so a later store that can fail to list does
    /// not change every caller.
    pub fn abbreviate(&self, oid: &ObjectId, minimum: usize) -> Result<String> {
        let hex = oid.to_hex().to_string();
        let len = self.unique_abbrev_len(oid, minimum);
        Ok(hex[..len.min(hex.len())].to_string())
    }
}
