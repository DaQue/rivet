//! Phase 11 — the SSH transport: spawn the system `ssh` and speak Git's protocol down its pipes.
//!
//! There is no SSH implementation here, and that is the design rather than a shortcut (plan §4
//! Phase 11). Git itself does the same thing: it runs
//!
//! ```text
//! ssh [-p port] [user@]host "git-upload-pack '/path/to/repo.git'"
//! ```
//!
//! and treats the child's stdout and stdin as the protocol stream. Everything the user has
//! configured — `~/.ssh/config`, agent forwarding, `ProxyJump`, hardware keys, an enterprise's
//! `Match` rules — therefore works, with no code on Rivet's side. An SSH library would have to
//! re-implement all of it and would still differ in the places that matter.
//!
//! # Why the remote command is quoted
//!
//! The command string is concatenated into a shell command line *on the remote host* and executed by
//! the remote user's login shell. That makes it an injection surface: a repository path containing a
//! space, a `$`, a backtick or a single quote is a path that can run a command. The rule Git applies
//! and Rivet must apply is:
//!
//! * **The path is wrapped in single quotes**, which makes every byte literal in every POSIX shell
//!   except `'` itself — and a `'` inside the path is then escaped as `'\''`, which is how a shell
//!   spells one inside a single-quoted string.
//! * **A path that begins with `~` or contains a `~` in the first component is left unquoted**,
//!   because the remote shell must expand it and quoting would defeat that. This is why `git clone
//!   host:~/repo` works and why the exception exists.
//! * **The command is `git-upload-pack` or `git-receive-pack`, never a bare `git`**, because a
//!   server's `git-shell` only permits those two entry points (`git-shell -c`) and a bare `git`
//!   is refused by every well-configured host.
//!
//! # Caveats that bite
//!
//! * **The child's stderr must be separated from the protocol stream.** A banner, a warning about
//!   `known_hosts`, the host's message of the day and an authentication prompt from the `ssh` client
//!   all arrive on stderr; Git passes them through to the user's terminal and keeps the protocol on
//!   stdout. A transport that read them together would feed the banner into the pkt-line parser and
//!   report a framing error at whatever the banner's first line happened to be.
//! * **The remote command must be one argument.** `ssh host cmd arg` sends `cmd` and `arg` as
//!   separate arguments to the remote command; `ssh host "cmd arg"` sends one string for the remote
//!   shell to split. Git wants the latter, and an implementation that passed the pieces separately
//!   produces a `git-upload-pack` that receives `arg` as its first argument rather than the path.
//! * **A missing or non-executable `ssh` is a transport error at spawn, not a panic.** The process
//!   spawn is where a machine with no SSH client fails, and the message must say "ssh" rather than
//!   "No such file or directory" against a bare path.
//! * **`disconnect` must close stdin before waiting.** A `git-upload-pack` that has read a flush
//!   packet and is waiting for more input will not exit until its stdin closes, so a `wait` that ran
//!   before the close blocks forever. This is the resource leak the transport seam's caveat warns
//!   about, and it presents as a hang rather than as an error.
//! * **The exit status is checked, but only after the stream is drained.** A server that failed
//!   mid-exchange writes the reason to stderr and exits non-zero; reporting "exit status 128" without
//!   the stderr text loses the only useful part of the message. The two are read in that order.
//!
//! # The scp-like form
//!
//! `git@example.com:org/repo.git` and `ssh://git@example.com/org/repo.git` reach the same host with
//! **different paths**: the first is relative to the login directory, the second is absolute. The
//! transport passes whichever spelling it got to the remote command unchanged, because normalising
//! them is how `host:repo` starts addressing `/repo` instead of `~/repo` — the difference is not
//! cosmetic and there is no way to recover it later.

use std::process::Child;

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::ObjectFormat;

use crate::remote::transport::{CapabilitySet, Service};
use crate::remote::url::RemoteUrl;
use crate::remote::{
    AdvertisedRef, ProtocolVersion, PushStatus, RefAdvertisement, RefUpdate, Transport,
};

