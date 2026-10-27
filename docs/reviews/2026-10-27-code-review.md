# rivet — independent code review, 2026-10-27

Reviewed at `13c3a81` (clean `master`). Review only; nothing in the tree was changed apart from this file.
Repro scripts used for every "Repro" below were kept outside the repository (`/tmp/rv-scripts/r*.sh`);
each one builds a throwaway repo under `/tmp` with real `git` 2.56 and runs `target/{debug,release}/rivet` on it.

## Ground truth

| Check | Result |
|---|---|
| `cargo build --workspace` | ok |
| `cargo test --workspace --no-fail-fast` | **269 passed, 0 failed, 0 ignored**, 35 suites. `difftest` (3 tests, one of which runs all 315 scenarios) takes 144.6 s. |
| `cargo clippy --workspace --all-targets` | **0 warnings** |
| Same, with `-W` `unwrap_used expect_used indexing_slicing panic too_many_lines cast_possible_truncation` (non-test code) | 842 `indexing may panic`, 446 `slicing may panic`, 96 functions > 100 lines, 132 truncating casts, 34 `expect`, 17 `unwrap` |
| Size | 94,575 lines of Rust under `crates/` (not ~117k; the difference is roughly `tools/difftest/scenarios.sh`, 10,146 lines, plus docs) |

All green. Every bug below is one the suite does not exercise.

---

## A. BROKEN

Ordered by severity. Everything marked **Repro** was run and the output is quoted. "Read only" means I followed the code path but did not build a hostile input for it.

