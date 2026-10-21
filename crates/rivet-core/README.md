# rivet-core

The library behind [`rivet`](https://github.com/DaQue/rivet), a Git-compatible version
control system.

It reads and writes real Git repositories — object database, pack files and indexes, the
worktree index, refs, config, revision parsing, revision walking, diff, three-way merge, and
the local and network remote transports — with no binding to libgit2 and no `git` process in
the normal paths.

## What it is not

Not a wrapper around the `git` binary, and not a reimplementation of libgit2's API. The types
here describe Git's on-disk and on-the-wire formats directly; the compatibility contract is
checked by differential tests that run the real `git` against the same repositories.

## Use

```toml
[dependencies]
rivet-core = "0.1"
```

```rust
use rivet_core::Repository;

let repo = Repository::open()?;          // the current directory
if let Some(head) = repo.head_commit()? {
    println!("{head}");
}
```

Pre-1.0: the API is not stable yet, and items may move between minor versions.

## Documentation

`cargo doc -p rivet-core --no-deps --open`.

## License

MIT OR Apache-2.0, at your option.