/// Phase 11 — the program Rivet runs to reach an SSH remote.
///
/// # Caveats
///
/// **Resolved through `PATH`**, not hard-coded to `/usr/bin/ssh`, because a user with a wrapper
/// script — a proxy, an agent shim, a `gpg`-based key holder — expects it to be used. Git honours
/// `GIT_SSH` and `GIT_SSH_COMMAND` here and Rivet must too, which is what
/// [`ssh_command_for`] is for; this constant is only the fallback.
pub const SSH_PROGRAM: &str = "ssh";

/// Phase 11 — the environment variable naming a drop-in replacement for `ssh`.
///
/// # Caveats
///
/// * **It is a path to a program, and it replaces `ssh` entirely.** Git runs it as
///   `$GIT_SSH host command` with no port handling: the `-p` argument is *not* added, and a
///   replacement that needs one must read `GIT_SSH_COMMAND` or the `ssh://` URL's port from
///   somewhere else. Rivet matches this, including its awkwardness, because a wrapper written for Git
///   must keep working.
/// * **An empty value means unset**, like every `GIT_*` variable.
/// * **The value may be a path with spaces.** It is a single program path, not a command line;
///   [`GIT_SSH_COMMAND_ENV`] is the variable for that, and using it as one here would break every
///   path containing a space.
pub const GIT_SSH_ENV: &str = "GIT_SSH";

/// Phase 11 — the environment variable holding a full shell command line to run instead of `ssh`.
///
/// # Caveats
///
/// * **It is a shell command line, parsed by the shell**, so quoting is the user's problem and a
///   value with unbalanced quotes is their bug to see. Git passes it to the shell; Rivet does too,
///   because a user who has written
///   `GIT_SSH_COMMAND='ssh -i ~/key -o IdentitiesOnly=yes'` expects those options to apply.
/// * **It takes precedence over [`GIT_SSH_ENV`]**, matching Git, which checks this one first.
/// * **The host and the remote command are appended as arguments**, not concatenated into the
///   command line — so `ssh -o Foo]='a b'` survives, and a hostname containing a shell
///   metacharacter is still one argument. This is the one place the two variables differ in how the
///   result is assembled.
pub const GIT_SSH_COMMAND_ENV: &str = "GIT_SSH_COMMAND";

/// Phase 11 — the environment variable that overrides the port an `ssh://` URL uses.
///
/// # Caveats
///
/// **Only affects a URL with no explicit port.** A port in the URL wins, because that is what the
/// user wrote in this repository's config; the variable is the machine-wide default. Git's
/// behaviour exactly, and worth stating because the opposite would be equally defensible.
pub const GIT_SSH_PORT_ENV: &str = "GIT_SSH_PORT";

/// Phase 11 — the SSH transport.
///
/// # Caveats
///
/// * **It owns a child process for the life of one exchange.** The child is `git-upload-pack` or
///   `git-receive-pack` running on the remote host, started by `connect` and reaped by
///   `disconnect`. Dropping the transport without calling `disconnect` leaves the child's stdin open
///   and the child alive — see the module caveat on the ordering of close and wait — so a caller
///   must not rely on `Drop`.
/// * **The protocol generation is v0 for both services**, and a v2 request is not offered over SSH
///   by this transport. Git does support v2 over SSH (`ssh host git-upload-pack 'path'` answers a v2
///   request the same way an HTTP server does), and Rivet's omission is deliberate: the v0 path is
///   the one that must work first, and adding v2 later changes only
///   [`SshTransport::connect`]'s request, not any signature here.
/// * **The remote path is bytes and may not be UTF-8.** It is passed through
///   [`Command::arg`](std::process::Command::arg), which on Unix takes an `OsStr`, so a non-UTF-8 path survives to the `ssh`
///   process — which is what makes a repository named in a non-UTF-8 encoding reachable at all.
/// * **`Debug` must not print the command line**, because for a `GIT_SSH_COMMAND` value it may
///   contain a key path or an inline identity. The credentials this transport has are the user's own
///   in the URL, and the same rule as everywhere else applies: [`RemoteUrl::redacted`].
#[derive(Debug)]
pub struct SshTransport {
    /// The remote being talked to. Printed only through [`RemoteUrl::redacted`].
    url: RemoteUrl,
    /// The spawned child, present between `connect` and `disconnect`.
    child: Option<Child>,
    /// The advertisement read from the child's first output, kept for [`Transport::capabilities`].
    advertisement: Option<RefAdvertisement>,
    /// The command string sent to the remote shell, kept so that `disconnect` and the error paths do
    /// not have to rebuild it — and so that a diagnostic can show it **after** being redacted of any
    /// inline credential a `GIT_SSH_COMMAND` supplied.
    remote_command: BString,
}

