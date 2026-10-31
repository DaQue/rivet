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

    /// Apply the updates that hold and report the others as `rejected`.
    #[arg(
        short = '0',
        long = "batch-updates",
        overrides_with = "no_batch_updates"
    )]
    pub batch_updates: bool,

    /// Do not batch the updates.
    #[arg(long = "no-batch-updates", overrides_with = "batch_updates")]
    pub no_batch_updates: bool,

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
#[derive(Clone, Debug, PartialEq, Eq)]
enum Old {
    /// No expectation.
    Any,
    /// The ref must not exist.
    Absent,
    /// The ref must hold this id.
    Is(ObjectId),
    /// The ref must be a symbolic ref to this name.
    Symref(RefName),
}

/// What a command does once the expectation holds.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Write(ObjectId),
    /// Make the ref a symbolic ref to this name.
    Symref(RefName),
    Delete,
    Verify,
}

/// Why an expectation did not hold: git's `cannot lock ref` text, and the reason `--batch-updates`
/// prints on a `rejected` line.
struct Reject {
    error: String,
    reason: &'static str,
}

/// One queued command: the ref it was named with, the ref it lands on, and what to do there.
#[derive(Debug)]
struct Op {
    /// The ref as the command named it; it differs from `target` when a symbolic ref was followed.
    named: RefName,
    target: RefName,
    old: Old,
    action: Action,
    /// Added by git's own splitting of an update through a symbolic ref, not named by the input.
    extra: bool,
}

