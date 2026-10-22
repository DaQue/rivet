//! Delta search for pack writing: the choice of which object is stored as a delta against which,
//! following the rules of `git pack-objects` (`builtin/pack-objects.c` and `diff-delta.c`).
//!
//! # What is the same as Git
//!
//! * **The candidate order** is Git's `type_size_sort`: tags, blobs, trees, commits; within a type
//!   by name hash, descending; then by size, descending; then by position in the object list.
//! * **The search** is Git's sliding window (`find_deltas`): each object is tried against the
//!   previous `window - 1` objects, most recent first, subject to the depth limit and Git's size
//!   heuristics (`try_delta`), and the best base moves up the window afterwards.
//! * **The delta encoder** is Git's `create_delta`: a Rabin-fingerprint index over 16-byte blocks
//!   of the base, a greedy longest match, copy operations limited to 64 KiB, and early exit when
//!   the delta would be larger than the allowed maximum. Bases are therefore chosen by the same
//!   delta *sizes* Git sees.
//!
//! # Caveats
//!
//! * **The rolling-hash polynomial's tables are generated here**, from the polynomial's value
//!   (`0xab59b4d1`), instead of copied. The tables only decide which blocks share a bucket; a match
//!   is always verified byte by byte, so the deltas come out the same except in pathological
//!   inputs where a bucket overflows `HASH_LIMIT`.
//! * **Single-threaded.** Git splits the sorted list over `pack.threads` workers once it has more
//!   than a couple of windows' worth of objects; this search is the one-thread search.
//! * Existing deltas are reused as Git reuses them: an object already stored as a delta against an
//!   object that is also in the pack keeps that base, and is not searched again.

use std::sync::OnceLock;

use crate::error::Result;
use crate::object::ObjectKind;

const RABIN_SHIFT: u32 = 23;
const RABIN_WINDOW: usize = 16;
const HASH_LIMIT: usize = 64;
/// Objects smaller than this are never deltified.
const MIN_DELTA_SIZE: u64 = 50;
/// `core.bigFileThreshold`: larger objects are stored whole.
const BIG_FILE: u64 = 512 * 1024 * 1024;
/// Polynomial of the fingerprint, as the table's first non-zero entry.
const POLY: u32 = 0xab59_b4d1;

struct Tables {
    t: [u32; 256],
    u: [u32; 256],
}

/// `v * x mod P` for a polynomial of degree below 31.
fn mulx(v: u32) -> u32 {
    let shifted = v << 1;
    if shifted & 0x8000_0000 != 0 {
        shifted ^ POLY
    } else {
        shifted
    }
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let x31 = POLY ^ 0x8000_0000;
        let mut t = [0u32; 256];
        let mut u = [0u32; 256];
        for i in 0..256u32 {
            // T[i] = (i * x^31 mod P) with the bit the truncated shift keeps cancelled.
            let mut acc = 0u32;
            for bit in 0..8 {
                if i & (1 << bit) != 0 {
                    let mut term = x31;
                    for _ in 0..bit {
                        term = mulx(term);
                    }
                    acc ^= term;
                }
            }
            t[i as usize] = acc ^ ((i & 1) << 31);
            // U[i] = i * x^(8 * (RABIN_WINDOW - 1)) mod P.
            let mut value = i;
            for _ in 0..(8 * (RABIN_WINDOW - 1)) {
                value = mulx(value);
            }
            u[i as usize] = value;
        }
        Tables { t, u }
    })
}

#[inline]
fn step(val: u32, byte: u8, t: &[u32; 256]) -> u32 {
    ((val << 8) | byte as u32) ^ t[(val >> RABIN_SHIFT) as usize]
}

/// A searchable index over one base object's bytes.
pub struct DeltaIndex {
    src: Vec<u8>,
    mask: u32,
    /// `(offset just past the window's first byte..., fingerprint)` per bucket, in source order.
    buckets: Vec<Vec<(usize, u32)>>,
}