### A1. Deleting packed refs brings them back; a concurrent `git pack-refs`/`gc` can lose refs (data loss)
- **What happens:** `rivet branch -D a b` prints `Deleted branch a` and `Deleted branch b`, but `a` still exists afterwards. `rivet fetch --prune` prints `[deleted] origin/x`, `origin/y` and `origin/z`, but only `origin/z` goes away.
- **Where:** `crates/rivet-core/src/refs/mod.rs:1066-1070` (`delete`) and `:1115-1118` (`rename`) both do `self.packed.clone()`, remove one name and `write` the result. `PackedRefs::write` (`crates/rivet-core/src/refs/packed.rs:307-311`) takes `packed-refs.lock` but never re-reads the file under that lock. The snapshot comes from when the `RefStore` was opened, and `delete(&self)` never updates it. So the second delete rewrites the file from a snapshot that still holds the first ref.
- **Worse case:** a `git gc` or `git pack-refs` that runs between rivet opening the store and rivet deleting a ref. It moves loose refs into `packed-refs` and removes the loose files. Rivet then writes its older snapshot, which drops those refs. Once they expire, the commits they pointed at get pruned.
- **Repro (`r11.sh`):** run `git branch a; git branch b; git pack-refs --all; rivet branch -D a b`. `git branch` then lists `a c main`. For the prune case, a clone with packed tracking refs where upstream deleted x, y and z ends with `origin/x origin/y` still present.
- **Fix:** re-read `packed-refs` after taking the lock, apply the edit, then write (this is what git's `packed_refs_lock` + `repack_without_refs` do).

### A2. Criss-cross merges silently produce a different result from git
- **Where:** `crates/rivet-cli/src/cmd/branching.rs:2342-2343` uses `bases.first()`. The docs admit it picks one base (`crates/rivet-core/src/merge/base.rs:26-31`, `merge/mod.rs:40-44`). GAPS.md does not mention it.
- **Repro (`r7.sh`):** a two-base criss-cross where both sides resolved the line to `a` and B then changed it to `c`. `git merge` stops with a conflict (exit 1). `rivet merge` commits `f = c` and exits 0, in both directions. The user is never told a decision was made for them.
- **Fix:** build a virtual base by merging the bases (as ort/recursive do), or refuse when `bases.len() > 1` until that exists.

### A3. `.gitattributes` and `core.autocrlf` are ignored: rivet makes different blobs from git
- **Where:** there is no attributes support anywhere in core. `crates/rivet-cli/src/cmd/plumbing.rs:42` calls `core.autocrlf` "a non-goal". GAPS.md does not list it.
- **Repro (`r3.sh`):** with `* text=auto` and a CRLF file, `git add` stages `422c2b7…` and `rivet add` stages `c30dea8…`. A rivet commit in such a repository differs from the one git would make. By the same reasoning (not reproduced): `filter=lfs` checkouts write pointer files, and `eol=crlf` checkouts write LF.
- **Fix:** honour `text`/`eol`/`crlf` in `worktree/add.rs` and `worktree/checkout.rs`, or refuse when a `.gitattributes` uses those attributes.

### A4. Revision parsing returns the wrong object, or nothing, for hex-looking names
- **Hex-looking ref names never resolve.** In `crates/rivet-core/src/revparse.rs:165-194` any hex string of 4 or more characters goes through prefix lookup and *returns*. The ref lookup at `:197` is never reached. Git tries refs first. **Repro (`r1.sh`):** with branches `cafe` and `facade`, `git rev-parse cafe` prints the commit; `rivet rev-parse cafe` exits 128 with "unknown revision". The same happens for `rivet log cafe` and anything else that takes a revision. If an object prefix *does* match, rivet picks the object and git picks the branch.
- **Even-length prefixes ending in `00` match too much.** `ObjectId::starts_with` (`crates/rivet-core/src/hash.rs:350-360`) treats every trailing zero byte as fill, so `ab00` is matched as `ab`. **Repro:** the only object starting with `02` is `02d6e514…`. `git rev-parse --verify 0200` fails (exit 1); `rivet rev-parse 0200` prints `02d6e514…` (exit 0). The same flaw makes `Odb::resolve_prefix` (`crates/rivet-core/src/odb/mod.rs:547`) treat a *full* id containing a `00` byte as a prefix: about 7.5 % of SHA-1 ids go down the slow scan path.
- **Fix:** carry the prefix length in nibbles (as gix's `Prefix` does) and resolve refs before short hashes.

### A5. Delta chains deeper than 50 cannot be read
- **Where:** `MAX_DELTA_DEPTH = 50` at `crates/rivet-core/src/pack/mod.rs:149`, enforced at `pack/reader.rs:357-375`. Git has no read-side limit. Packs from `pack.depth` > 50, `fast-import --depth`, or older `gc --aggressive` (250) all contain longer chains.
- **Repro (`r2c.sh`):** `git fast-import --depth=200` gives chains up to 98 long. `git fsck` is clean. `rivet fsck` reports 48 objects as `object corrupt or missing: delta chain for 0000000000000000000000000000000000000000 is deeper than 50 levels` and exits 1. `rivet cat-file -s 030c2d7…` fails, while git answers `14643`.
- The error also names the all-zero id (`reader.rs:359,373`); see B2.
- **Fix:** raise the limit to ≥ 4095 (the loop is already iterative), or detect cycles by tracking visited offsets.

### A6. Shallow clones are unusable
- **Where:** `.git/shallow` is never read; `crates/rivet-core/src/remote/mod.rs:52` lists shallow as unsupported. GAPS.md only mentions it inside the submodule bullet.
- **Repro:** in a `git clone --depth=2`, `rivet log --oneline` prints *nothing* and exits 128 with `object not found: 7737ee0…`. `rivet fsck` reports `missing commit`.
- This is the default CI checkout (actions/checkout uses `fetch-depth: 1`).

### A7. Unknown repository extensions are not refused
- **Where:** `crates/rivet-core/src/repo.rs:468-471` only rejects a format version above 1. It never checks `extensions.*`.
- **Repro (`r5.sh`):** with `repositoryformatversion=1` and `extensions.frobnicate=true`, `git status` fails with `unknown repository extension found`; `rivet status` exits 0.
- **Real consequences:**
  - `extensions.preciousObjects` is ignored, so `rivet gc` deletes packs and objects that git refuses to touch in a shared object store.
  - `extensions.worktreeConfig` is only honoured for sparse settings (`crates/rivet-core/src/sparse.rs:281-335`); `config.worktree` is otherwise never read.
  - `extensions.refStorage=reftable` only fails by accident: `fatal: invalid ref name refs/heads/.invalid`, after `rivet add` has already written objects.

### A8. Commits with malformed-but-real identity lines make `log` abort
- **Where:** `crates/rivet-core/src/object/commit.rs:373-390` turns any `Signature::parse` failure into a hard `InvalidHeader`. Git's `log` tolerates these (fsck has dedicated msg-ids for them: `missingEmail`, `badTimezone`, `badDateOverflow`, …).
- **Repro (`r10.sh`):** each of `author Foo 1234 +0000`, `author Foo <foo@x> 1234`, an overflowing date, and `+00000` gives `git log` output and `fatal: commit object has a malformed author header` from rivet. One such commit anywhere in history breaks `rivet log`, `blame`, `rebase` and so on for the whole repository. Imported SVN/CVS histories contain these.

### A9. Index formats git writes by default under common settings
- **v4 is refused outright.** `crates/rivet-core/src/index/mod.rs:1084-1088`. `feature.manyFiles=true` sets `index.version=4`. **Repro (`r4.sh`):** `fatal: unsupported index version 4` from `status` and `ls-files`. Not in GAPS.
- **Split index is misread silently.** Only `sdir` is refused (`index/mod.rs:125,1135`); `link` is accepted and the shared index is never loaded. **Repro:** with `core.splitIndex=true`, `rivet ls-files` prints five *empty* names and exits 0, and `rivet status` fails with `i/o error on : Is a directory` (the path in the message is empty).

### A10. Alternates are not read
- **Where:** `crates/rivet-core/src/odb/mod.rs:127-130`.
- **Repro (`r3.sh`):** in a `git clone --shared` repo, `rivet log` says `your current branch 'main' does not have any commits yet`, which is false. `rivet status` gives `object not found`.
- This also covers `clone --reference`, and rivet's own `submodule update --reference` (GAPS.md:85 says rivet writes `alternates` that it cannot read).

### A11. Fetch updates refs without checking that the history arrived
- **Where:** `crates/rivet-core/src/remote_ops.rs:505-509` stores the pack (`store_downloaded`, `:329-335`) and then writes refs. There is no `check_connected` equivalent anywhere (grep for connectivity / "did not send all necessary objects" finds only fsck).
- **Effect:** a buggy or hostile server that sends an incomplete pack leaves refs pointing at commits whose trees or parents are missing. Git aborts the fetch with `did not send all necessary objects`.
- **Status:** read only; not reproduced against a hostile server.

### A12. Nothing is fsynced before `gc` deletes the old copies
- **Where:** `PackWriter::finish_to_path` (`crates/rivet-core/src/pack/writer.rs:395-410`) writes and renames without `sync_all`. `gc` then removes the old packs and loose objects (`crates/rivet-core/src/gc.rs:1408-1430`). The only `sync_all` in the tree is in `worktree/checkout.rs:764,778`.
- **Effect:** git's default `core.fsync` covers packs and pack metadata. A power loss after rivet's deletes but before writeback can lose the whole object store.
- **Also:** the temp names `pack-<sha>.pack.tmp` are deterministic (`writer.rs:393-394`), so two concurrent writers of the same pack truncate each other's file.

### A13. Hostile packs can abort the process before validation
- **Delta target size:** `apply_delta` (`crates/rivet-core/src/pack/delta.rs:165-171`) does `Vec::with_capacity(target_len)` straight from the delta header. On 64-bit the `> usize::MAX` guard can never fire, so a 2^62 target triggers an allocation failure (abort), not an `Error`. The doc at `delta.rs:264-267` claims the opposite.
- **Zlib bombs:** `inflate_at` (`pack/mod.rs:670-712`) inflates without bounding the output by the entry's declared size; the check at `reader.rs:349` / `pack/mod.rs:377` only runs after everything is in memory.
- Both are reachable through `fetch`/`clone`, via `index_pack`.
- **Status:** read only; not reproduced.

### A14. `rev-parse --short HEAD` fails, and `log` abbreviates differently from git
- **`--short` swallows the revision.** It is `Option<Option<usize>>` without `require_equals` (`crates/rivet-cli/src/cmd/plumbing.rs:422-423`), so clap tries to parse `HEAD` as the number. **Repro:** `error: invalid value 'HEAD' for '--short [<N>]'`. This is one of the most common idioms in scripts.
- **Fixed 7-character abbreviations.** `log`/`show` use `history::abbrev` (`crates/rivet-cli/src/cmd/history.rs:1060-1085`): always 7 or more, ignoring `core.abbrev` and git's auto-length. **Repro (44k objects):** `git log --oneline -1` prints `54c947ee`, rivet prints `54c947e`. A correct implementation already exists in `query.rs:840-888`; see B1.

### A15. Public `refs::loose` API panics on SHA-256 repositories
- **Where:** `crates/rivet-core/src/refs/loose.rs:276,298,379` hardcode `ObjectFormat::Sha1`. `RefTarget::to_bytes` then hits `.expect("write hex")` (`refs/mod.rs:190-191`) for a 32-byte id, and `rename` parses with SHA-1.
- **Effect:** none inside the crate today (no internal callers), but `refs::loose` is `pub` (`refs/mod.rs:64`), so any library user writing a ref in a SHA-256 repo panics.

---

## B. CODE SMELLS

### B1. Duplicated plumbing, with diverging semantics

| What | Copy 1 | Copy 2 | Difference / fix |
|---|---|---|---|
| Index parsing | `Index::load` (`index/mod.rs:1060-1158`) | `Index::parse` (`:1172-1249`) | `parse` hardcodes a 20-byte checksum (`:1199,1231,1237`), so it rejects every SHA-256 index. It also accepts v4, which `load` refuses. Make `load` = read file + `parse`. |
| Abbreviation | `history::abbrev` (`history.rs:1060`): O(objects) per call, see B3 | `Abbreviator` (`query.rs:840-888`): correct | Move `Abbreviator` into core next to `Odb`. |
| Revision walking | core `revwalk.rs` (used by 10 CLI files) | CLI `cmd/gitwalk.rs` (a port of git's `revision.c` order, used by 2) | `log` and `rev-list` can order commits differently. Pick one and put it in core. |
| Fetch | `remote::fetch::fetch` (`remote/fetch.rs:393`) | `remote_ops::fetch_remote` (`remote_ops.rs:338`) | The first has no callers (dead public API), and its `negotiate` is a stub; see B6. Delete it or make it the implementation. |
| Kind + bytes → `Object` | `odb/loose.rs:228-233` | `pack/mod.rs:636-643`, `odb/packed.rs:224-229` | Use one `Object::from_kind`. |
| OFS_DELTA distance decoding | `pack/delta.rs:324-350` | inline in `pack/reader.rs:477-502` | Call the function. |
| Pack loading | `Odb::open_objects_dir` (`odb/mod.rs:200-214`) | `Odb::refresh` (`:287-310`) | `refresh` re-reads *every* pack fully into memory just to compare checksums. Compare file names first. |
| `IoAtPath` mapping boilerplate | 77 hand-written `map_err(… IoAtPath { path: BString::from(p.to_string_lossy()…) })` in core | — | Add `trait IoContext { fn at(self, &Path) }`. |

### B2. Error handling that loses information or panics
- **Error variants carry an all-zero id instead of the real one:** `DeltaChainTooDeep` and `MissingDeltaBase` (`pack/reader.rs:359,373,395`, `pack/mod.rs:494-498`). Users see `delta chain for 0000…` (A5 output).
- **`index_pack` discards every base-provider error** (`Err(_) => None` at `pack/mod.rs:463-468`), so I/O failures or corruption are reported as a missing base.
- **`merge_bases(...).unwrap_or_default()` turns corruption into "no common ancestor"** in `rebase.rs:613,647`, `query.rs:384,1414`, `history.rs:1652`, `submodule.rs:2624` and `bisect.rs:995`. A rebase over a damaged odb replays the whole branch instead of failing. Propagate everything except `NoMergeBase`.
- **Other swallowed errors:**
  - `history.rs:1078` (`.unwrap_or(true)`) and `remote_ops.rs:311` (`if let Ok(all)`): a refs read error silently sends no haves.
  - `commit.rs:374-390` (`map_err(|_| …)`) drops the parser's reason.
- **`ObjectFormatMismatch { expected: format, found: format }`** (`remote/fetch.rs:550-553`): the error cannot say what it found.
- **Panicking serializers** in non-test code: `Index::to_bytes` `.expect`s at `index/mod.rs:1280,1292`, and `RefTarget::to_bytes` at `refs/mod.rs:191`. Return `Result`.
- **`commit.rs:401-424` tracks presence twice** (a `required: [bool; 3]` array plus `Option`s, then `.unwrap()`). Use `let Some(tree) = tree else { return Err(…) }`.
- **The catch-all string error has many uses.** `Error::Message(String)` (`error.rs:832`) is used 44 times in core (`remote_ops.rs` 9, `bundle.rs` 8, `gc.rs` 6, …), against the stated rule "every parser returns a specific variant". `Error::Protocol(format!(…))` adds another 14.

### B3. Hot paths that scale with repository size per call

| Path | Where | Measured (synthetic repo, 3,000 commits, 20k files, 44k objects, release build) |
|---|---|---|
| `log --oneline` / any `%h`: every abbreviation enumerates every object id and allocates a hex `String` per id | `history.rs:1060-1085` | **9.94 s vs git 0.01 s.** `log --format=%H` takes 0.05 s. `-n 1000` takes 3.3 s. Linear in commits × objects. |
| Odd-length short hash (`git log 2461a6d`, the common case) | `revparse.rs:170-189` | `odb.oids()` plus `to_hex()` for every object on every resolution |
| Even-length prefix | `Odb::resolve_prefix`, `odb/mod.rs:561-567` | Same full scan. The doc at `:534-537` claims the fanout table avoids this, but it is never used. |
| Packed reads | `odb/mod.rs:673-687` caches only REF_DELTA bases (keyed `(id, 0)`) | OFS chains (git's default) are re-inflated from the root on every read (`reader.rs:346-416`). `DeltaBaseCache` exists but is keyed for exactly this and unused for it. |
| `Odb::read_header` for packed objects | `odb/mod.rs:451-453` | Fully reads, resolves and hashes the object; the doc at `:431-433` claims a fast path for undeltified entries. `LooseObjectStore::read_header` (`loose.rs:280-303`) inflates the whole stream although its doc (`:239-241`) says it stops at the NUL. |
| `index_pack` | `pack/mod.rs:367-421,470,491,501` | Holds every inflated payload and every resolved object, clones each base per delta, clones every object again just to hash it, and hashes each object twice. Peak memory is more than 2× the uncompressed repository. `fix_thin_pack` (`:552-577`) inflates everything a second time. |
| Pack residency | `odb/mod.rs:117-120,210` | Every command `fs::read`s every pack at open. |
| `blame d7/f7` | (cause not traced) | **80.6 s vs git 0.02 s** |
| `local_tips` | `remote_ops.rs:315` | `Vec::contains` makes it O(refs²) |

### B4. Stringly typed control flow
- **`main.rs:130-145` sorts errors by sniffing message prefixes:** `reason.starts_with("RPC failed; ")`, `"could not read "`, `"credential helper "`. Make `Authentication` carry an enum kind.
- **Strategy names are compared as strings:** `strategy != "recursive"` (`branching.rs:1166`). Use a `MergeStrategy` enum parsed by clap (`ValueEnum`).
- **`match x.as_str()` over subcommand/option words** that clap could model: 11 in `submodule.rs`, 8 in `worktree.rs`, 5 in `bundle.rs`.
- **Hand-rolled argv rewriting** (`main.rs:57-121`, with booleans `in_log`/`in_format_patch`/`in_blame`) works around clap for `-<n>`, `-M<n>` and `-C<n>`. It is per-command state outside the command, and it cannot see global options (A14, C). Move it to per-subcommand pre-parsers.

### B5. Over-long functions
96 functions are over 100 lines. The worst:

| Function | Lines |
|---|---|
| `cmd/snapshot.rs:984` | 514 |
| `remote_ops.rs:338` (`fetch_remote`) | 416 |
| `cmd/query.rs:314` | 411 |
| `cmd/patch.rs:543` | 355 |
| `merge/tree.rs:92` | 335 |
| `remote_ops.rs:841` | 319 |
| `cmd/foreachref.rs:1415` | 315 |
| `cmd/grep.rs:188` | 313 |
| `config/mod.rs:336` | 231 |

`fetch_remote` mixes refspec mapping, tag following, FETCH_HEAD writing and pruning. Split it into `plan_mappings` / `download` / `apply` stages that can each be tested.

### B6. Module boundaries that leak
- **`refs::loose::{write, write_locked, delete, rename}` are public.** They bypass `RefStore`'s invariants (packed-refs shadowing, reflogs, format width; see A15). Make them `pub(crate)`.
- **`remote::fetch::negotiate` is a stub behind real-looking docs** (`fetch.rs:540-598`):
  - capabilities are copied from the request, not intersected with the advertisement as its doc claims (`:522-525`);
  - `_haves` is computed and thrown away (`:564`);
  - `common`, `negotiation_rounds` and `ready` are constants.
- **`AUTH_HEADER` is a thread-local** (`remote/http.rs:100-115`), so request auth state is global per thread rather than per transport.
- **Large parts of the VCS live in the binary crate,** not the library: the sequencer and rebase (`cmd/rebase.rs` 1,565 lines plus `rebase/interactive.rs` 1,373), merge orchestration (`branching.rs`), the git-order walker (`gitwalk.rs`) and the graph renderer.
- **Unix-only core:** 44 uses of `std::os::unix` against 17 `cfg` gates, for example ungated in `repo.rs:450,503-504`. I did not compile for a Windows target, but core will not build there.

### B7. Documentation that contradicts the code
- `README.md:14-16` says hooks, credential helpers, sparse checkout and interactive rebase are missing; `GAPS.md:26-60,156-202` documents them as implemented.
- `docs/IMPL-ORDER.md:3` still says "Scaffold is signature-only … `todo!()`".
- `AGENTS.md` "Current work: T04 tag signature" does not match IMPL-ORDER's T04 (`index/mod.rs`).
- `LooseObjectStore::read` promises `HashMismatch` (`loose.rs:171`) but returns `CorruptObject` (`:221-226`).
- `index/mod.rs:98-101` says a saturated 0xFFF name length is unrecoverable; git writes exactly that and reads by NUL.
- `GAPS.md:45` refers to "`update-index` details", but there is no `update-index` command.

---

## C. MISSING FEATURES

### Not listed in GAPS.md at all
- **Attributes:** `.gitattributes` (`text`/`eol`/`crlf`, `filter` clean/smudge so no LFS, `diff`/`merge` drivers, `binary`) and `core.autocrlf` (A3). Belongs in a new `rivet-core/src/attr/`, used by `worktree/add.rs`, `worktree/checkout.rs`, `diff/` and `merge/content.rs`.
- **Object storage:** reading shallow repositories (A6, `.git/shallow` in `revwalk.rs` and `merge/base.rs`) and alternates (A10, `odb/mod.rs`).
- **Index:** v4, split index, and using (not just preserving) `UNTR`/`FSMN` (A9, `index/mod.rs`).
- **Repository format:** checking `extensions.*`, `preciousObjects`, general `config.worktree` layering, and reftable (A7, `repo.rs`, `config/`).
- **Merging criss-cross histories via a virtual base** (A2, `merge/`).
- **Global options** `-C <dir>`, `-c key=value`, `--git-dir`, `--work-tree`. **Repro (`r8.sh`):** `error: unexpected argument '-C' found`, and the same for `--git-dir`; only `--repo`/`GIT_DIR` work. Belongs in `cli.rs` `GlobalArgs` plus the config overlay in `config/`.
- **Read-side performance structures:** multi-pack-index, commit-graph, bitmaps (`gc` writes `.rev` but nothing reads any of them). On a large repository this is the difference between seconds and minutes.
- **Protocol v2 over HTTP.** `advertise` explicitly asks for v0 (`remote/http.rs:1014-1016`), so there is no `ls-refs` ref-prefix filtering. Hosts with 100k+ `refs/pull/*` send the whole advertisement on every fetch.
- **Config keys that are silently not honoured:** `diff.renameLimit` (constant at `diff/rename.rs:24`) and `core.abbrev` in `log` (A14).
- **CLI gaps for common usage:**
  - `branch -D a b c`: more than two names is a clap error, because the grammar is `[BRANCH] [START-POINT]`.
  - `rev-parse --short <rev>` (A14).
- **Plumbing commands that scripts and tools call:** `update-ref`, `update-index`, `read-tree`, `commit-tree`, `mktree`, `mktag`, `diff-tree`, `diff-index`, `diff-files`, `name-rev`, `index-pack`, `unpack-objects`, `pack-objects`, `prune`, `check-attr`, `check-ref-format`, `var`, `rerere`, `range-diff`, `maintenance`.
- **A `gc.pid` lock:** a concurrent `git gc --auto` and `rivet gc` are not serialized.

### Listed in GAPS.md
Partial clone and filters; sparse index; `bundle-uri` beyond files; `archive --remote` over the network; server-side hooks; `reference-transaction`; `pre-auto-gc`/`post-index-change`; `apply` binary patches and `--3way` with `--reject`; `format-patch` cover letters and threading; `stash -p/-S/--staged`; `gc --cruft/--geometric/--max-pack-size`; `bisect --first-parent`; `grep -P/--and/--no-index`; `blame -L :func/--since`; multi-round credential `continue=1`; `credential.<url>.*`; `pull --rebase <remote> <branch>` fork point; v2 capabilities beyond `object-format`.

---

## D. ARCHITECTURE / TEST GAPS

### Design problems that will get worse at 2× the code
1. **No single "repository view".** Config layering, `config.worktree`, extension checks, attributes and alternates each need to be consulted by many commands, and today each is read ad hoc or not at all (A3, A7, A10). Add one `Repository` capability object that owns them; otherwise every new feature re-decides layering.
2. **Snapshot state with in-place mutation.** `RefStore` caches `packed` at open and mutates through `&self` (A1); `Odb` caches the pack list and only ever adds to it (`odb/mod.rs:273-276`). Any long-running operation (gc, rebase, fetch with prune) is exposed to stale writes. Ref writes need a transaction type that re-reads under lock: `RefTransaction { updates, deletes } → commit()`. That is also the prerequisite for `reference-transaction` (GAPS §2).
3. **Everything in memory.** Every pack is in memory (`odb/mod.rs:210`), `index_pack` keeps every object (B3), and pack writing buffers the whole pack (`writer.rs:389`). This caps repository size at RAM. mmap plus a streaming `index_pack` with a bounded delta cache has to come before more porcelain is built on the current types.
4. **Logic in the CLI crate.** Rebase, sequencer, merge orchestration and one of the two walkers live in `rivet-cli` (B6). Embedders get none of it, and the differential tests are the only tests it has.
5. **Unix-only core** (B6). Retrofitting `cfg` gates across 44 sites gets harder with every file.

### Test gaps
- **Every A-item above passes the suite.** The 315 differential scenarios exercise what rivet *writes*. They almost never start from a repository that *git* wrote with non-default settings: shallow, `--shared`, `feature.manyFiles`, `splitIndex`, `pack.depth > 50`, attributes, criss-cross history, hex-named branches, malformed identities, or ≥ 32k objects. Add a "git-authored fixtures" scenario family: build with git using each non-default setting, then run read-only rivet commands and compare the output.
- **The scenarios run as a single Rust test** (`crates/rivet-cli/tests/difftest.rs`, `every_scenario_matches_real_git`). A failure is one red test in a 145 s run; there is no per-scenario granularity in `cargo test` output or for test filtering.
- **No test does two operations in one process against packed refs** (A1).
- **No concurrency tests:** git and rivet interleaved on refs, gc, or the index.
- **No hostile-input fuzzing** for `apply_delta`, `inflate_at`, `PackFile::entry_at`, `Index::parse` or the commit/tag parsers (A13). The only robustness test (`tests/robustness.rs`) uses corrupted git repositories, not adversarial headers. `proptest` is already a dev dependency.
- **No performance gate.** `tools/difftest/scale.sh` exists, but nothing fails on a 1000× regression such as `log --oneline` (B3).
- **Self-referential unit tests.** Most `#[cfg(test)]` tests in `index/`, `refs/` and `odb/` round-trip rivet's own output (for example `index/mod.rs:1840-2052` serializes with `to_bytes` and re-parses). The `*_git_differential.rs` files are the exception, and they cover only `Sha1` and default settings.
- **`Index::parse` is only exercised by tests** (`tests/index_git_differential.rs:234`), and only with SHA-1, which is why its 20-byte checksum assumption (B1) survives.

---

## E. TOP 10

| # | Change | Size |
|---|---|---|
| 1 | Re-read `packed-refs` under its lock before rewriting it, in `delete` and `rename`, and add a two-deletes-in-one-process test (A1) | S |
| 2 | Resolve refs before short hashes and carry prefix length in nibbles; fixes `cafe`, `0200` and full ids containing `00` (A4) | S |
| 3 | Raise `MAX_DELTA_DEPTH` to ≥ 4095, or detect cycles by visited offset, and report real ids in delta errors (A5, B2) | S |
| 4 | Refuse unknown `extensions.*` with v1, and honour `preciousObjects` in gc (A7) | S |
| 5 | Replace `history::abbrev` with a core `Abbreviator` (auto length, `core.abbrev`), and fix `rev-parse --short HEAD` (A14, B3) | S |
| 6 | Refuse criss-cross merges, or implement a virtual base (A2) | S (refuse) / L (implement) |
| 7 | Read `.git/shallow` and `objects/info/alternates` in the walker and odb (A6, A10) | M |
| 8 | Make commit/tag identity parsing lenient like git's `split_ident_line`, keeping strictness for `fsck` (A8) | M |
| 9 | fsync packs and idx before gc deletes anything; add a post-fetch connectivity check; bound inflate and delta allocation by the declared sizes (A11–A13) | M |
| 10 | Add a git-authored-fixture scenario family (shallow, shared, manyFiles, splitIndex, deep deltas, attributes, criss-cross) and split `difftest` into one test per scenario (D) | M |

Next in line after the ten: attributes and eol support (A3, L); index v4 read/write (A9, M); mmap plus a streaming `index_pack` (D3, L).

---

## UNVERIFIED SUSPICIONS

- **HTTP credentials after a redirect.**
  - Once `redirected_base` is set (`remote/http.rs:1020-1026`), helper credentials are still requested for the *original* URL (`settings.credential_for(url, …)` at `:959`). The helper-provided `Authorization` value goes into the thread-local `AUTH_HEADER`, which `with_authorization` (`:110-115`) adds to every request regardless of host. A `Bearer`-type credential for host A may therefore be sent to redirect target B.
  - Basic credentials, by contrast, are silently dropped after a redirect, because `base_url` returns the redirected base without userinfo (`:309-312`). Authenticated push through a redirect would then fail.
  - Not reproduced: it needs two HTTP servers.
- **`blame` cost.** 80 s for a two-line file over 3,000 commits suggests whole-tree reads or diffs per commit, compounded by B3's abbreviations. Not profiled.
- **`resolve_prefix` on full ids containing a `00` byte** (A4) should give correct but slow answers; I did not construct such an id to time it.
- **Stale `UNTR`/`FSMN` extensions** are preserved verbatim across rivet index writes (`index/mod.rs:1283-1289`; only `TREE` is invalidated, at `:1494-1496`). Git revalidates `UNTR` against directory stat data, so this is probably harmless, but stale fsmonitor tokens were not tested.
- **A file deleted by rivet's loose-ref `delete`** prunes empty parent directories with no stop at `refs/` (`refs/loose.rs:333-348`). In a repository without `refs/tags/`, deleting the last loose branch could remove `refs/` itself, after which git no longer recognises the directory as a repository. Not reproduced.