impl SshTransport {
    /// Phase 11 — build a transport for an SSH remote URL.
    ///
    /// # Caveats
    ///
    /// * **No process is spawned here.** Construction only validates the URL and works out the
    ///   command line, so that a caller can build a transport and learn about a bad URL at a point
    ///   where the error is still a configuration error rather than a spawn failure. The spawn is in
    ///   [`SshTransport::spawn`].
    /// * **The scheme must be [`RemoteScheme::Ssh`](crate::remote::RemoteScheme::Ssh)**, which covers both `ssh://` and the scp-like
    ///   form; a `git://` or `https://` URL is [`crate::error::Error::InvalidRemoteUrl`] rather than
    ///   being attempted over SSH.
    /// * **A missing host is refused**, and so is an empty path: `ssh://host` names no repository and
    ///   a `git-upload-pack` with an empty argument fails on the remote with a confusing message
    ///   about the current directory.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidRemoteUrl`] for a URL this transport cannot serve.
    pub fn new(url: RemoteUrl) -> Result<Self> {
        todo!()
    }

    /// Phase 11 — the remote command that will be run, with any inline credential redacted.
    ///
    /// # Caveats
    ///
    /// * **The path is shell-quoted for the remote host**; see the module caveat. The returned string
    ///   is what `ssh` receives as its single command argument, not what the remote shell ends up
    ///   executing after expansion — the two differ for a `~` path, and that is intended.
    /// * **The program name is `git-upload-pack` or `git-receive-pack`**, never a bare `git`, so that
    ///   a remote configured with `git-shell` (which is every well-locked-down host) accepts it.
    /// * **A `GIT_SSH_COMMAND` value is not part of this string**, because it is the `ssh`
    ///   invocation rather than the remote command; a caller assembling a diagnostic needs both, and
    ///   [`SshTransport::ssh_command`] returns the other half.
    pub fn build_remote_command(url: &RemoteUrl, service: Service) -> BString {
        let path = quote_remote_path(url.path.as_ref());
        remote_command(service, path.as_ref())
    }

    /// Phase 11 — the `ssh` invocation, as a program and its arguments.
    ///
    /// # Caveats
    ///
    /// * **The return is `(program, args)` rather than a command line**, so that a program path with
    ///   a space in it survives — which is the difference between this and
    ///   [`GIT_SSH_COMMAND_ENV`], whose value is a shell line and is handled separately.
    /// * **`-p` is added only when the URL named a port explicitly**, and `GIT_SSH_PORT_ENV` supplies
    ///   the default otherwise. Adding `-p 22` unconditionally is harmless on most hosts and changes
    ///   nothing observable, but it *is* observable in a test that records the arguments, and more
    ///   importantly it overrides a `Port` in the user's `~/.ssh/config`, which is a real
    ///   misconfiguration.
    /// * **`-o SendEnv=GIT_PROTOCOL` is what makes v2 work over SSH** and is sent even though this
    ///   transport currently stays on v0, because a peer that receives it and does not use it is
    ///   unaffected. A future v2 path depends on it and would otherwise have to remember.
    /// * **`BatchMode=yes` is not set.** Git does not set it, and setting it would break the
    ///   interactive passphrase prompt that a user with a passphrase-protected key depends on. A
    ///   caller that needs non-interactive behaviour sets `GIT_SSH_COMMAND` with it, which is exactly
    ///   what CI does.
    pub fn ssh_command(&self, port: Option<u16>) -> (BString, Vec<BString>) {
        todo!()
    }

