//! Git's default diff algorithm, ported from `xdiffi.c`: a divide-and-conquer Myers search
//! (`xdl_split` / `xdl_recs_cmp`) preceded by Git's preparation of the two files — trimming the
//! common ends and discarding lines that cannot take part in a match.
//!
//! A textbook Myers finds *a* shortest edit script; Git finds a particular one, and when several
//! scripts are equally short the choice shows in the output. Matching the choice needs the same
//! search order, the same heuristics (`snake_cnt`, the "good diagonal" sampling, `mxcost`) and the
//! same preprocessing, so this follows the C closely.

use std::collections::HashMap;
use std::hash::Hash;

const MAX_COST_MIN: isize = 256;
const HEUR_MIN_COST: isize = 256;
const SNAKE_CNT: isize = 20;
const K_HEUR: isize = 4;
const MAX_EQLIMIT: usize = 1024;
const SIMSCAN_WINDOW: isize = 100;
const KPDIS_RUN: isize = 4;
const LINE_MAX: isize = isize::MAX / 4;

fn bogosqrt(mut n: usize) -> usize {
    let mut i = 1usize;
    while n > 0 {
        i <<= 1;
        n >>= 2;
    }
    i
}

/// `xdl_clean_mmatch`: whether a line with many matches sits inside a run of discardable lines.
fn clean_mmatch(dis: &[u8], i: isize, mut s: isize, mut e: isize) -> bool {
    if i - s > SIMSCAN_WINDOW {
        s = i - SIMSCAN_WINDOW;
    }
    if e - i > SIMSCAN_WINDOW {
        e = i + SIMSCAN_WINDOW;
    }
    let (mut rdis0, mut rpdis0) = (0isize, 1isize);
    let mut r = 1;
    while i - r >= s {
        match dis[(i - r) as usize] {
            0 => rdis0 += 1,
            2 => rpdis0 += 1,
            _ => break,
        }
        r += 1;
    }
    if rdis0 == 0 {
        return false;
    }
    let (mut rdis1, mut rpdis1) = (0isize, 1isize);
    r = 1;
    while i + r <= e {
        match dis[(i + r) as usize] {
            0 => rdis1 += 1,
            2 => rpdis1 += 1,
            _ => break,
        }
        r += 1;
    }
    if rdis1 == 0 {
        return false;
    }
    rdis1 += rdis0;
    rpdis1 += rpdis0;
    rpdis1 * KPDIS_RUN < (rpdis1 + rdis1)
}

struct Split {
    i1: isize,
    i2: isize,
    min_lo: bool,
    min_hi: bool,
}

struct Env {
    mxcost: isize,
}