/// Git's `create_delta_index`. `None` for a base too short to index.
pub fn create_delta_index(src: &[u8]) -> Option<DeltaIndex> {
    if src.len() < RABIN_WINDOW + 1 {
        return None;
    }
    let t = &tables().t;
    let entries = (src.len() - 1) / RABIN_WINDOW;
    let hsize_target = entries / 4;
    let mut bits = 4;
    while bits < 31 && (1usize << bits) < hsize_target {
        bits += 1;
    }
    let hsize = 1usize << bits;
    let mask = (hsize - 1) as u32;
    let mut buckets: Vec<Vec<(usize, u32)>> = vec![Vec::new(); hsize];
    let mut prev_val = !0u32;
    // Scan from the last block to the first; consecutive identical blocks keep the lowest.
    let mut start = entries * RABIN_WINDOW - RABIN_WINDOW;
    let mut collected: Vec<(usize, u32)> = Vec::new();
    loop {
        let mut val = 0u32;
        for i in 1..=RABIN_WINDOW {
            val = step(val, src[start + i], t);
        }
        if val == prev_val {
            if let Some(last) = collected.last_mut() {
                last.0 = start + RABIN_WINDOW;
            }
        } else {
            prev_val = val;
            collected.push((start + RABIN_WINDOW, val));
        }
        if start < RABIN_WINDOW {
            break;
        }
        start -= RABIN_WINDOW;
    }
    // Head insertion in the C code leaves each bucket in ascending source order.
    for (ptr, val) in collected.into_iter().rev() {
        buckets[(val & mask) as usize].push((ptr, val));
    }
    for bucket in &mut buckets {
        if bucket.len() > HASH_LIMIT {
            let count = bucket.len();
            let kept: Vec<(usize, u32)> = (0..HASH_LIMIT)
                .map(|k| bucket[k * count / HASH_LIMIT])
                .collect();
            *bucket = kept;
        }
    }
    Some(DeltaIndex {
        src: src.to_vec(),
        mask,
        buckets,
    })
}

fn push_varint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Git's `create_delta`: the delta turning the indexed base into `trg`, or `None` when it would
/// be larger than `max_size` (0 means unlimited).
pub fn create_delta(index: &DeltaIndex, trg: &[u8], max_size: usize) -> Option<Vec<u8>> {
    if trg.is_empty() {
        return None;
    }
    let tb = tables();
    let (t, u) = (&tb.t, &tb.u);
    let mut out: Vec<u8> = Vec::with_capacity(trg.len() / 2 + 64);
    push_varint(&mut out, index.src.len());
    push_varint(&mut out, trg.len());

    let ref_data = &index.src[..];
    let mut data = 0usize;
    let top = trg.len();

    out.push(0); // slot for the first insert opcode
    let mut val = 0u32;
    let mut inscnt = 0usize;
    while data < RABIN_WINDOW && data < top {
        out.push(trg[data]);
        val = step(val, trg[data], t);
        data += 1;
        inscnt += 1;
    }

    let mut moff = 0usize;
    let mut msize = 0usize;
    while data < top {
        if msize < 4096 {
            val ^= u[trg[data - RABIN_WINDOW] as usize];
            val = step(val, trg[data], t);
            for &(ptr, entry_val) in &index.buckets[(val & index.mask) as usize] {
                if entry_val != val {
                    continue;
                }
                let mut ref_size = ref_data.len() - ptr;
                if ref_size > top - data {
                    ref_size = top - data;
                }
                if ref_size <= msize {
                    break;
                }
                let mut matched = 0;
                while matched < ref_size && trg[data + matched] == ref_data[ptr + matched] {
                    matched += 1;
                }
                if msize < matched {
                    msize = matched;
                    moff = ptr;
                    if msize >= 4096 {
                        break;
                    }
                }
            }
        }

        if msize < 4 {
            if inscnt == 0 {
                out.push(0);
            }
            out.push(trg[data]);
            data += 1;
            inscnt += 1;
            if inscnt == 0x7f {
                let at = out.len() - inscnt - 1;
                out[at] = inscnt as u8;
                inscnt = 0;
            }
            msize = 0;
        } else {
            if inscnt > 0 {
                while moff > 0 && ref_data[moff - 1] == trg[data - 1] {
                    // One byte of the pending insert is covered by the match too.
                    msize += 1;
                    moff -= 1;
                    data -= 1;
                    out.pop();
                    inscnt -= 1;
                    if inscnt > 0 {
                        continue;
                    }
                    out.pop(); // the now empty insert's count slot
                    break;
                }
                if inscnt > 0 {
                    let at = out.len() - inscnt - 1;
                    out[at] = inscnt as u8;
                }
                inscnt = 0;
            }

            let left = msize.saturating_sub(0x10000);
            msize -= left;

            let op_at = out.len();
            out.push(0);
            let mut op = 0x80u8;
            if moff & 0x0000_00ff != 0 {
                out.push(moff as u8);
                op |= 0x01;
            }
            if moff & 0x0000_ff00 != 0 {
                out.push((moff >> 8) as u8);
                op |= 0x02;
            }
            if moff & 0x00ff_0000 != 0 {
                out.push((moff >> 16) as u8);
                op |= 0x04;
            }
            if moff & 0xff00_0000 != 0 {
                out.push((moff >> 24) as u8);
                op |= 0x08;
            }
            if msize & 0x00ff != 0 {
                out.push(msize as u8);
                op |= 0x10;
            }
            if msize & 0xff00 != 0 {
                out.push((msize >> 8) as u8);
                op |= 0x20;
            }
            out[op_at] = op;

            data += msize;
            moff += msize;
            msize = left;

            if msize < 4096 {
                val = 0;
                for &byte in &trg[data - RABIN_WINDOW..data] {
                    val = step(val, byte, t);
                }
            }
        }

        if max_size > 0 && out.len() > max_size {
            return None;
        }
    }
    if inscnt > 0 {
        let at = out.len() - inscnt - 1;
        out[at] = inscnt as u8;
    }
    if max_size > 0 && out.len() > max_size {
        return None;
    }
    Some(out)
}