    /// Phase 11 — spawn the remote service.
    ///
    /// # Caveats
    ///
    /// * **stdin and stdout are piped and stderr is inherited.** The protocol runs over stdin/stdout;
    ///   stderr must reach the user's terminal directly so that a host key prompt, a banner and the
    ///   `ssh` client's own errors are visible and never mistaken for protocol bytes. An
    ///   implementation that piped stderr would have to decide what to do with an interactive prompt,
    ///   and there is no right answer that is not "let it through".
    /// * **The spawn may block on an interactive prompt** — a first connection to an unknown host, a
    ///   passphrase — which is correct behaviour for a terminal and a hang in a script. That is why
    ///   the prompt goes to the real stderr rather than into a buffer nobody reads.
    /// * **Calling this twice without a `disconnect` leaks the first child.** The transport holds
    ///   one; a second spawn replaces the handle without reaping it, and the check for that is the
    ///   state, not a count.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Transport`] if the program cannot be spawned — a missing `ssh`, a
    /// non-executable `GIT_SSH`, a permission failure — with the program's name in the message, and
    /// [`crate::error::Error::Protocol`] if the child exits before writing an advertisement, which is
    /// how a rejected connection, a wrong user and a nonexistent path all present.
    pub fn spawn(&mut self, service: Service) -> Result<()> {
        todo!()
    }

    /// Phase 11 — read the advertisement the remote service wrote on startup.
    ///
    /// # Caveats
    ///
    /// * **The first line of a v0 advertisement is a service line** (`# service=…` followed by a
    ///   flush) only over HTTP; over SSH `git-upload-pack` writes the ref list immediately, with no
    ///   service line. A reader that waited for one over SSH blocks until the server's timeout.
    /// * **The read must consume the whole advertisement including its trailing flush**, or the
    ///   subsequent request is written while the child is still writing refs — which a small
    ///   repository hides and a large one does not.
    /// * **A child that writes nothing and exits is the common failure**, and its reason is on stderr
    ///   where this function cannot see it. The error therefore names the command and says to check
    ///   the remote path, because "the remote closed the connection" with no context is the least
    ///   actionable message a clone can produce.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`] for an advertisement that does not parse or one that ends
    /// without a flush, and [`crate::error::Error::Transport`] for a pipe that fails.
    pub fn read_advertisement(&mut self, format: ObjectFormat) -> Result<RefAdvertisement> {
        todo!()
    }

    /// Phase 11 — the child's stdout, for a caller that wants to read the stream itself.
    ///
    /// # Caveats
    ///
    /// * **Taking it once is the only option** — it is a pipe, and there is no second handle. A
    ///   caller that takes it and then calls a method that reads (such as
    ///   [`SshTransport::read_advertisement`]) gets the transport's error about a missing stdin
    ///   rather than a double-take.
    /// * **The handle is unbuffered**, so a caller wrapping it in a `BufReader` gets to choose the
    ///   size and a caller reading a pack in bulk is not paying for a per-byte syscall.
    /// * **Reading it to end-of-file consumes the pack**, which for [`Transport::fetch`] is what the
    ///   demultiplexer does. A caller that does both the demux and its own read will see one of them
    ///   get nothing.
    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut().and_then(|c| c.stdout.take())
    }

    /// Phase 11 — the child's stdin, for a caller writing its own request.
    ///
    /// # Caveats
    ///
    /// * **The handle must be flushed and closed** before the remote will act on a complete request.
    ///   A `git-upload-pack` reads until it sees a flush packet, which is why a caller writing a
    ///   pkt-line list does not need to close — but a caller writing something that is not
    ///   self-terminating does, and the resulting hang is the child waiting for input.
    /// * **Dropping the handle closes the pipe**, which is how the transport signals end-of-input
    ///   during `disconnect`. A caller that drops it early ends the exchange.
    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.child.as_mut().and_then(|c| c.stdin.take())
    }

    /// Phase 11 — whether a child is currently running.
    ///
    /// # Caveats
    ///
    /// True between a successful [`SshTransport::spawn`] and the `disconnect` that reaps it, and
    /// false before. It says nothing about whether the child is *alive*: a `git-upload-pack` that
    /// exited on its own leaves the handle in place until it is waited on, which is exactly the state
    /// [`SshTransport::wait`] exists to resolve.
    pub fn is_connected(&self) -> bool {
        self.child.is_some()
    }

    /// Phase 11 — close the child's stdin and wait for it to exit.
    ///
    /// # Caveats
    ///
    /// * **Ordering is the whole of it**: close stdin, then wait. See the module caveat — a wait
    ///   before the close blocks until the child's own timeout, and the child is waiting for input
    ///   that only the close can say is finished.
    /// * **A non-zero exit is an error only if the exchange did not already report one.** A push
    ///   whose refs were all rejected still exits zero; a `git-upload-pack` killed by the remote's
    ///   out-of-memory killer exits non-zero with nothing on stderr. The caller that has already
    ///   parsed a complete response should report the exit status as a footnote rather than as the
    ///   failure, and this function returns the status rather than deciding.
    /// * **The stderr text is not collected here**, because it went to the terminal by design. A
    ///   caller that wants it captured sets `GIT_SSH_COMMAND` with a redirection, which is the same
    ///   escape hatch Git offers.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Transport`] if the child cannot be waited on — a failure of the
    /// operating system rather than of the exchange — and no error for a non-zero exit status, which
    /// is returned for the caller to judge.
    pub fn wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        todo!()
    }
}