/// `xdl_split`: find the point where the forward and backward searches meet.
#[allow(clippy::too_many_arguments)]
fn split(
    ha1: &[u64],
    off1: isize,
    lim1: isize,
    ha2: &[u64],
    off2: isize,
    lim2: isize,
    kvdf: &mut [isize],
    kvdb: &mut [isize],
    kbase: isize,
    need_min: bool,
    env: &Env,
) -> Split {
    let k = |d: isize| (d + kbase) as usize;
    let dmin = off1 - lim2;
    let dmax = lim1 - off2;
    let fmid = off1 - off2;
    let bmid = lim1 - lim2;
    let odd = (fmid - bmid) & 1 != 0;
    let (mut fmin, mut fmax) = (fmid, fmid);
    let (mut bmin, mut bmax) = (bmid, bmid);

    kvdf[k(fmid)] = off1;
    kvdb[k(bmid)] = lim1;

    let mut ec = 1isize;
    loop {
        let mut got_snake = false;

        if fmin > dmin {
            fmin -= 1;
            kvdf[k(fmin - 1)] = -1;
        } else {
            fmin += 1;
        }
        if fmax < dmax {
            fmax += 1;
            kvdf[k(fmax + 1)] = -1;
        } else {
            fmax -= 1;
        }

        let mut d = fmax;
        while d >= fmin {
            let mut i1 = if kvdf[k(d - 1)] >= kvdf[k(d + 1)] {
                kvdf[k(d - 1)] + 1
            } else {
                kvdf[k(d + 1)]
            };
            let prev1 = i1;
            let mut i2 = i1 - d;
            while i1 < lim1 && i2 < lim2 && ha1[i1 as usize] == ha2[i2 as usize] {
                i1 += 1;
                i2 += 1;
            }
            if i1 - prev1 > SNAKE_CNT {
                got_snake = true;
            }
            kvdf[k(d)] = i1;
            if odd && bmin <= d && d <= bmax && kvdb[k(d)] <= i1 {
                return Split {
                    i1,
                    i2,
                    min_lo: true,
                    min_hi: true,
                };
            }
            d -= 2;
        }

        if bmin > dmin {
            bmin -= 1;
            kvdb[k(bmin - 1)] = LINE_MAX;
        } else {
            bmin += 1;
        }
        if bmax < dmax {
            bmax += 1;
            kvdb[k(bmax + 1)] = LINE_MAX;
        } else {
            bmax -= 1;
        }

        d = bmax;
        while d >= bmin {
            let mut i1 = if kvdb[k(d - 1)] < kvdb[k(d + 1)] {
                kvdb[k(d - 1)]
            } else {
                kvdb[k(d + 1)] - 1
            };
            let prev1 = i1;
            let mut i2 = i1 - d;
            while i1 > off1 && i2 > off2 && ha1[(i1 - 1) as usize] == ha2[(i2 - 1) as usize] {
                i1 -= 1;
                i2 -= 1;
            }
            if prev1 - i1 > SNAKE_CNT {
                got_snake = true;
            }
            kvdb[k(d)] = i1;
            if !odd && fmin <= d && d <= fmax && i1 <= kvdf[k(d)] {
                return Split {
                    i1,
                    i2,
                    min_lo: true,
                    min_hi: true,
                };
            }
            d -= 2;
        }

        if need_min {
            ec += 1;
            continue;
        }

        if got_snake && ec > HEUR_MIN_COST {
            let mut best = 0isize;
            let mut spl = Split {
                i1: 0,
                i2: 0,
                min_lo: false,
                min_hi: false,
            };
            d = fmax;
            while d >= fmin {
                let dd = if d > fmid { d - fmid } else { fmid - d };
                let i1 = kvdf[k(d)];
                let i2 = i1 - d;
                let v = (i1 - off1) + (i2 - off2) - dd;
                if v > K_HEUR * ec
                    && v > best
                    && off1 + SNAKE_CNT <= i1
                    && i1 < lim1
                    && off2 + SNAKE_CNT <= i2
                    && i2 < lim2
                {
                    let mut kk = 1;
                    while ha1[(i1 - kk) as usize] == ha2[(i2 - kk) as usize] {
                        if kk == SNAKE_CNT {
                            best = v;
                            spl.i1 = i1;
                            spl.i2 = i2;
                            break;
                        }
                        kk += 1;
                    }
                }
                d -= 2;
            }
            if best > 0 {
                spl.min_lo = true;
                spl.min_hi = false;
                return spl;
            }

            best = 0;
            d = bmax;
            while d >= bmin {
                let dd = if d > bmid { d - bmid } else { bmid - d };
                let i1 = kvdb[k(d)];
                let i2 = i1 - d;
                let v = (lim1 - i1) + (lim2 - i2) - dd;
                if v > K_HEUR * ec
                    && v > best
                    && off1 < i1
                    && i1 <= lim1 - SNAKE_CNT
                    && off2 < i2
                    && i2 <= lim2 - SNAKE_CNT
                {
                    let mut kk = 0;
                    while ha1[(i1 + kk) as usize] == ha2[(i2 + kk) as usize] {
                        if kk == SNAKE_CNT - 1 {
                            best = v;
                            spl.i1 = i1;
                            spl.i2 = i2;
                            break;
                        }
                        kk += 1;
                    }
                }
                d -= 2;
            }
            if best > 0 {
                spl.min_lo = false;
                spl.min_hi = true;
                return spl;
            }
        }

        if ec >= env.mxcost {
            let (mut fbest, mut fbest1) = (-1isize, -1isize);
            d = fmax;
            while d >= fmin {
                let mut i1 = kvdf[k(d)].min(lim1);
                let mut i2 = i1 - d;
                if lim2 < i2 {
                    i1 = lim2 + d;
                    i2 = lim2;
                }
                if fbest < i1 + i2 {
                    fbest = i1 + i2;
                    fbest1 = i1;
                }
                d -= 2;
            }
            let (mut bbest, mut bbest1) = (LINE_MAX, LINE_MAX);
            d = bmax;
            while d >= bmin {
                let mut i1 = off1.max(kvdb[k(d)]);
                let mut i2 = i1 - d;
                if i2 < off2 {
                    i1 = off2 + d;
                    i2 = off2;
                }
                if i1 + i2 < bbest {
                    bbest = i1 + i2;
                    bbest1 = i1;
                }
                d -= 2;
            }
            return if (lim1 + lim2) - bbest < fbest - (off1 + off2) {
                Split {
                    i1: fbest1,
                    i2: fbest - fbest1,
                    min_lo: true,
                    min_hi: false,
                }
            } else {
                Split {
                    i1: bbest1,
                    i2: bbest - bbest1,
                    min_lo: false,
                    min_hi: true,
                }
            };
        }
        ec += 1;
    }
}

