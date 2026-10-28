//! `update-ref`: create, move, verify and delete refs, one at a time or as a transaction.

use std::collections::HashSet;
use std::io::Read;

use anyhow::{Result, bail};
use bstr::BString;
use rivet_core::{ObjectId, RefLock, RefName, RefTarget, ReflogEntry, Repository};

use crate::cli::GlobalArgs;

/// Arguments to `rivet update-ref`.
#[derive(Debug, clap::Args)]
pub struct UpdateRefArgs {
    /// The reason recorded in the reflog.
    #[arg(short = 'm', value_name = "REASON")]
    pub message: Option<String>,

    /// Delete the reference.
    #[arg(short = 'd')]
    pub delete: bool,

    /// Update the named ref itself, not the ref it points to.
    #[arg(long = "no-deref", overrides_with = "deref")]
    pub no_deref: bool,

    /// Opposite of `--no-deref`.
    #[arg(long = "deref", overrides_with = "no_deref")]
    pub deref: bool,

    /// Standard input is NUL-terminated.
    #[arg(short = 'z')]
    pub nul: bool,

    /// Read updates from standard input.
    #[arg(long = "stdin", overrides_with = "no_stdin")]
    pub stdin: bool,

    /// Do not read updates from standard input.
    #[arg(long = "no-stdin", overrides_with = "stdin")]
    pub no_stdin: bool,

    /// Create a reflog even where `core.logAllRefUpdates` would not.
    #[arg(long = "create-reflog", overrides_with = "no_create_reflog")]
    pub create_reflog: bool,

    /// Do not force a reflog.
    #[arg(long = "no-create-reflog", overrides_with = "create_reflog")]
    pub no_create_reflog: bool,

    /// `<ref> <new> [<old>]`, or with `-d` `<ref> [<old>]`.
    #[arg(value_name = "ARG")]
    pub names: Vec<String>,
}

const USAGE: &str = "usage: git update-ref [<options>] -d <refname> [<old-oid>]
   or: git update-ref [<options>]    <refname> <new-oid> [<old-oid>]
   or: git update-ref [<options>] --stdin [-z] [--batch-updates]

    -m <reason>           reason of the update
    -d                    delete the reference
    --no-deref            update <refname> not the one it points to
    --deref               opposite of --no-deref
    -z                    stdin has NUL-terminated arguments
    --[no-]stdin          read updates from stdin
    --[no-]create-reflog  create a reflog
    -0, --[no-]batch-updates
                          batch reference updates

";

/// What a command expects the ref to hold before it is touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Old {
    /// No expectation.
    Any,
    /// The ref must not exist.
    Absent,
    /// The ref must hold this id.
    Is(ObjectId),
}

/// What a command does once the expectation holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Write(ObjectId),
    Delete,
    Verify,
}

/// One queued command: the ref it was named with, the ref it lands on, and what to do there.
#[derive(Debug)]
struct Op {
    /// The ref as the command named it; it differs from `target` when a symbolic ref was followed.
    named: RefName,
    target: RefName,
    old: Old,
    action: Action,
}

/// Handler for `rivet update-ref`.
///
/// # Exit codes
///
/// `0`; `1` for a failed `-d`; `128` for a fatal error; `129` for bad arguments (usage on stderr).
///
/// # Caveats
///
/// * `--stdin` accepts `update`, `create`, `delete`, `verify`, `option no-deref` and
///   `start`/`prepare`/`commit`/`abort`. The `symref-*` commands are refused by name.
/// * A `delete` releases its lock just before the ref is removed, because the removal takes the
///   lock itself.
pub fn run_update_ref(global: &GlobalArgs, args: &UpdateRefArgs) -> Result<i32> {
    let stdin = args.stdin && !args.no_stdin;
    let positional_ok = if stdin {
        args.names.is_empty() && !args.delete
    } else if args.nul {
        false
    } else if args.delete {
        matches!(args.names.len(), 1 | 2)
    } else {
        matches!(args.names.len(), 2 | 3)
    };
    if !positional_ok {
        eprint!("{USAGE}");
        return Ok(129);
    }
    let repository = super::open(global)?;
    let session = Session {
        repository: &repository,
        reason: args.message.clone().unwrap_or_default(),
        force_log: args.create_reflog && !args.no_create_reflog,
    };
    let no_deref = args.no_deref && !args.deref;
    if stdin {
        return session.run_stdin(args.nul, no_deref);
    }

    let name = args.names[0].as_str();
    let Ok(parsed) = RefName::from_bytes(name.as_bytes()) else {
        bail!("update_ref failed for ref '{name}': refusing to update ref with bad name '{name}'");
    };
    let zero = ObjectId::zero(repository.format());
    let (new_arg, old_arg) = if args.delete {
        (None, args.names.get(1))
    } else {
        (Some(&args.names[1]), args.names.get(2))
    };
    let new = match new_arg {
        Some(spec) => Some(positional_oid(&repository, spec)?),
        None => None,
    };
    let old = match old_arg {
        Some(spec) if spec.is_empty() => Old::Absent,
        Some(spec) => old_from(positional_oid(&repository, spec)?, zero),
        None => Old::Any,
    };
    let action = match new {
        Some(new) if new != zero => Action::Write(new),
        _ => Action::Delete,
    };
    let op = session.plan(&parsed, old, action, no_deref);
    match session.commit(vec![op]) {
        Ok(()) => Ok(0),
        Err(reason) if args.delete => {
            eprintln!("error: {reason}");
            Ok(1)
        }
        Err(reason) => bail!("update_ref failed for ref '{name}': {reason}"),
    }
}