/// Phase 11 — the command line to run, resolving `GIT_SSH_COMMAND` and `GIT_SSH`.
///
/// # Caveats
///
/// * **The precedence is [`GIT_SSH_COMMAND_ENV`], then [`GIT_SSH_ENV`], then [`SSH_PROGRAM`]**, as
///   Git's is. The order matters: a user with both set — a wrapper script and a full command line —
///   means the more specific one to win, and the command line is more specific than the path.
/// * **A `GIT_SSH_COMMAND` value is split by the shell, not by this function.** The return
///   distinguishes the two cases, because a program path with a space must not be split while a
///   command line with quotes must be. Returning a single `Vec<BString>` for both would silently
///   break one of them.
/// * **The environment is read at call time**, not cached, so a test can set the variable, call, and
///   unset it — which is how the differential suite drives this without spawning anything.
pub fn ssh_command_for(url: &RemoteUrl, port: Option<u16>) -> (SshInvocation, Vec<BString>) {
    todo!()
}

/// Phase 11 — how a resolved `ssh` invocation should be turned into a process.
///
/// # Caveats
///
/// * **[`SshInvocation::Program`] is executed directly**, with each argument passed separately, so a
///   path containing a space is one program. This is the `GIT_SSH` and default case.
/// * **[`SshInvocation::ShellLine`] is handed to `sh -c`**, so the user's own quoting and options are
///   interpreted. This is the `GIT_SSH_COMMAND` case, and it is a deliberate second code path rather
///   than a special case of the first, because the two cannot be unified without breaking one.
/// * **Neither form is ever built from a URL's user or host by string concatenation.** Both are
///   appended as arguments, so a hostname is never parsed as shell syntax — the one place a remote
///   URL reaches a command line is [`SshTransport::build_remote_command`]'s *remote* command, which
///   is a single quoted argument for the remote shell and not for the local one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshInvocation {
    /// Run this program with these arguments.
    Program(BString),
    /// Run this line through the shell.
    ShellLine(BString),
}

impl Transport for SshTransport {
    /// Phase 11 — spawn the service and read its advertisement.
    ///
    /// # Caveats
    ///
    /// * **This is spawn-then-read, in that order, and both here rather than split.** A caller cannot
    ///   read an advertisement from a child it has not started, and a caller that started one and did
    ///   not read leaves the child blocked writing into a full pipe — which for a repository with
    ///   many refs is immediate, because the advertisement exceeds the pipe buffer.
    /// * **The object-format check is made here**, against `format`, before any id is parsed. See the
    ///   transport seam's caveat; the SSH advertisement is where this matters most, because the
    ///   server's `object-format` arrives as an ordinary capability line and there is no HTTP status
    ///   to hint that anything is wrong.
    /// * **`version` is forced to [`ProtocolVersion::V0`]**, matching what this transport speaks. A
    ///   caller that asked for v2 and got a v0 advertisement is not an error — it is the transport
    ///   telling the truth — and the returned [`RefAdvertisement`] says which it is.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidRemoteUrl`], [`crate::error::Error::Transport`] for a spawn or
    /// pipe failure, [`crate::error::Error::ObjectFormatMismatch`], and
    /// [`crate::error::Error::Protocol`] for an advertisement that does not parse.
    fn connect(&mut self, url: &RemoteUrl, format: ObjectFormat) -> Result<RefAdvertisement> {
        todo!()
    }