struct Side<'a> {
    /// Class ids of the lines that survived preparation.
    ha: Vec<u64>,
    /// Original line index of each surviving line.
    rindex: Vec<usize>,
    changed: &'a mut [bool],
}

/// `xdl_recs_cmp`.
#[allow(clippy::too_many_arguments)]
fn recs_cmp(
    d1: &mut Side<'_>,
    mut off1: isize,
    mut lim1: isize,
    d2: &mut Side<'_>,
    mut off2: isize,
    mut lim2: isize,
    kvdf: &mut [isize],
    kvdb: &mut [isize],
    kbase: isize,
    need_min: bool,
    env: &Env,
) {
    while off1 < lim1 && off2 < lim2 && d1.ha[off1 as usize] == d2.ha[off2 as usize] {
        off1 += 1;
        off2 += 1;
    }
    while off1 < lim1 && off2 < lim2 && d1.ha[(lim1 - 1) as usize] == d2.ha[(lim2 - 1) as usize] {
        lim1 -= 1;
        lim2 -= 1;
    }
    if off1 == lim1 {
        while off2 < lim2 {
            d2.changed[d2.rindex[off2 as usize]] = true;
            off2 += 1;
        }
    } else if off2 == lim2 {
        while off1 < lim1 {
            d1.changed[d1.rindex[off1 as usize]] = true;
            off1 += 1;
        }
    } else {
        let spl = split(
            &d1.ha, off1, lim1, &d2.ha, off2, lim2, kvdf, kvdb, kbase, need_min, env,
        );
        recs_cmp(
            d1, off1, spl.i1, d2, off2, spl.i2, kvdf, kvdb, kbase, spl.min_lo, env,
        );
        recs_cmp(
            d1, spl.i1, lim1, d2, spl.i2, lim2, kvdf, kvdb, kbase, spl.min_hi, env,
        );
    }
}