/// One object offered to the search.
#[derive(Clone, Debug)]
pub struct DeltaInput {
    /// The object's type.
    pub kind: ObjectKind,
    /// Git's `pack_name_hash` of the path the object was first seen under (0 for none).
    pub name_hash: u32,
    /// The object's size.
    pub size: u64,
    /// An existing delta to keep: the position of the base in the input list.
    pub reused_base: Option<usize>,
    /// Size of the reused delta, for the depth accounting.
    pub reused_size: u64,
}

/// The delta chosen for one object.
#[derive(Clone, Debug)]
pub struct ChosenDelta {
    /// Position of the base in the input list.
    pub base: usize,
    /// The delta bytes (`None` for a reused delta, whose bytes the caller already has).
    pub delta: Option<Vec<u8>>,
}

/// Git's `pack_name_hash` (name hash version 1): the last characters count most.
pub fn pack_name_hash(name: &[u8]) -> u32 {
    let mut hash = 0u32;
    for &c in name {
        if c.is_ascii_whitespace() || c == 0x0b {
            continue;
        }
        hash = (hash >> 2).wrapping_add((c as u32) << 24);
    }
    hash
}

fn type_rank(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Commit => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Blob => 3,
        ObjectKind::Tag => 4,
    }
}

struct Unpacked {
    entry: Option<usize>,
    data: Option<Vec<u8>>,
    index: Option<DeltaIndex>,
    depth: usize,
}

impl Unpacked {
    fn empty() -> Self {
        Self {
            entry: None,
            data: None,
            index: None,
            depth: 0,
        }
    }
}