    /// Phase 11 — the capabilities from the last [`Transport::connect`].
    fn capabilities(&self) -> &CapabilitySet<'_> {
        todo!()
    }

    /// Phase 11 — negotiate a fetch down the child's pipes and write the pack to `into`.
    ///
    /// # Caveats
    ///
    /// * **The negotiation and the pack share one stream.** This is the difference from HTTP, where
    ///   each round is a request: over SSH the client writes `want`/`have` rounds into the child's
    ///   stdin and reads the answers from its stdout, and the `done` line that ends the negotiation is
    ///   followed by the pack on the *same* stdout. A reader that treated the streams as separable
    ///   would need a second connection that `git-upload-pack` does not offer.
    /// * **The write must be flushed after each round**, or the child blocks waiting for input the
    ///   client has already queued in a buffer. This is the SSH-side version of the `BufWriter`
    ///   caveat in [`crate::remote::PktLine::write_to`].
    /// * **stdout is drained to the flush packet**, per the module caveat on exit status: a return
    ///   before the drain leaves the child alive and `disconnect` blocking.
    /// * **The pack may be thin**, exactly as over HTTP — the transport does not index or fix it, and
    ///   the caller runs [`crate::pack::fix_thin_pack`] before [`crate::pack::index_pack`].
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`] for a malformed frame, an unknown sideband channel or an
    /// `ERR` line from the server; [`crate::error::Error::Transport`] for a pipe failure; and whatever
    /// writing to `into` raises.
    fn fetch(
        &mut self,
        wants: &[crate::hash::ObjectId],
        haves: &[crate::hash::ObjectId],
        into: &mut dyn std::io::Write,
    ) -> Result<()> {
        todo!()
    }

    /// Phase 11 — write the updates and the pack, and read `report-status`.
    ///
    /// # Caveats
    ///
    /// * **`git-receive-pack` reads the update list and the pack from the same stdin**, with the
    ///   pkt-line list's flush marking the end of the commands and the pack following immediately.
    ///   Writing a second flush between them makes the server treat the pack header as a command.
    /// * **The output is sideband-framed when `side-band-64k` was requested**, and the status lines
    ///   are on channel 1 while progress is on channel 2 — the same demultiplexer as a fetch, with a
    ///   different interpretation of what channel 1 means. See [`crate::remote::SidebandKind`].
    /// * **The child must be drained and waited on before the status is trusted.** A server that
    ///   reported `unpack ok` and then died has not finished; over SSH the exit status is the only
    ///   place that shows up.
    /// * **The pack may be empty**, which is the delete-only and already-present cases, and the
    ///   update list is still sent and still answered.
    ///
    /// # Errors
    ///
    /// As [`Transport::push`]: protocol and transport failures only. A rejected ref arrives in the
    /// returned vector.
    fn push(&mut self, updates: &[RefUpdate], pack: &[u8]) -> Result<Vec<PushStatus>> {
        todo!()
    }

    /// Phase 11 — unsupported over this SSH transport.
    ///
    /// # Caveats
    ///
    /// **Always [`crate::error::Error::Protocol`]**, because this transport speaks v0 and there is
    /// no `ls-refs` request in v0. A caller wants the advertisement's refs, which
    /// [`Transport::connect`] has already returned. The error rather than a silent empty result is
    /// deliberate: a caller that asked for `ls-refs` and got an empty list would conclude the remote
    /// has no refs.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`], always.
    fn ls_refs(&mut self, prefixes: &[BString], peel: bool) -> Result<Vec<AdvertisedRef>> {
        todo!()
    }

    /// Phase 11 — close the child's stdin and reap it.
    ///
    /// # Caveats
    ///
    /// * **This is where a leaked `git-upload-pack` is prevented**, so a caller that skips it on the
    ///   error path — the path where it matters most — leaves a zombie per failed fetch. It is
    ///   idempotent so that a cleanup path may call it after a normal one.
    /// * **A non-zero exit after an otherwise complete exchange is reported, not raised.** See
    ///   [`SshTransport::wait`]; a caller that discarded the status would miss the case where the
    ///   server failed after the client had read everything it needed.
    /// * **The stderr of a failed child has already reached the user's terminal**, which is why the
    ///   error here cannot repeat it. A caller building a message should say that the remote's output
    ///   above is the explanation rather than inventing one.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Transport`] if the child cannot be terminated or reaped.
    fn disconnect(&mut self) -> Result<()> {
        todo!()
    }
}