/// Git's default line diff: returns, for each file, which lines are changed.
pub fn diff_flags<T: Eq + Hash>(old: &[T], new: &[T]) -> (Vec<bool>, Vec<bool>) {
    let (n, m) = (old.len(), new.len());
    let mut changed_old = vec![false; n];
    let mut changed_new = vec![false; m];

    // Classify lines: equal lines share a class id, and each file counts its members.
    let mut classes: HashMap<&T, u64> = HashMap::new();
    let mut len1: Vec<usize> = Vec::new();
    let mut len2: Vec<usize> = Vec::new();
    let mut class_old: Vec<u64> = Vec::with_capacity(n);
    let mut class_new: Vec<u64> = Vec::with_capacity(m);
    for line in old {
        let next = classes.len() as u64;
        let id = *classes.entry(line).or_insert(next);
        if id as usize >= len1.len() {
            len1.push(0);
            len2.push(0);
        }
        len1[id as usize] += 1;
        class_old.push(id);
    }
    for line in new {
        let next = classes.len() as u64;
        let id = *classes.entry(line).or_insert(next);
        if id as usize >= len1.len() {
            len1.push(0);
            len2.push(0);
        }
        len2[id as usize] += 1;
        class_new.push(id);
    }

    // `xdl_trim_ends`.
    let lim = n.min(m);
    let mut dstart = 0;
    while dstart < lim && class_old[dstart] == class_new[dstart] {
        dstart += 1;
    }
    let mut trailing = 0;
    while trailing < lim - dstart && class_old[n - 1 - trailing] == class_new[m - 1 - trailing] {
        trailing += 1;
    }
    let dend1 = n as isize - trailing as isize - 1;
    let dend2 = m as isize - trailing as isize - 1;

    // `xdl_cleanup_records`: lines with no counterpart can never match, and lines with very many
    // counterparts are discarded when they sit among discardable ones.
    let discard = |dstart: usize,
                   dend: isize,
                   classes: &[u64],
                   other_len: &dyn Fn(u64) -> usize,
                   total: usize|
     -> Vec<u8> {
        let mut dis = vec![0u8; total + 1];
        let mlim = bogosqrt(total).min(MAX_EQLIMIT);
        let mut i = dstart as isize;
        while i <= dend {
            let nm = other_len(classes[i as usize]);
            dis[i as usize] = if nm == 0 {
                0
            } else if nm >= mlim {
                2
            } else {
                1
            };
            i += 1;
        }
        dis
    };
    let dis1 = discard(dstart, dend1, &class_old, &|c| len2[c as usize], n);
    let dis2 = discard(dstart, dend2, &class_new, &|c| len1[c as usize], m);

    let mut ha1: Vec<u64> = Vec::new();
    let mut r1: Vec<usize> = Vec::new();
    let mut i = dstart as isize;
    while i <= dend1 {
        let d = dis1[i as usize];
        if d == 1 || (d == 2 && !clean_mmatch(&dis1, i, dstart as isize, dend1)) {
            r1.push(i as usize);
            ha1.push(class_old[i as usize]);
        } else {
            changed_old[i as usize] = true;
        }
        i += 1;
    }
    let mut ha2: Vec<u64> = Vec::new();
    let mut r2: Vec<usize> = Vec::new();
    i = dstart as isize;
    while i <= dend2 {
        let d = dis2[i as usize];
        if d == 1 || (d == 2 && !clean_mmatch(&dis2, i, dstart as isize, dend2)) {
            r2.push(i as usize);
            ha2.push(class_new[i as usize]);
        } else {
            changed_new[i as usize] = true;
        }
        i += 1;
    }

    let (nreff1, nreff2) = (ha1.len() as isize, ha2.len() as isize);
    let ndiags = nreff1 + nreff2 + 3;
    let mut mxcost = bogosqrt(ndiags as usize) as isize;
    if mxcost < MAX_COST_MIN {
        mxcost = MAX_COST_MIN;
    }
    let env = Env { mxcost };
    let size = (ndiags + 2) as usize;
    let mut kvdf = vec![0isize; size + 2];
    let mut kvdb = vec![0isize; size + 2];
    let kbase = nreff2 + 1;

    {
        let mut side1 = Side {
            ha: ha1,
            rindex: r1,
            changed: &mut changed_old,
        };
        let mut side2 = Side {
            ha: ha2,
            rindex: r2,
            changed: &mut changed_new,
        };
        recs_cmp(
            &mut side1, 0, nreff1, &mut side2, 0, nreff2, &mut kvdf, &mut kvdb, kbase, false, &env,
        );
    }
    (changed_old, changed_new)
}