/// Choose deltas for `inputs` (given in Git's object-list order). `load` reads an object's bytes.
///
/// # Errors
///
/// Whatever `load` returns.
pub fn search_deltas(
    inputs: &[DeltaInput],
    load: &mut dyn FnMut(usize) -> Result<Vec<u8>>,
    window: usize,
    max_depth_limit: usize,
    raw_len: usize,
) -> Result<Vec<Option<ChosenDelta>>> {
    let n = inputs.len();
    let mut chosen: Vec<Option<ChosenDelta>> = vec![None; n];
    let mut delta_size: Vec<u64> = vec![0; n];

    // Reused deltas, dropped when their chain would be deeper than allowed.
    let mut reused: Vec<Option<usize>> = inputs.iter().map(|i| i.reused_base).collect();
    let mut depth_of: Vec<Option<usize>> = vec![None; n];
    fn chain_depth(
        i: usize,
        reused: &mut [Option<usize>],
        depth_of: &mut [Option<usize>],
        limit: usize,
        visiting: &mut Vec<usize>,
    ) -> usize {
        if let Some(d) = depth_of[i] {
            return d;
        }
        let Some(base) = reused[i] else {
            depth_of[i] = Some(0);
            return 0;
        };
        if visiting.contains(&i) {
            reused[i] = None;
            depth_of[i] = Some(0);
            return 0;
        }
        visiting.push(i);
        let d = chain_depth(base, reused, depth_of, limit, visiting) + 1;
        visiting.pop();
        if d > limit {
            reused[i] = None;
            depth_of[i] = Some(0);
            return 0;
        }
        depth_of[i] = Some(d);
        d
    }
    for i in 0..n {
        chain_depth(
            i,
            &mut reused,
            &mut depth_of,
            max_depth_limit,
            &mut Vec::new(),
        );
    }
    // Children of each object among the reused deltas, for the depth the chain below already has.
    let mut below: Vec<usize> = vec![0; n];
    for i in 0..n {
        if reused[i].is_some() {
            let mut cur = i;
            let mut up = 0;
            while let Some(base) = reused[cur] {
                up += 1;
                // The deepest chain hanging off `base` through `i`.
                below[base] = below[base].max(up);
                cur = base;
            }
        }
    }
    for i in 0..n {
        if let Some(base) = reused[i] {
            chosen[i] = Some(ChosenDelta { base, delta: None });
            delta_size[i] = inputs[i].reused_size;
        }
    }

    // The list to search, in Git's order.
    let mut list: Vec<usize> = (0..n)
        .filter(|&i| {
            reused[i].is_none() && inputs[i].size >= MIN_DELTA_SIZE && inputs[i].size <= BIG_FILE
        })
        .collect();
    list.sort_by(|&a, &b| {
        let (x, y) = (&inputs[a], &inputs[b]);
        type_rank(y.kind)
            .cmp(&type_rank(x.kind))
            .then(y.name_hash.cmp(&x.name_hash))
            .then(y.size.cmp(&x.size))
            .then(a.cmp(&b))
    });

    let window = window.max(1);
    let mut array: Vec<Unpacked> = (0..window).map(|_| Unpacked::empty()).collect();
    let mut idx = 0usize;
    let mut count = 0usize;
    for &entry in &list {
        array[idx] = Unpacked::empty();
        array[idx].entry = Some(entry);

        let mut max_depth = max_depth_limit as isize;
        if below[entry] > 0 {
            max_depth -= below[entry] as isize;
        }
        let mut best_base: Option<usize> = None;
        if max_depth > 0 {
            let mut j = window;
            while j > 1 {
                j -= 1;
                let mut other = idx + j;
                if other >= window {
                    other -= window;
                }
                if array[other].entry.is_none() {
                    break;
                }
                let ret = try_delta(
                    inputs,
                    &mut array,
                    idx,
                    other,
                    max_depth as usize,
                    raw_len as u64,
                    &mut chosen,
                    &mut delta_size,
                    load,
                )?;
                match ret {
                    Ret::Stop => break,
                    Ret::Better => best_base = Some(other),
                    Ret::No => {}
                }
            }
        }

        // Move the best base up the window, just after the object that now uses it.
        if chosen[entry].is_some()
            && let Some(best) = best_base
        {
            let swap = std::mem::replace(&mut array[best], Unpacked::empty());
            let mut dist = (window + idx - best) % window;
            let mut dst = best;
            while dist > 0 {
                dist -= 1;
                let src = (dst + 1) % window;
                array[dst] = std::mem::replace(&mut array[src], Unpacked::empty());
                dst = src;
            }
            array[dst] = swap;
        }

        idx += 1;
        if count + 1 < window {
            count += 1;
        }
        if idx >= window {
            idx = 0;
        }
    }
    Ok(chosen)
}

enum Ret {
    /// The two objects cannot be deltified against each other; stop trying older ones too.
    Stop,
    No,
    Better,
}