/// Phase 11 — quote a path for the remote shell, as `git-upload-pack`'s argument.
///
/// # Caveats
///
/// * **A path beginning with `~` is left unquoted**, because the remote shell must expand it and
///   quoting defeats that. Only the first component matters: `~/repo` expands, `a/~/b` does not and
///   gets quoted, which is right because the shell would not expand it either.
/// * **Every other path is single-quoted, with `'` escaped as `'\''`.** That is the only escape a
///   POSIX shell's single quotes need, and it is what makes a path containing a space, a `$`, a
///   backtick or a newline literal.
/// * **The result is for the remote shell and is not a local quoting** — it goes inside the single
///   argument [`SshTransport::build_remote_command`] produces. A caller that used this for a local
///   command line would be quoting twice.
/// * **A NUL byte in the path is a caller bug and is not escaped**, because a `Command` argument on
///   Unix cannot contain one at all: the path would already have failed at the spawn with an
///   `InvalidInput` error naming nothing useful. Callers that need a good error check for NUL before
///   getting here.
pub fn quote_remote_path(path: &BStr) -> BString {
    todo!()
}

/// Phase 11 — build the single command string an `ssh` invocation runs on the remote host.
///
/// # Caveats
///
/// * **It is one argument**, passed as one argument to `ssh`, which the remote login shell then
///   splits and expands. See the module caveat.
/// * **The service name is fixed** to `git-upload-pack` or `git-receive-pack`, per
///   [`Service::as_str`], and is never taken from the URL. A URL that could name the remote command
///   would be a way to run arbitrary commands on the remote host from a cloned repository's config.
/// * **The path is quoted by [`quote_remote_path`]** and the result is `git-upload-pack '<path>'`,
///   with the `~` exception. A caller that concatenated the path itself would produce a command
///   injection reachable from a malicious remote-tracking configuration.
pub fn remote_command(service: Service, path: &BStr) -> BString {
    todo!()
}

/// Phase 11 — the arguments to append to the `ssh` program for a URL.
///
/// # Caveats
///
/// * **Order matters**: `-p` before the host, the host before the command. An `ssh` that receives
///   the command first treats it as the host name, and the error is "Could not resolve hostname
///   git-upload-pack", which points at the wrong thing entirely.
/// * **The host may carry the user as `user@host`**, which is the scp-like form's spelling and is
///   what `ssh` itself expects. The two are joined here rather than passed as `-l`, because `-l` and
///   `user@host` differ for a host that contains an `@` — which is not legal, but the joined form is
///   what Git sends and matching it costs nothing.
/// * **An IPv6 literal is re-bracketed**, per [`crate::remote::RemoteUrl::host_port`]; a bare `::1`
///   as an `ssh` host is read as a malformed port.
pub fn ssh_args(url: &RemoteUrl, port: Option<u16>, version: ProtocolVersion) -> Vec<BString> {
    todo!()
}

/// Phase 11 — whether the environment asks for an `ssh` replacement.
///
/// # Caveats
///
/// True if either [`GIT_SSH_COMMAND_ENV`] or [`GIT_SSH_ENV`] is set and non-empty. Exposed so that a
/// diagnostic or a test can ask the question without building a transport, and so that the CLI can
/// tell the user which program it is about to run — which is the first thing to check when an SSH
/// remote fails.
pub fn has_ssh_override() -> bool {
    todo!()
}
