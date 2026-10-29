//! The split index: a `link` extension naming `sharedindex.<hash>` plus two EWAH bitmaps.
//!
//! The main `index` file lists only what differs from the shared file: entries whose names are
//! empty come first and stand for the shared entries named by the *replace* bitmap, in bit order;
//! the *delete* bitmap names shared entries that are gone; any further entries are additions.

use crate::error::{Error, Result};
use crate::hash::{ObjectFormat, ObjectId};

fn corrupt(reason: &'static str) -> Error {
    Error::CorruptIndex { offset: 0, reason }
}

/// Decode one serialised EWAH bitmap into its set bit positions; returns the bytes consumed.
fn read_ewah(data: &[u8]) -> Result<(Vec<usize>, usize)> {
    let be32 = |at: usize| -> Result<usize> {
        data.get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
            .ok_or_else(|| corrupt("truncated split index bitmap"))
    };
    let _bit_size = be32(0)?;
    let words = be32(4)?;
    let end = 8usize
        .checked_add(
            words
                .checked_mul(8)
                .ok_or_else(|| corrupt("bad bitmap size"))?,
        )
        .ok_or_else(|| corrupt("bad bitmap size"))?;
    if data.len() < end + 4 {
        return Err(corrupt("truncated split index bitmap"));
    }
    let word = |i: usize| -> u64 {
        let b = &data[8 + i * 8..16 + i * 8];
        u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    };
    let mut bits = Vec::new();
    let mut offset = 0usize;
    let mut at = 0usize;
    while at < words {
        let rlw = word(at);
        let running = rlw & 1 == 1;
        let run_len = ((rlw >> 1) & 0xffff_ffff) as usize;
        let literals = (rlw >> 33) as usize;
        if at + 1 + literals > words {
            return Err(corrupt("split index bitmap runs past its end"));
        }
        if running {
            bits.extend(offset..offset + run_len * 64);
        }
        offset += run_len * 64;
        for i in 0..literals {
            let w = word(at + 1 + i);
            for b in 0..64 {
                if w >> b & 1 == 1 {
                    bits.push(offset + b);
                }
            }
            offset += 64;
        }
        at += 1 + literals;
    }
    Ok((bits, end + 4))
}

/// Encode sorted bit positions as one run-length word followed by literal words.
fn write_ewah(bits: &[usize], out: &mut Vec<u8>) {
    let size = bits.last().map_or(0, |last| last + 1);
    let literals = size.div_ceil(64);
    let mut words = vec![0u64; literals];
    for &bit in bits {
        words[bit / 64] |= 1 << (bit % 64);
    }
    out.extend_from_slice(&(size as u32).to_be_bytes());
    out.extend_from_slice(&((1 + literals) as u32).to_be_bytes());
    out.extend_from_slice(&((literals as u64) << 33).to_be_bytes());
    for word in words {
        out.extend_from_slice(&word.to_be_bytes());
    }
    out.extend_from_slice(&0u32.to_be_bytes());
}

/// The parts of a `link` extension: the shared index's id, shared positions deleted and shared
/// positions replaced.
pub(super) struct Link {
    pub shared: ObjectId,
    pub deleted: Vec<usize>,
    pub replaced: Vec<usize>,
}

pub(super) fn parse_link(format: ObjectFormat, data: &[u8]) -> Result<Link> {
    let id_len = format.len();
    if data.len() < id_len {
        return Err(corrupt("truncated split index link"));
    }
    let shared = ObjectId::from_bytes(format, &data[..id_len])?;
    let mut rest = &data[id_len..];
    let mut deleted = Vec::new();
    let mut replaced = Vec::new();
    if !rest.is_empty() {
        let (bits, used) = read_ewah(rest)?;
        deleted = bits;
        rest = &rest[used..];
        let (bits, _) = read_ewah(rest)?;
        replaced = bits;
    }
    Ok(Link {
        shared,
        deleted,
        replaced,
    })
}

pub(super) fn link_bytes(shared: &ObjectId, deleted: &[usize], replaced: &[usize]) -> Vec<u8> {
    let mut out = shared.as_bytes().to_vec();
    write_ewah(deleted, &mut out);
    write_ewah(replaced, &mut out);
    out
}

/// The file name of a shared index.
pub(super) fn shared_name(shared: &ObjectId) -> String {
    format!("sharedindex.{}", shared.to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_link_is_the_bytes_git_writes() {
        let shared = ObjectId::from_bytes(ObjectFormat::Sha1, &[7; 20]).unwrap();
        let bytes = link_bytes(&shared, &[], &[]);
        // 20-byte id, then two empty bitmaps: bit size 0, one word (0), run-length word at 0.
        let mut expected = vec![7u8; 20];
        for _ in 0..2 {
            expected
                .extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        }
        assert_eq!(bytes, expected);
    }

    #[test]
    fn bitmaps_round_trip_and_read_runs() {
        let shared = ObjectId::from_bytes(ObjectFormat::Sha1, &[1; 20]).unwrap();
        let bytes = link_bytes(&shared, &[0, 5, 64, 130], &[3]);
        let link = parse_link(ObjectFormat::Sha1, &bytes).unwrap();
        assert_eq!(link.deleted, vec![0, 5, 64, 130]);
        assert_eq!(link.replaced, vec![3]);
        // A run of two all-ones words followed by one literal word with bit 1 set.
        let mut run = vec![1u8; 20];
        run.extend_from_slice(&[0, 0, 0, 192, 0, 0, 0, 2]);
        run.extend_from_slice(&((2u64 << 1) | 1 | (1u64 << 33)).to_be_bytes());
        run.extend_from_slice(&2u64.to_be_bytes());
        run.extend_from_slice(&[0, 0, 0, 0]);
        run.extend_from_slice(&link_bytes(&shared, &[], &[])[20..40]);
        let link = parse_link(ObjectFormat::Sha1, &run).unwrap();
        assert_eq!(link.deleted.len(), 129);
        assert_eq!(link.deleted[128], 129);
    }

    #[test]
    fn a_truncated_bitmap_is_corrupt_not_a_panic() {
        let shared = ObjectId::from_bytes(ObjectFormat::Sha1, &[1; 20]).unwrap();
        let bytes = link_bytes(&shared, &[1], &[2]);
        assert!(parse_link(ObjectFormat::Sha1, &bytes[..bytes.len() - 6]).is_err());
    }
}