/// Git's `try_delta`: can `window[trg]` be stored as a delta against `window[src]`?
#[allow(clippy::too_many_arguments)]
fn try_delta(
    inputs: &[DeltaInput],
    array: &mut [Unpacked],
    trg: usize,
    src: usize,
    max_depth: usize,
    raw_len: u64,
    chosen: &mut [Option<ChosenDelta>],
    delta_size: &mut [u64],
    load: &mut dyn FnMut(usize) -> Result<Vec<u8>>,
) -> Result<Ret> {
    let trg_entry = array[trg].entry.expect("target is set");
    let src_entry = array[src].entry.expect("source is set");
    if inputs[trg_entry].kind != inputs[src_entry].kind {
        return Ok(Ret::Stop);
    }
    if array[src].depth >= max_depth {
        return Ok(Ret::No);
    }

    let trg_size = inputs[trg_entry].size;
    let src_size = inputs[src_entry].size;
    let (mut max_size, ref_depth) = if chosen[trg_entry].is_none() {
        (trg_size / 2 - raw_len, 1usize)
    } else {
        (delta_size[trg_entry], array[trg].depth)
    };
    max_size =
        max_size * (max_depth - array[src].depth) as u64 / (max_depth - ref_depth + 1) as u64;
    if max_size == 0 {
        return Ok(Ret::No);
    }
    let sizediff = trg_size.saturating_sub(src_size);
    if sizediff >= max_size {
        return Ok(Ret::No);
    }
    if trg_size < src_size / 32 {
        return Ok(Ret::No);
    }

    if array[trg].data.is_none() {
        array[trg].data = Some(load(trg_entry)?);
    }
    if array[src].data.is_none() {
        array[src].data = Some(load(src_entry)?);
    }
    if array[src].index.is_none() {
        let built = create_delta_index(array[src].data.as_deref().unwrap_or(&[]));
        match built {
            Some(index) => array[src].index = Some(index),
            None => return Ok(Ret::No),
        }
    }
    let delta = {
        let index = array[src].index.as_ref().expect("index built");
        let data = array[trg].data.as_deref().unwrap_or(&[]);
        create_delta(index, data, max_size as usize)
    };
    let Some(delta) = delta else {
        return Ok(Ret::No);
    };
    let size = delta.len() as u64;
    if chosen[trg_entry].is_some()
        && size == delta_size[trg_entry]
        && array[src].depth + 1 >= array[trg].depth
    {
        return Ok(Ret::No);
    }
    chosen[trg_entry] = Some(ChosenDelta {
        base: src_entry,
        delta: Some(delta),
    });
    delta_size[trg_entry] = size;
    array[trg].depth = array[src].depth + 1;
    Ok(Ret::Better)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::delta::apply_delta;

    fn pseudo(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                b'a' + ((state >> 33) % 7) as u8
            })
            .collect()
    }

    #[test]
    fn deltas_reconstruct_the_target() {
        for seed in 1..40u64 {
            let base = pseudo(seed, 200 + (seed as usize * 97) % 5000);
            let mut target = base.clone();
            let at = (seed as usize * 31) % target.len();
            target.splice(at..at.min(target.len()), b"inserted text".iter().copied());
            let cut = (seed as usize * 13) % target.len();
            target.drain(cut..(cut + 40).min(target.len()));
            target.extend_from_slice(&pseudo(seed + 100, 30));
            let index = create_delta_index(&base).expect("indexable");
            let delta = create_delta(&index, &target, 0).expect("unlimited");
            assert_eq!(
                apply_delta(&base, &delta).expect("applies"),
                target,
                "seed {seed}"
            );
            assert!(
                delta.len() < target.len(),
                "seed {seed}: delta should be smaller"
            );
        }
    }

    #[test]
    fn a_delta_over_the_limit_is_refused() {
        let base = pseudo(1, 3000);
        let target = pseudo(2, 3000);
        let index = create_delta_index(&base).expect("indexable");
        assert!(create_delta(&index, &target, 100).is_none());
    }

    #[test]
    fn identical_content_is_one_copy() {
        let base = pseudo(3, 4000);
        let index = create_delta_index(&base).expect("indexable");
        let delta = create_delta(&index, &base, 0).expect("delta");
        assert!(delta.len() < 24, "got {} bytes", delta.len());
        assert_eq!(apply_delta(&base, &delta).expect("applies"), base);
    }

    #[test]
    fn name_hash_matches_git() {
        assert_eq!(pack_name_hash(b"big.txt"), 0x9a7f_6000);
        assert_eq!(pack_name_hash(b"copy.txt"), 0x9a95_7c00);
        assert_eq!(pack_name_hash(b""), 0);
    }
}
