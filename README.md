# rivet

A Git-compatible version control system, written in Rust.

Rivet reads and writes real Git repositories: the same object format (loose and packed),
the same index, the same refs, the same wire protocol for `git-upload-pack` and
`git-receive-pack`. A repository created by `git` can be opened by `rivet`, and one created
by `rivet` passes `git fsck` and can be cloned with `git` — the differential tests in
`crates/*/tests` run both tools against the same repository and compare the results.

## Status

**Pre-1.0, in active development.** The core object model, index, refs, config, revision
parsing, diffing, three-way merge and the local transports work. The network transports and
the command surface are being filled in; see [`docs/IMPL-ORDER.md`](docs/IMPL-ORDER.md) and
the open items in [`.claude/PRIORITIES.md`](.claude/PRIORITIES.md).

Nothing here is published to crates.io yet.

## Layout

| Crate | Contents |
|-------|----------|
| `rivet-core` | The library: objects, object database, pack files, index, refs, config, revision walking, diff, merge, remotes. |
| `rivet-cli` | The `rivet` binary: argument parsing and one handler per subcommand. |

`rivet-core` is the public API. `rivet-cli` is a binary and exposes no library.

## Building

Rust 1.88 or newer, edition 2024.

```sh
cargo build --release          # target/release/rivet
cargo test --workspace         # unit + differential tests against the real git binary
```

The differential tests shell out to `git`, so a working `git` must be on `PATH`.

## Using it

```sh
rivet init my-project
cd my-project
rivet add file.txt
rivet commit -m "first commit"
rivet remote add origin git@github.com:you/my-project.git
rivet push origin main
```

`rivet <command> --help` documents each command, including the exit codes it returns —
which follow Git's, because scripts branch on them.

## Documentation

* [`docs/SCAFFOLD-BRIEF.md`](docs/SCAFFOLD-BRIEF.md) — the design brief the implementation follows.
* [`docs/IMPL-ORDER.md`](docs/IMPL-ORDER.md) — what is implemented, in what order, and what is next.
* API documentation: **<https://docs.rs/rivet-core>** once published (the crate-level docs are
  the front page there), or locally with `cargo doc --workspace --no-deps --open`.

## License

MIT OR Apache-2.0, at your option.