/// A positional id: anything the revision parser accepts, with git's complaint otherwise.
fn positional_oid(repository: &Repository, spec: &str) -> Result<ObjectId> {
    super::resolve_rev(repository, spec).map_err(|_| anyhow::anyhow!("{spec}: not a valid SHA1"))
}

/// The expectation an id stands for: the all-zero id means "must not exist".
fn old_from(oid: ObjectId, zero: ObjectId) -> Old {
    if oid == zero {
        Old::Absent
    } else {
        Old::Is(oid)
    }
}

/// The shared state of one invocation.
struct Session<'a> {
    repository: &'a Repository,
    reason: String,
    force_log: bool,
}

impl Session<'_> {
    /// Resolve `name` to the ref the command lands on and queue it.
    fn plan(&self, name: &RefName, old: Old, action: Action, no_deref: bool) -> Op {
        let mut target = name.clone();
        if !no_deref {
            // Follow symbolic refs to their end; a dangling one lands on its target.
            for _ in 0..10 {
                match self.repository.refs().try_read(&target) {
                    Ok(Some(RefTarget::Symbolic(next))) => target = next,
                    _ => break,
                }
            }
        }
        Op {
            named: name.clone(),
            target,
            old,
            action,
        }
    }

    /// The directory that holds `name` and its lock.
    fn home(&self, name: &RefName) -> &std::path::Path {
        let bytes = name.as_bytes();
        let private = !bytes.starts_with(b"refs/")
            || bytes.starts_with(b"refs/bisect/")
            || bytes.starts_with(b"refs/worktree/")
            || bytes.starts_with(b"refs/rewritten/");
        if private {
            self.repository.git_dir()
        } else {
            self.repository.common_dir()
        }
    }

    /// Lock every ref and check every expectation; the locks come back to the caller.
    fn prepare(&self, ops: &[Op]) -> Result<Vec<RefLock>, String> {
        let mut seen = HashSet::new();
        for op in ops {
            if !seen.insert(op.target.clone()) {
                return Err(format!(
                    "multiple updates for ref '{}' not allowed",
                    op.target
                ));
            }
        }
        let mut locks = Vec::new();
        for op in ops {
            let lock = RefLock::acquire(self.home(&op.target), &op.target)
                .map_err(|e| format!("cannot lock ref '{}': {e}", op.target))?;
            let current = self
                .repository
                .refs()
                .resolve_oid(&op.target)
                .ok()
                .filter(|_| {
                    self.repository
                        .refs()
                        .try_read(&op.target)
                        .ok()
                        .flatten()
                        .is_some()
                });
            let refuse = |why: String| format!("cannot lock ref '{}': {why}", op.target);
            match (op.old, current) {
                (Old::Any, _) => {}
                (Old::Absent, Some(_)) => return Err(refuse("reference already exists".into())),
                (Old::Absent, None) => {}
                (Old::Is(_), None) => {
                    return Err(refuse(format!(
                        "unable to resolve reference '{}'",
                        op.target
                    )));
                }
                (Old::Is(want), Some(found)) if want != found => {
                    return Err(refuse(format!("is at {found} but expected {want}")));
                }
                (Old::Is(_), Some(_)) => {}
            }
            locks.push(lock);
        }
        Ok(locks)
    }

    /// Write what the prepared transaction decided.
    fn apply(&self, ops: &[Op], locks: Vec<RefLock>) -> Result<(), String> {
        let refs = self.repository.refs();
        let format = self.repository.format();
        let zero = ObjectId::zero(format);
        let who = self
            .repository
            .committer_signature()
            .map_err(|e| e.to_string())?;
        for (op, mut lock) in ops.iter().zip(locks) {
            let current = refs.resolve_oid(&op.target).ok();
            match op.action {
                Action::Verify => {
                    let _ = lock.release();
                }
                Action::Delete => {
                    let _ = lock.release();
                    if current.is_some() || refs.exists(&op.target) {
                        refs.delete(&op.target).map_err(|e| e.to_string())?;
                    }
                }
                Action::Write(new) => {
                    let old = current.unwrap_or(zero);
                    let entry = ReflogEntry::new(
                        old,
                        new,
                        who.clone(),
                        BString::from(self.reason.as_bytes()),
                    );
                    // The ref itself is rewritten and logged only when its value changes.
                    let changed = current != Some(new);
                    if changed {
                        std::fs::write(lock.path(), RefTarget::Direct(new).to_bytes(format))
                            .map_err(|e| e.to_string())?;
                        if self.force_log || self.repository.should_log_ref(&op.target) {
                            refs.append_reflog(&op.target, &entry)
                                .map_err(|e| e.to_string())?;
                        }
                    }
                    // A symbolic ref the update went through logs it too, and so does `HEAD`
                    // when the branch it names moves.
                    let head = RefName::head();
                    let mut also = Vec::new();
                    if op.named != op.target {
                        also.push(op.named.clone());
                    }
                    if changed
                        && op.target != head
                        && !also.contains(&head)
                        && matches!(refs.head_branch(), Ok(Some(ref b)) if *b == op.target)
                    {
                        also.push(head);
                    }
                    for name in also {
                        if self.repository.should_log_ref(&name) {
                            refs.append_reflog(&name, &entry)
                                .map_err(|e| e.to_string())?;
                        }
                    }
                    if changed {
                        lock.commit().map_err(|e| e.to_string())?;
                    } else {
                        let _ = lock.release();
                    }
                }
            }
        }
        Ok(())
    }

    /// Prepare and apply `ops` as one unit.
    fn commit(&self, ops: Vec<Op>) -> Result<(), String> {
        let locks = self.prepare(&ops)?;
        self.apply(&ops, locks)
    }

    /// Run the `--stdin` command stream.
    fn run_stdin(&self, nul: bool, mut no_deref: bool) -> Result<i32> {
        #[derive(PartialEq)]
        enum State {
            Open,
            Started,
            Prepared,
            Closed,
        }
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input)?;
        let sep = if nul { 0u8 } else { b'\n' };
        let mut fields = input.split(|b| *b == sep).map(<[u8]>::to_vec).peekable();
        // The split leaves one empty tail after the last terminator.
        let mut pieces: Vec<Vec<u8>> = fields.by_ref().collect();
        if pieces.last().is_some_and(Vec::is_empty) {
            pieces.pop();
        }
        let mut cursor = pieces.into_iter();
        let mut state = State::Open;
        let mut ops: Vec<Op> = Vec::new();
        let mut held: Option<Vec<RefLock>> = None;
        let zero = ObjectId::zero(self.repository.format());

        while let Some(raw) = cursor.next() {
            let line = String::from_utf8_lossy(&raw).into_owned();
            let (command, rest) = match line.split_once(' ') {
                Some((c, r)) => (c, Some(r)),
                None => (line.as_str(), None),
            };
            match (command, rest) {
                ("start", None) => {
                    match state {
                        State::Open => {}
                        State::Closed => bail!("transaction is closed"),
                        _ => bail!("cannot restart ongoing transaction"),
                    }
                    state = State::Started;
                    println!("start: ok");
                }
                ("prepare", None) => {
                    if state == State::Closed {
                        bail!("transaction is closed");
                    }
                    if held.is_none() {
                        held = Some(
                            self.prepare(&ops)
                                .map_err(|e| anyhow::anyhow!("prepare: {e}"))?,
                        );
                    }
                    state = State::Prepared;
                    println!("prepare: ok");
                }
                ("commit", None) => {
                    if state == State::Closed {
                        bail!("transaction is closed");
                    }
                    let locks = match held.take() {
                        Some(locks) => locks,
                        None => self
                            .prepare(&ops)
                            .map_err(|e| anyhow::anyhow!("commit: {e}"))?,
                    };
                    self.apply(&ops, locks)
                        .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
                    ops.clear();
                    state = State::Closed;
                    println!("commit: ok");
                }
                ("abort", None) => {
                    if state == State::Closed {
                        bail!("transaction is closed");
                    }
                    held = None;
                    ops.clear();
                    state = State::Closed;
                    println!("abort: ok");
                }
                ("option", Some(opt)) => match opt {
                    "no-deref" => no_deref = true,
                    _ => bail!("option unknown: {opt}\n"),
                },
                ("update" | "create" | "delete" | "verify", rest) => {
                    if state == State::Closed {
                        bail!("transaction is closed");
                    }
                    let op = self.parse_command(command, rest, nul, &mut cursor, no_deref)?;
                    // A zero or empty new id on `update` deletes, like git.
                    let op = match op.action {
                        Action::Write(new) if new == zero => Op {
                            action: Action::Delete,
                            ..op
                        },
                        _ => op,
                    };
                    ops.push(op);
                }
                (other, _) if other.starts_with("symref-") => {
                    bail!("not implemented: update-ref --stdin {other}");
                }
                _ => bail!("unknown command: {line}\n"),
            }
        }
        if state == State::Open {
            self.commit(ops).map_err(anyhow::Error::msg)?;
        }
        Ok(0)
    }

    /// Parse the arguments of one `update`, `create`, `delete` or `verify` command.
    fn parse_command(
        &self,
        command: &str,
        rest: Option<&str>,
        nul: bool,
        cursor: &mut std::vec::IntoIter<Vec<u8>>,
        no_deref: bool,
    ) -> Result<Op> {
        let zero = ObjectId::zero(self.repository.format());
        let Some(rest) = rest else {
            bail!("unknown command: {command}\n");
        };
        // In line mode the arguments share the line; with `-z` each is its own NUL field.
        let mut args: Vec<String> = Vec::new();
        let name;
        let mut tail_extra = None;
        if nul {
            name = rest.to_string();
            let fields: &[&str] = match command {
                "update" => &["new-oid", "old-oid"],
                "create" => &["new-oid"],
                _ => &["old-oid"],
            };
            for what in fields {
                let Some(field) = cursor.next() else {
                    bail!("{command} {name}: unexpected end of input when reading <{what}>");
                };
                args.push(String::from_utf8_lossy(&field).into_owned());
            }
        } else {
            let mut parts = rest.splitn(2, ' ');
            name = parts.next().unwrap_or_default().to_string();
            let mut remaining = parts.next();
            let want = if command == "update" { 2 } else { 1 };
            while args.len() < want {
                let Some(text) = remaining else { break };
                let (head, tail) = match text.split_once(' ') {
                    Some((h, t)) => (h, Some(t)),
                    None => (text, None),
                };
                args.push(head.to_string());
                remaining = tail;
            }
            if let Some(extra) = remaining {
                tail_extra = Some(extra.to_string());
            }
        }
        let Ok(parsed) = RefName::from_bytes(name.as_bytes()) else {
            bail!("invalid ref format: {name}");
        };
        if let Some(extra) = tail_extra {
            bail!("{command} {name}: extra input:  {extra}\n");
        }
        let oid_of = |what: &str, spec: &str| -> Result<ObjectId> {
            if spec.is_empty() {
                return Ok(zero);
            }
            super::resolve_rev(self.repository, spec)
                .map_err(|_| anyhow::anyhow!("{command} {name}: invalid <{what}>: {spec}"))
        };
        let (action, old) = match command {
            "update" | "create" => {
                let Some(new_spec) = args.first() else {
                    bail!("{command} {name}: missing <new-oid>");
                };
                let new = oid_of("new-oid", new_spec)?;
                let old = match (command, args.get(1)) {
                    ("create", _) => Old::Absent,
                    (_, Some(spec)) if nul && spec.is_empty() => Old::Any,
                    (_, Some(spec)) => old_from(oid_of("old-oid", spec)?, zero),
                    (_, None) => Old::Any,
                };
                (Action::Write(new), old)
            }
            _ => {
                let old = match args.first() {
                    Some(spec) if nul && spec.is_empty() => Old::Any,
                    Some(spec) => old_from(oid_of("old-oid", spec)?, zero),
                    None => Old::Any,
                };
                let action = if command == "verify" {
                    Action::Verify
                } else {
                    Action::Delete
                };
                (action, old)
            }
        };
        Ok(self.plan(&parsed, old, action, no_deref))
    }
}
