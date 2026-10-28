//! `commit-tree`: write a commit object for an existing tree, without touching any ref.

use std::io::Read;

use anyhow::Result;
use bstr::BStr;
use rivet_core::{ObjectId, ObjectKind, Repository};

use crate::cli::GlobalArgs;

/// Arguments of `commit-tree`: everything after the command, in Git's own grammar.
#[derive(Debug, clap::Args)]
pub struct CommitTreeArgs {
    /// `<tree> [(-p <parent>)...] [(-m <message>)...] [(-F <file>)...]`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

const USAGE: &str = "usage: git commit-tree <tree> [(-p <parent>)...]
   or: git commit-tree [(-p <parent>)...] [-S[<keyid>]] [(-m <message>)...]
                       [(-F <file>)...] <tree>

    -p <parent>           id of a parent commit object
    -m <message>          commit message
    -F <file>             read commit log message from file
    -S, --[no-]gpg-sign[=<key-id>]
                          GPG sign commit

";

const UTF8_WARNING: &str = "Warning: commit message did not conform to UTF-8.
You may want to amend it after fixing the message, or set the config
variable i18n.commitEncoding to the encoding your project uses.
";

/// `rivet commit-tree <tree> [-p <parent>]... [-m <message>]... [-F <file>]...`.
///
/// # Caveats
///
/// * **The message is not cleaned up.** Each `-m` adds a line-completed paragraph, separated from
///   the previous text by one blank line; `-F` appends the file (or standard input for `-`)
///   verbatim. A message that is still empty afterwards is read from standard input, as Git does.
/// * **`-S`/`--gpg-sign` and a non-UTF-8 `i18n.commitEncoding` are refused by name**: both would
///   change the bytes of the object and neither is implemented.
/// * **Parents and the tree are checked as they are named**, so the first bad argument decides
///   the error, as in Git. A repeated parent is reported once and dropped.
pub fn run_commit_tree(global: &GlobalArgs, args: &CommitTreeArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let mut parents: Vec<ObjectId> = Vec::new();
    let mut message: Vec<u8> = Vec::new();
    let mut positional: Vec<&str> = Vec::new();
    let mut sign = false;

    // clap consumes the first `--`, but after it Git treats `-m` as a tree name, so take the
    // words from the process arguments when the subcommand can be found in them.
    let raw: Vec<String> = std::env::args()
        .skip_while(|word| word != "commit-tree")
        .skip(1)
        .collect();
    let words = if raw.is_empty() { &args.args } else { &raw };
    let mut iter = words.iter();
    let mut only_positional = false;
    while let Some(arg) = iter.next() {
        if only_positional || arg == "-" || !arg.starts_with('-') {
            positional.push(arg);
            continue;
        }
        if arg == "--" {
            only_positional = true;
            continue;
        }
        // `-p<value>` and `-p <value>` are the same thing for the three short options.
        let short = arg
            .as_bytes()
            .get(1)
            .copied()
            .filter(|c| b"pmF".contains(c));
        if let Some(letter) = short.filter(|_| !arg.starts_with("--")) {
            let letter = letter as char;
            let value = if arg.len() > 2 {
                arg[2..].to_string()
            } else {
                match iter.next() {
                    Some(value) => value.clone(),
                    None => {
                        eprintln!("error: switch `{letter}' requires a value");
                        return Ok(129);
                    }
                }
            };
            match letter {
                'p' => {
                    let oid = super::resolve_rev(&repository, &value)
                        .map_err(|_| anyhow::anyhow!("not a valid object name {value}"))?;
                    assert_kind(&repository, &oid, ObjectKind::Commit)?;
                    if parents.contains(&oid) {
                        eprintln!("error: duplicate parent {oid} ignored");
                    } else {
                        parents.push(oid);
                    }
                }
                'm' => {
                    if !message.is_empty() {
                        message.push(b'\n');
                    }
                    message.extend_from_slice(value.as_bytes());
                    if message.last().is_some_and(|b| *b != b'\n') {
                        message.push(b'\n');
                    }
                }
                _ => {
                    if !message.is_empty() {
                        message.push(b'\n');
                    }
                    if value == "-" {
                        std::io::stdin().read_to_end(&mut message)?;
                    } else {
                        let bytes = std::fs::read(&value).map_err(|error| {
                            anyhow::anyhow!(
                                "could not open '{value}' for reading: {}",
                                strip_os_code(&error)
                            )
                        })?;
                        message.extend_from_slice(&bytes);
                    }
                }
            }
            continue;
        }
        if arg == "-S"
            || arg.starts_with("-S") && !arg.starts_with("--")
            || arg == "--gpg-sign"
            || arg.starts_with("--gpg-sign=")
        {
            sign = true;
        } else if arg == "--no-gpg-sign" {
            sign = false;
        } else {
            let name = arg.trim_start_matches('-');
            if arg.starts_with("--") {
                eprintln!(
                    "error: unknown option `{}'",
                    name.split('=').next().unwrap_or(name)
                );
            } else {
                eprintln!(
                    "error: unknown switch `{}'",
                    &name[..name.chars().next().map_or(0, char::len_utf8)]
                );
            }
            eprint!("{USAGE}");
            return Ok(129);
        }
    }

    if positional.len() != 1 {
        anyhow::bail!("must give exactly one tree");
    }
    let spec = positional[0];
    let tree = super::resolve_rev(&repository, spec)
        .map_err(|_| anyhow::anyhow!("not a valid object name {spec}"))?;
    assert_kind(&repository, &tree, ObjectKind::Tree)?;

    if message.is_empty() {
        std::io::stdin()
            .read_to_end(&mut message)
            .map_err(|error| anyhow::anyhow!("git commit-tree: failed to read: {error}"))?;
    }

    if sign {
        anyhow::bail!("commit-tree: signing (-S) is not supported");
    }
    if let Ok(Some(encoding)) = repository.config().get(BStr::new("i18n.commitEncoding")) {
        let lowered = encoding.to_ascii_lowercase();
        if lowered != b"utf-8" && lowered != b"utf8" {
            anyhow::bail!("commit-tree: i18n.commitEncoding other than UTF-8 is not supported");
        }
    }

    // `resolve_identity` falls back to the clock on a date it cannot read; Git dies instead.
    for variable in ["GIT_AUTHOR_DATE", "GIT_COMMITTER_DATE"] {
        if let Some(value) = std::env::var_os(variable) {
            let bytes = value.as_encoded_bytes();
            if rivet_core::object::commit::parse_git_date(BStr::new(bytes)).is_none() {
                anyhow::bail!("invalid date format: {}", value.to_string_lossy());
            }
        }
    }

    if !verify_utf8(&mut message) {
        eprint!("{UTF8_WARNING}");
    }

    let oid = repository.write_commit_object(tree, &parents, BStr::new(&message))?;
    println!("{oid}");
    Ok(0)
}

/// Git's `verify_utf8`: re-encode every byte that is not part of a valid sequence as the Latin-1
/// character of the same value, and report whether the message was valid to begin with.
///
/// Git also rejects U+FFFE and U+FFFF, which Rust's decoder accepts.
fn verify_utf8(message: &mut Vec<u8>) -> bool {
    let mut ok = true;
    let mut pos = 0;
    loop {
        let rest = &message[pos..];
        let (valid, bad) = match std::str::from_utf8(rest) {
            Ok(_) => (rest.len(), false),
            Err(error) => (error.valid_up_to(), true),
        };
        let noncharacter = rest[..valid]
            .windows(3)
            .position(|w| w[0] == 0xef && w[1] == 0xbf && (w[2] == 0xbe || w[2] == 0xbf));
        let at = match (noncharacter, bad) {
            (Some(at), _) => at,
            (None, true) => valid,
            (None, false) => return ok,
        };
        ok = false;
        pos += at;
        let c = message[pos];
        message.splice(pos..=pos, [0xc0 + (c >> 6), 0x80 + (c & 0x3f)]);
        pos += 2;
    }
}

/// Fail with Git's `assert_oid_type` text unless `oid` names an object of `kind`.
fn assert_kind(repository: &Repository, oid: &ObjectId, kind: ObjectKind) -> Result<()> {
    let object = repository
        .odb()
        .read(oid)
        .map_err(|_| anyhow::anyhow!("{oid} is not a valid object"))?;
    if object.kind() != kind {
        let name = match kind {
            ObjectKind::Commit => "commit",
            _ => "tree",
        };
        anyhow::bail!("{oid} is not a valid '{name}' object");
    }
    Ok(())
}

/// `std::io::Error`'s text without the trailing ` (os error N)`, which Git's `strerror` lacks.
fn strip_os_code(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}