/// Handler for `rivet update-ref`.
///
/// # Exit codes
///
/// `0`; `1` for a failed `-d`; `128` for a fatal error; `129` for bad arguments (usage on stderr).
///
/// # Caveats
///
/// * `--stdin` accepts `update`, `create`, `delete`, `verify`, the four `symref-*` commands,
///   `option no-deref` (for the next command only) and `start`/`prepare`/`commit`/`abort`.
/// * `-0`/`--batch-updates` applies the updates whose expectations hold and prints the others as
///   `rejected <ref> <new> <old> <reason>`; a name conflict is still fatal, not a rejection.
/// * A `delete` removes the ref while it still holds the ref's lock.
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
        batch: args.batch_updates && !args.no_batch_updates,
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
    batch: bool,
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
            extra: false,
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

    /// Lock every ref and check every expectation; the locks come back to the caller, `None` for an
    /// update `--batch-updates` rejected (reported here, so it is skipped when applying).
    fn prepare(&self, ops: &[Op]) -> Result<Vec<Option<RefLock>>, String> {
        let mut seen = HashSet::new();
        let mut named = HashSet::new();
        for op in ops {
            // Git queues an update under the name it was given, before any symbolic ref is followed.
            if !op.extra && !named.insert(op.named.clone()) {
                return Err(format!(
                    "multiple updates for ref '{}' not allowed",
                    op.named
                ));
            }
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
            match self.check(op) {
                Ok(()) => locks.push(Some(lock)),
                Err(reject) if self.batch => {
                    eprintln!("error: {}", reject.error);
                    println!(
                        "rejected {} {} {} {}",
                        op.target,
                        self.new_field(op),
                        self.old_field(op),
                        reject.reason
                    );
                    locks.push(None);
                }
                Err(reject) => return Err(reject.error),
            }
        }
        Ok(locks)
    }

    /// The `<new>` a `rejected` line shows for `op`.
    fn new_field(&self, op: &Op) -> String {
        match &op.action {
            Action::Write(new) => new.to_string(),
            Action::Verify => "(null)".to_string(),
            Action::Symref(_) | Action::Delete => {
                ObjectId::zero(self.repository.format()).to_string()
            }
        }
    }

    /// The `<old>` a `rejected` line shows for `op`.
    fn old_field(&self, op: &Op) -> String {
        match &op.old {
            Old::Any => "(null)".to_string(),
            Old::Absent | Old::Symref(_) => ObjectId::zero(self.repository.format()).to_string(),
            Old::Is(old) => old.to_string(),
        }
    }

    /// Whether the ref `op` lands on holds what the command expects of it.
    fn check(&self, op: &Op) -> Result<(), Reject> {
        let refs = self.repository.refs();
        let raw = refs.try_read(&op.target).ok().flatten();
        let current = raw.as_ref().and_then(|_| refs.resolve_oid(&op.target).ok());
        let shown = if matches!(op.action, Action::Symref(_)) {
            &op.named
        } else {
            &op.target
        };
        let cannot = |why: String| format!("cannot lock ref '{shown}': {why}");
        let missing = || Reject {
            error: cannot(format!("unable to resolve reference '{}'", op.target)),
            reason: "reference does not exist",
        };
        match &op.old {
            Old::Any => {}
            Old::Absent => {
                if current.is_some() {
                    return Err(Reject {
                        error: cannot("reference already exists".into()),
                        reason: "reference already exists",
                    });
                }
                if raw.is_some() && matches!(op.action, Action::Symref(_)) {
                    return Err(Reject {
                        error: cannot("dangling symref already exists".into()),
                        reason: "reference already exists",
                    });
                }
            }
            Old::Is(want) => match current {
                None => return Err(missing()),
                Some(found) if found != *want => {
                    return Err(Reject {
                        error: cannot(format!("is at {found} but expected {want}")),
                        reason: "incorrect old value provided",
                    });
                }
                Some(_) => {}
            },
            Old::Symref(want) => match raw {
                None => return Err(missing()),
                Some(RefTarget::Direct(_)) => {
                    return Err(Reject {
                        error: cannot(format!(
                            "expected symref with target '{want}': but is a regular ref"
                        )),
                        reason: "expected symref but found regular ref",
                    });
                }
                Some(RefTarget::Symbolic(found)) if found != *want => {
                    return Err(Reject {
                        error: format!(
                            "verifying symref target: '{}': is at {found} but expected {want}",
                            op.target
                        ),
                        reason: "incorrect old value provided",
                    });
                }
                Some(RefTarget::Symbolic(_)) => {}
            },
        }
        Ok(())
    }

    /// Write what the prepared transaction decided.
    fn apply(&self, ops: &[Op], locks: Vec<Option<RefLock>>) -> Result<(), String> {
        let refs = self.repository.refs();
        let format = self.repository.format();
        let zero = ObjectId::zero(format);
        let who = self
            .repository
            .committer_signature()
            .map_err(|e| e.to_string())?;
        for (op, lock) in ops.iter().zip(locks) {
            let Some(mut lock) = lock else { continue };
            let current = refs.resolve_oid(&op.target).ok();
            match op.action.clone() {
                Action::Verify => {
                    let _ = lock.release();
                }
                Action::Delete => {
                    if current.is_some() || refs.exists(&op.target) {
                        refs.delete_holding(&op.target, &lock)
                            .map_err(|e| e.to_string())?;
                    }
                    let _ = lock.release();
                    // The lock file kept the ref's directory from being pruned as empty.
                    let _ = rivet_core::refs::loose::delete(self.home(&op.target), &op.target);
                }
                Action::Symref(target) => {
                    std::fs::write(
                        lock.path(),
                        RefTarget::Symbolic(target.clone()).to_bytes(format),
                    )
                    .map_err(|e| e.to_string())?;
                    // A symbolic ref to nothing yet has no value to log.
                    if let Ok(new) = refs.resolve_oid(&target)
                        && (self.force_log || self.repository.should_log_ref(&op.target))
                    {
                        let entry = ReflogEntry::new(
                            current.unwrap_or(zero),
                            new,
                            who.clone(),
                            BString::from(self.reason.as_bytes()),
                        );
                        refs.append_reflog(&op.target, &entry)
                            .map_err(|e| e.to_string())?;
                    }
                    lock.commit().map_err(|e| e.to_string())?;
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
    fn run_stdin(&self, nul: bool, no_deref: bool) -> Result<i32> {
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
        let mut next_no_deref = false;
        let mut ops: Vec<Op> = Vec::new();
        let mut held: Option<Vec<Option<RefLock>>> = None;
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
                    // A batch is checked when it commits: what it rejects is reported then.
                    if held.is_none() && !self.batch {
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
                    "no-deref" => next_no_deref = true,
                    _ => bail!("option unknown: {opt}\n"),
                },
                (
                    "update" | "create" | "delete" | "verify" | "symref-update" | "symref-create"
                    | "symref-delete" | "symref-verify",
                    rest,
                ) => {
                    if state == State::Closed {
                        bail!("transaction is closed");
                    }
                    // `option no-deref` is for the next command naming a ref, and no further.
                    let deref_off = no_deref || std::mem::take(&mut next_no_deref);
                    let mut parsed = if command.starts_with("symref-") {
                        self.parse_symref(command, rest, nul, &mut cursor, deref_off)?
                    } else {
                        vec![self.parse_command(command, rest, nul, &mut cursor, deref_off)?]
                    };
                    let op = parsed.remove(0);
                    // A zero or empty new id on `update` deletes, like git.
                    let op = match op.action {
                        Action::Write(new) if new == zero => Op {
                            action: Action::Delete,
                            ..op
                        },
                        _ => op,
                    };
                    ops.push(op);
                    ops.append(&mut parsed);
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

    /// Parse the arguments of one `symref-update`, `symref-create`, `symref-delete` or
    /// `symref-verify` command.
    fn parse_symref(
        &self,
        command: &str,
        rest: Option<&str>,
        nul: bool,
        cursor: &mut std::vec::IntoIter<Vec<u8>>,
        no_deref: bool,
    ) -> Result<Vec<Op>> {
        let zero = ObjectId::zero(self.repository.format());
        let Some(rest) = rest else {
            bail!("unknown command: {command}\n");
        };
        // Line mode: space separated words on the line; `-z`: the ref after the space, then one
        // NUL-terminated field for each further argument.
        let mut words: Vec<String> = Vec::new();
        let name;
        let mut extra = None;
        let take_field = |cursor: &mut std::vec::IntoIter<Vec<u8>>| {
            cursor
                .next()
                .map(|field| String::from_utf8_lossy(&field).into_owned())
        };
        // The old value of `symref-update`: `ref <target>`, `oid <id>`, or nothing.
        let mut old_kind = None;
        if nul {
            name = rest.to_string();
            if name.is_empty() {
                bail!("{command}: missing <ref>");
            }
            match command {
                "symref-update" | "symref-create" => {
                    words.push(take_field(cursor).unwrap_or_default());
                    if command == "symref-update" && !words[0].is_empty() {
                        let kind = take_field(cursor).unwrap_or_default();
                        if !kind.is_empty() {
                            old_kind = Some((kind, take_field(cursor).unwrap_or_default()));
                        }
                    }
                }
                _ => words.push(take_field(cursor).unwrap_or_default()),
            }
        } else {
            let mut parts = rest.splitn(2, ' ');
            name = parts.next().unwrap_or_default().to_string();
            if name.is_empty() {
                bail!("{command}: missing <ref>");
            }
            let mut remaining = parts.next();
            let next_word = |remaining: &mut Option<&str>| {
                let text = remaining.take()?;
                let (head, tail) = match text.split_once(' ') {
                    Some((h, t)) => (h, Some(t)),
                    None => (text, None),
                };
                *remaining = tail;
                Some(head.to_string())
            };
            words.push(next_word(&mut remaining).unwrap_or_default());
            if command == "symref-update" && !words[0].is_empty() && remaining.is_some() {
                let kind = next_word(&mut remaining).unwrap_or_default();
                if !matches!(kind.as_str(), "ref" | "oid") {
                    bail!("{command} {name}: invalid arg '{kind}' for old value");
                }
                let value = next_word(&mut remaining).unwrap_or_default();
                old_kind = Some((kind, value));
            }
            if let Some(text) = remaining {
                extra = Some(text.to_string());
            }
        }
        if (command == "symref-update" || command == "symref-create") && words[0].is_empty() {
            bail!("{command} {name}: missing <new-target>");
        }
        if let Some(text) = extra {
            bail!("{command} {name}: extra input:  {text}\n");
        }
        let Ok(parsed) = RefName::from_bytes(name.as_bytes()) else {
            bail!("invalid ref format: {name}");
        };
        let target_of = |text: &str| {
            RefName::from_bytes(text.as_bytes())
                .map_err(|_| anyhow::anyhow!("invalid ref format: {text}"))
        };
        let (action, old) = match command {
            "symref-update" | "symref-create" => {
                let new = target_of(&words[0])?;
                let old = match old_kind {
                    _ if command == "symref-create" => Old::Absent,
                    None => Old::Any,
                    Some((kind, value)) => {
                        if value.is_empty() {
                            bail!("{command} {name}: expected old value");
                        }
                        if kind == "ref" {
                            Old::Symref(target_of(&value)?)
                        } else if kind == "oid" {
                            let id = super::resolve_rev(self.repository, &value).map_err(|_| {
                                anyhow::anyhow!("{command} {name}: invalid oid: {value}")
                            })?;
                            old_from(id, zero)
                        } else {
                            bail!("{command} {name}: invalid arg '{kind}' for old value");
                        }
                    }
                };
                (Action::Symref(new), old)
            }
            _ => {
                if !no_deref {
                    bail!("{command}: cannot operate with deref mode");
                }
                let old = if words[0].is_empty() {
                    if command == "symref-verify" {
                        Old::Absent
                    } else {
                        Old::Any
                    }
                } else {
                    Old::Symref(target_of(&words[0])?)
                };
                let action = if command == "symref-verify" {
                    Action::Verify
                } else {
                    Action::Delete
                };
                (action, old)
            }
        };
        let op = self.plan(&parsed, old.clone(), action.clone(), no_deref);
        // Through a symbolic ref git points the named ref at the target as well as the ref at the
        // end of the chain.
        if matches!(action, Action::Symref(_)) && op.named != op.target {
            let named = Op {
                target: op.named.clone(),
                named: op.named.clone(),
                old,
                action,
                extra: true,
            };
            return Ok(vec![op, named]);
        }
        Ok(vec![op])
    }
}
