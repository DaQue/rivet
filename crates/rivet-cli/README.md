# rivet-cli

The `rivet` command-line interface — a Git-compatible version control client built on
[`rivet-core`](https://github.com/DaQue/rivet).

This crate is a binary, not a library: it parses arguments, calls into `rivet-core`, and
prints what Git would print. Nothing here is meant to be depended on by another crate; if you
want the functionality, use `rivet-core`.

## Build

```sh
cargo build --release -p rivet-cli   # target/release/rivet
```

## Install from this checkout

```sh
cargo install --path crates/rivet-cli
```

## Commands

`rivet --help` lists them. Each subcommand documents its own exit codes, which follow Git's.

## License

MIT OR Apache-2.0, at your option.
