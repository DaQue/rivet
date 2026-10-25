//! Credential helpers: which `credential.helper` entries apply to a URL, the helper protocol, and
//! the prompt of last resort.
//!
//! # Caveats
//!
//! * **Helpers are collected in configuration order**, system then global then repository, from
//!   `credential.helper` and from every `credential.<url>.helper` whose `<url>` matches the remote
//!   (scheme, host, port, a path prefix and a user when the pattern has them). An empty value
//!   resets the list. Git does not put the most specific entry first and neither does this.
//! * **`get` stops at the first helper that supplies both a user name and a password**; `store`
//!   (after a success) and `erase` (after a rejection) go to every helper.
//! * **A helper is run through the shell** with the operation appended: `!cmd` as written, an
//!   absolute path as written, and anything else as `git-credential-<name>` (looked up on `PATH` and
//!   in `GIT_EXEC_PATH` / `/usr/lib/git-core`, so no `git` binary is needed).
//! * **The input is Git's**, including the `capability[]=authtype`/`state` lines it sends with
//!   `get` and the `wwwauth[]` challenge lines (`get` and `erase`, not `store`), so a helper
//!   written against Git sees the same stream.
//! * **Secrets are never printed.** [`Credential`]'s `Debug` omits the password.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use bstr::{BStr, BString, ByteSlice};

use crate::config::Config;
use crate::error::Result;
use crate::remote::RemoteUrl;

/// A credential being looked up, stored or erased.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credential {
    /// `http` or `https`.
    pub protocol: String,
    /// The host, with `:port` when the URL had one.
    pub host: String,
    /// The repository path without its leading slash, only when `credential.useHttpPath` is set.
    pub path: Option<String>,
    /// The user name, once known.
    pub username: Option<BString>,
    /// The password or token, once known.
    pub password: Option<BString>,
    /// The `WWW-Authenticate` header values of the challenge that asked for it.
    pub wwwauth: Vec<String>,
    /// When the password stops being valid (`password_expiry_utc`, seconds since the epoch), if a
    /// helper said so. A helper's `store` and `erase` hear it back.
    pub password_expiry_utc: Option<i64>,
    /// The OAuth refresh token a helper handed out (`oauth_refresh_token`), passed back to
    /// `store` and `erase`.
    pub oauth_refresh_token: Option<BString>,
    /// Set when the challenge answered a POST: Git then asks its helpers without the
    /// `capability[]=authtype`/`state` lines (it retries a POST with plain username/password).
    pub plain_get: bool,
    /// The scheme a helper asked for (`authtype=Bearer`): the request then carries
    /// `Authorization: <authtype> <credential>` instead of Basic user and password.
    pub authtype: Option<String>,
    /// The opaque value that goes after [`Self::authtype`] in the `Authorization` header.
    pub credential: Option<BString>,
    /// A helper said `capability[]=authtype`; `store` and `erase` then hear `authtype` and
    /// `credential` back, preceded by that line.
    pub capa_authtype: bool,
    /// A helper said `capability[]=state`; `store` and `erase` then hear that line, the helper's
    /// `continue` and its `state[]` values back.
    pub capa_state: bool,
    /// The opaque `state[]` values a helper handed out, in the order it gave them.
    pub state: Vec<BString>,
    /// A helper said `continue=1`: it expects another round of authentication.
    pub multistage: bool,
}

/// Undo `%XX` escapes; a `%` not followed by two hex digits stays as it is (as in Git).
fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|b| hex(*b)),
                bytes.get(i + 2).and_then(|b| hex(*b)),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

impl Credential {
    /// Whether nothing more needs to be asked: a user name and password, or an `authtype` with its
    /// `credential`.
    pub fn is_complete(&self) -> bool {
        (self.username.is_some() && self.password.is_some())
            || (self.authtype.is_some() && self.credential.is_some())
    }

    /// Replace the credential by what a helper's `url=` line spells out, as Git's
    /// `credential_from_url` does: everything set so far is dropped, then the protocol, host, path,
    /// user name and password of the URL are taken (percent-decoded, trailing slashes of the path
    /// removed). The path is kept only with `credential.useHttpPath`.
    fn reset_from_url(
        &mut self,
        url: &str,
        use_http_path: bool,
    ) -> std::result::Result<(), String> {
        let unparsable = || format!("credential url cannot be parsed: {url}");
        let Some((protocol, rest)) = url.split_once("://") else {
            eprintln!("warning: url has no scheme: {url}");
            return Err(unparsable());
        };
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let (authority, path) = rest.split_at(authority_end);
        // Git looks for the first '@' and the first ':' of the whole remainder.
        let at = rest.find('@').filter(|at| *at < authority_end);
        let colon = rest.find(':');
        let (username, password, host) = match at {
            None => (None, None, authority),
            Some(at) => match colon.filter(|colon| *colon < at) {
                None => (Some(&rest[..at]), None, &rest[at + 1..authority_end]),
                Some(colon) => (
                    Some(&rest[..colon]),
                    Some(&rest[colon + 1..at]),
                    &rest[at + 1..authority_end],
                ),
            },
        };
        let path = percent_decode(path.trim_start_matches('/'));
        let host = percent_decode(host);
        for (what, value) in [("host", &host), ("path", &path)] {
            if value.contains(&b'\n') {
                eprintln!("warning: url contains a newline in its {what} component: {url}");
                return Err(unparsable());
            }
        }
        let path = path.trim_end_with(|c| c == '/');
        *self = Credential {
            protocol: protocol.to_owned(),
            host: String::from_utf8_lossy(&host).into_owned(),
            path: (use_http_path && !path.is_empty())
                .then(|| String::from_utf8_lossy(path).into_owned()),
            username: username.map(|u| BString::from(percent_decode(u))),
            password: password.map(|p| BString::from(percent_decode(p))),
            plain_get: self.plain_get,
            capa_authtype: self.capa_authtype,
            ..Credential::default()
        };
        Ok(())
    }

    /// The `Authorization` header value a helper's `authtype` and `credential` ask for.
    pub fn authorization(&self) -> Option<String> {
        let (authtype, credential) = self.authtype.as_ref().zip(self.credential.as_ref())?;
        Some(format!("{authtype} {}", credential.to_str_lossy()))
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("protocol", &self.protocol)
            .field("host", &self.host)
            .field("path", &self.path)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .finish()
    }
}

/// The helper operations of the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Get,
    Store,
    Erase,
}

impl Operation {
    fn word(self) -> &'static str {
        match self {
            Operation::Get => "get",
            Operation::Store => "store",
            Operation::Erase => "erase",
        }
    }
}

/// The credential configuration that applies to one remote URL.
#[derive(Clone, Debug, Default)]
pub struct CredentialSettings {
    /// The helpers to consult, in order.
    pub helpers: Vec<String>,
    /// `credential.useHttpPath`: whether the repository path is part of the credential.
    pub use_http_path: bool,
    /// `credential.username` (also per URL): a user name to offer before asking.
    pub username: Option<BString>,
    /// The program to ask with (`GIT_ASKPASS`, `core.askPass`, `SSH_ASKPASS`), if any.
    pub askpass: Option<String>,
}

fn default_port(scheme: &str) -> u16 {
    if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    }
}

/// Whether the `credential.<pattern>.*` section applies to `url`.
fn pattern_matches(pattern: &str, url: &RemoteUrl) -> bool {
    let Some((scheme, rest)) = pattern.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case(url.scheme.as_str()) {
        return false;
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (user, hostport) = match authority.rsplit_once('@') {
        Some((user, hostport)) => (Some(user), hostport),
        None => (None, authority),
    };
    if let Some(user) = user
        && url.user.as_ref().map(|u| u.as_bstr()) != Some(BStr::new(user))
    {
        return false;
    }
    let (host, port) = match hostport.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => {
            (host, port.parse::<u16>().ok())
        }
        _ => (hostport, None),
    };
    let url_host = url
        .host
        .as_ref()
        .map(|h| h.to_str_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let host = host.to_ascii_lowercase();
    let host_ok = match host.strip_prefix("*.") {
        Some(suffix) => url_host.ends_with(&format!(".{suffix}")),
        None => host == url_host,
    };
    if !host_ok {
        return false;
    }
    let scheme_default = default_port(url.scheme.as_str());
    if port.unwrap_or(scheme_default) != url.port.unwrap_or(scheme_default) {
        return false;
    }
    let pattern_path = path.trim_end_matches('/');
    if pattern_path.is_empty() {
        return true;
    }
    let url_path = url.path.to_str_lossy();
    let url_path = if url_path.starts_with('/') {
        url_path.to_string()
    } else {
        format!("/{url_path}")
    };
    let url_path = url_path.trim_end_matches('/');
    url_path == pattern_path || url_path.starts_with(&format!("{pattern_path}/"))
}

impl CredentialSettings {
    /// The configuration for `url`, read from the system and global files and `local`.
    ///
    /// # Errors
    ///
    /// Whatever loading a configuration file raises.
    pub fn for_url(local: &Config, url: &RemoteUrl) -> Result<Self> {
        let mut layers = crate::ops::load_outer_configs()?;
        layers.push(local.clone());
        let mut settings = Self::default();
        // The most specific section wins for the single-valued keys.
        let mut username_rank: Option<(usize, usize)> = None;
        let mut path_rank: Option<(usize, usize)> = None;
        for (index, entry) in layers.iter().flat_map(|c| c.entries()).enumerate() {
            if !entry.section.eq_ignore_ascii_case(b"credential") {
                continue;
            }
            let rank = match &entry.subsection {
                None => 0,
                Some(pattern) => {
                    if !pattern_matches(&pattern.to_str_lossy(), url) {
                        continue;
                    }
                    1 + pattern.len()
                }
            };
            let value = entry.value.clone();
            if entry.key.eq_ignore_ascii_case(b"helper") {
                if value.is_empty() {
                    settings.helpers.clear();
                } else {
                    settings.helpers.push(value.to_str_lossy().into_owned());
                }
            } else if entry.key.eq_ignore_ascii_case(b"usehttppath") {
                if path_rank.is_none_or(|best| (rank, index) >= best) {
                    path_rank = Some((rank, index));
                    settings.use_http_path =
                        crate::config::parse_bool(value.as_bstr()).unwrap_or(false);
                }
            } else if entry.key.eq_ignore_ascii_case(b"username")
                && username_rank.is_none_or(|best| (rank, index) >= best)
            {
                username_rank = Some((rank, index));
                settings.username = Some(value);
            }
        }
        settings.askpass = std::env::var("GIT_ASKPASS")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| {
                layers
                    .iter()
                    .rev()
                    .find_map(|c| c.get(BStr::new("core.askPass")).ok().flatten())
                    .map(|v| v.to_str_lossy().into_owned())
                    .filter(|v| !v.is_empty())
            })
            .or_else(|| std::env::var("SSH_ASKPASS").ok().filter(|v| !v.is_empty()));
        Ok(settings)
    }

    /// The credential to look up for `url`, with the user name the URL or the configuration gives.
    pub fn credential_for(&self, url: &RemoteUrl, wwwauth: Vec<String>) -> Credential {
        let mut host = url
            .host
            .as_ref()
            .map(|h| h.to_str_lossy().into_owned())
            .unwrap_or_default();
        if let Some(port) = url.port {
            host = format!("{host}:{port}");
        }
        Credential {
            protocol: url.scheme.as_str().to_string(),
            host,
            path: self
                .use_http_path
                .then(|| url.path.to_str_lossy().trim_start_matches('/').to_string()),
            username: url.user.clone().or_else(|| self.username.clone()),
            password: url.password.clone(),
            wwwauth,
            ..Credential::default()
        }
    }

    /// Ask the helpers for a user name and password (`get`), stopping at the first that gives both.
    ///
    /// A password whose `password_expiry_utc` has passed is discarded, as Git does, so the next
    /// helper (or the prompt) is asked for it.
    ///
    /// # Errors
    ///
    /// Git's text when a helper that did not complete the credential said `quit=1`.
    pub fn fill(&self, credential: &mut Credential) -> std::result::Result<(), String> {
        for helper in &self.helpers {
            if credential.is_complete() {
                break;
            }
            let Some(output) =
                run_helper(helper, Operation::Get, &input(credential, Operation::Get))
            else {
                continue;
            };
            let mut quit = false;
            let mut replaced = false;
            for line in output.lines() {
                if let Some(value) = line.strip_prefix(b"username=") {
                    credential.username = Some(BString::from(value));
                } else if let Some(value) = line.strip_prefix(b"password=") {
                    credential.password = Some(BString::from(value));
                } else if line == b"capability[]=authtype" {
                    credential.capa_authtype = !credential.plain_get;
                } else if line == b"capability[]=state" {
                    credential.capa_state = !credential.plain_get;
                } else if let Some(value) = line.strip_prefix(b"state[]=") {
                    credential.state.push(BString::from(value));
                } else if let Some(value) = line.strip_prefix(b"continue=") {
                    credential.multistage = value
                        .to_str()
                        .ok()
                        .and_then(|v| v.parse::<i64>().ok())
                        .is_some_and(|v| v != 0);
                } else if let Some(value) = line.strip_prefix(b"authtype=") {
                    credential.authtype = Some(String::from_utf8_lossy(value).into_owned());
                } else if let Some(value) = line.strip_prefix(b"credential=") {
                    credential.credential = Some(BString::from(value));
                } else if let Some(value) = line.strip_prefix(b"oauth_refresh_token=") {
                    credential.oauth_refresh_token = Some(BString::from(value));
                } else if let Some(value) = line.strip_prefix(b"password_expiry_utc=") {
                    credential.password_expiry_utc =
                        value.to_str().ok().and_then(|v| v.parse().ok());
                } else if let Some(value) = line.strip_prefix(b"url=") {
                    credential.reset_from_url(&value.to_str_lossy(), self.use_http_path)?;
                    replaced = true;
                } else if let Some(value) = line.strip_prefix(b"quit=") {
                    quit = crate::config::parse_bool(BStr::new(value)).unwrap_or(false);
                }
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            if credential.password_expiry_utc.is_some_and(|at| at < now) {
                credential.password = None;
                credential.password_expiry_utc = None;
            }
            if credential.is_complete() {
                return Ok(());
            }
            if quit {
                return Err(format!("credential helper '{helper}' told us to quit"));
            }
            // Git's `credential_clear` also forgets the helper list, so none that follow is asked.
            if replaced {
                break;
            }
        }
        Ok(())
    }

    /// Tell every helper the credential worked (`store`).
    pub fn approve(&self, credential: &Credential) {
        for helper in &self.helpers {
            run_helper(
                helper,
                Operation::Store,
                &input(credential, Operation::Store),
            );
        }
    }

    /// Tell every helper the credential was refused (`erase`).
    pub fn reject(&self, credential: &Credential) {
        for helper in &self.helpers {
            run_helper(
                helper,
                Operation::Erase,
                &input(credential, Operation::Erase),
            );
        }
    }

    /// Ask the user for `what` (`Username` or `Password`): through the askpass program when there
    /// is one, otherwise on the terminal.
    ///
    /// # Errors
    ///
    /// Git's reason text (`terminal prompts disabled`, `No such device or address`, …).
    pub fn prompt(
        &self,
        what: &str,
        credential: &Credential,
    ) -> std::result::Result<BString, String> {
        let mut shown = format!("{}://", credential.protocol);
        if what == "Password"
            && let Some(user) = &credential.username
        {
            shown.push_str(&user.to_str_lossy());
            shown.push('@');
        }
        shown.push_str(&credential.host);
        if let Some(path) = &credential.path {
            shown.push('/');
            shown.push_str(path);
        }
        let prompt = format!("{what} for '{shown}': ");
        if let Some(program) = &self.askpass {
            let output = Command::new("sh")
                .arg("-c")
                .arg(format!("{program} \"$0\""))
                .arg(&prompt)
                .stdin(Stdio::inherit())
                .stderr(Stdio::inherit())
                .output()
                .map_err(|e| e.to_string())?;
            let mut answer = output.stdout;
            while answer.last() == Some(&b'\n') {
                answer.pop();
            }
            return Ok(BString::from(answer));
        }
        if std::env::var("GIT_TERMINAL_PROMPT")
            .is_ok_and(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        {
            return Err("terminal prompts disabled".to_string());
        }
        let mut tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|e| {
                let text = e.to_string();
                text.split(" (os error").next().unwrap_or(&text).to_string()
            })?;
        tty.write_all(prompt.as_bytes())
            .map_err(|e| e.to_string())?;
        let secret = what == "Password";
        if secret {
            let _ = Command::new("stty")
                .arg("-echo")
                .stdin(tty.try_clone().map_err(|e| e.to_string())?)
                .status();
        }
        let mut answer = Vec::new();
        let mut byte = [0u8; 1];
        while tty.read(&mut byte).map_err(|e| e.to_string())? == 1 && byte[0] != b'\n' {
            answer.push(byte[0]);
        }
        if secret {
            let _ = Command::new("stty")
                .arg("echo")
                .stdin(tty.try_clone().map_err(|e| e.to_string())?)
                .status();
            let _ = tty.write_all(b"\n");
        }
        Ok(BString::from(answer))
    }
}

/// The text a helper reads on stdin for `operation`.
fn input(credential: &Credential, operation: Operation) -> Vec<u8> {
    let mut out = Vec::new();
    if operation == Operation::Get && !credential.plain_get {
        out.extend_from_slice(b"capability[]=authtype\ncapability[]=state\n");
    }
    let echo_state = operation != Operation::Get && credential.capa_state;
    if operation != Operation::Get && credential.capa_authtype {
        out.extend_from_slice(b"capability[]=authtype\n");
    }
    if echo_state {
        out.extend_from_slice(b"capability[]=state\n");
    }
    if operation != Operation::Get && credential.capa_authtype {
        if let Some(authtype) = &credential.authtype {
            out.extend_from_slice(format!("authtype={authtype}\n").as_bytes());
        }
        if let Some(value) = &credential.credential {
            out.extend_from_slice(b"credential=");
            out.extend_from_slice(value);
            out.push(b'\n');
        }
    }
    out.extend_from_slice(
        format!(
            "protocol={}\nhost={}\n",
            credential.protocol, credential.host
        )
        .as_bytes(),
    );
    if let Some(path) = &credential.path {
        out.extend_from_slice(format!("path={path}\n").as_bytes());
    }
    if let Some(user) = &credential.username {
        out.extend_from_slice(b"username=");
        out.extend_from_slice(user);
        out.push(b'\n');
    }
    if operation != Operation::Get
        && let Some(password) = &credential.password
    {
        out.extend_from_slice(b"password=");
        out.extend_from_slice(password);
        out.push(b'\n');
    }
    if operation != Operation::Get {
        if let Some(token) = &credential.oauth_refresh_token {
            out.extend_from_slice(b"oauth_refresh_token=");
            out.extend_from_slice(token);
            out.push(b'\n');
        }
        if let Some(at) = credential.password_expiry_utc {
            out.extend_from_slice(format!("password_expiry_utc={at}\n").as_bytes());
        }
    }
    if operation != Operation::Store {
        for challenge in &credential.wwwauth {
            out.extend_from_slice(format!("wwwauth[]={challenge}\n").as_bytes());
        }
    }
    if echo_state {
        if credential.multistage {
            out.extend_from_slice(b"continue=1\n");
        }
        for value in &credential.state {
            out.extend_from_slice(b"state[]=");
            out.extend_from_slice(value);
            out.push(b'\n');
        }
    }
    out
}

/// Run one helper for `operation`, feeding it `input`; its stdout, or `None` if it could not run.
fn run_helper(helper: &str, operation: Operation, input: &[u8]) -> Option<Vec<u8>> {
    let command = if let Some(shell) = helper.strip_prefix('!') {
        shell.to_string()
    } else if helper.starts_with('/') {
        helper.to_string()
    } else {
        format!("git-credential-{helper}")
    };
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(exec) = std::env::var_os("GIT_EXEC_PATH") {
        dirs.push(exec.into());
    }
    dirs.push("/usr/lib/git-core".into());
    dirs.push("/usr/libexec/git-core".into());
    let mut all: Vec<std::path::PathBuf> = std::env::split_paths(&path).collect();
    all.extend(dirs);
    if let Ok(joined) = std::env::join_paths(all) {
        path = joined;
    }
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("{command} {}", operation.word()))
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    let mut output = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_end(&mut output);
    }
    let _ = child.wait();
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> RemoteUrl {
        RemoteUrl::parse(BStr::new(text)).unwrap()
    }

    #[test]
    fn authtype_and_credential_complete_a_credential_and_come_back_on_store() {
        let mut credential = Credential {
            protocol: "http".into(),
            host: "h:1".into(),
            authtype: Some("Bearer".into()),
            ..Credential::default()
        };
        assert!(!credential.is_complete(), "an authtype alone is not enough");
        assert_eq!(credential.authorization(), None);
        credential.credential = Some(BString::from("tok"));
        assert!(credential.is_complete());
        assert_eq!(credential.authorization().as_deref(), Some("Bearer tok"));
        assert_eq!(
            input(&credential, Operation::Store),
            b"protocol=http\nhost=h:1\n"
        );
        credential.capa_authtype = true;
        assert_eq!(
            input(&credential, Operation::Store),
            b"capability[]=authtype\nauthtype=Bearer\ncredential=tok\nprotocol=http\nhost=h:1\n"
        );
    }

    #[test]
    fn state_and_continue_come_back_on_store_only_when_the_helper_announced_state() {
        let mut credential = Credential {
            protocol: "http".into(),
            host: "h:1".into(),
            username: Some(BString::from("u")),
            password: Some(BString::from("p")),
            wwwauth: vec!["Basic".into()],
            state: vec![BString::from("a:b"), BString::from("c")],
            multistage: true,
            ..Credential::default()
        };
        assert_eq!(
            input(&credential, Operation::Store),
            b"protocol=http\nhost=h:1\nusername=u\npassword=p\n"
        );
        credential.capa_state = true;
        assert_eq!(
            input(&credential, Operation::Erase),
            b"capability[]=state\nprotocol=http\nhost=h:1\nusername=u\npassword=p\nwwwauth[]=Basic\ncontinue=1\nstate[]=a:b\nstate[]=c\n"
                as &[u8]
        );
        assert!(
            !input(&credential, Operation::Get)
                .windows(6)
                .any(|w| w == b"state[")
        );
    }

    #[test]
    fn patterns_match_on_scheme_host_port_and_path_prefix() {
        let target = url("http://127.0.0.1:18790/o.git");
        assert!(pattern_matches("http://127.0.0.1:18790", &target));
        assert!(pattern_matches("http://127.0.0.1:18790/o.git", &target));
        assert!(pattern_matches("http://127.0.0.1:18790/", &target));
        assert!(!pattern_matches("https://127.0.0.1:18790", &target));
        assert!(!pattern_matches("http://127.0.0.1", &target));
        assert!(!pattern_matches("http://127.0.0.1:18790/other", &target));
        assert!(!pattern_matches("http://127.0.0.1:18790/o", &target));
        assert!(pattern_matches(
            "https://example.com",
            &url("https://example.com:443/x")
        ));
        assert!(pattern_matches(
            "https://*.example.com",
            &url("https://git.example.com/x")
        ));
    }

    #[test]
    fn a_post_challenge_asks_without_capabilities() {
        let credential = Credential {
            protocol: "http".into(),
            host: "h:1".into(),
            plain_get: true,
            ..Credential::default()
        };
        assert_eq!(
            input(&credential, Operation::Get),
            b"protocol=http\nhost=h:1\n"
        );
    }

    #[test]
    fn the_helper_input_is_gits() {
        let mut credential = Credential {
            protocol: "http".into(),
            host: "h:1".into(),
            username: Some("u".into()),
            password: Some("p".into()),
            wwwauth: vec!["Basic realm=\"r\"".into()],
            ..Credential::default()
        };
        assert_eq!(
            input(&credential, Operation::Get),
            b"capability[]=authtype\ncapability[]=state\nprotocol=http\nhost=h:1\nusername=u\nwwwauth[]=Basic realm=\"r\"\n"
        );
        assert_eq!(
            input(&credential, Operation::Store),
            b"protocol=http\nhost=h:1\nusername=u\npassword=p\n"
        );
        credential.path = Some("o.git".into());
        assert_eq!(
            input(&credential, Operation::Erase),
            b"protocol=http\nhost=h:1\npath=o.git\nusername=u\npassword=p\nwwwauth[]=Basic realm=\"r\"\n"
        );
    }

    #[test]
    fn store_and_erase_hear_the_expiry_and_refresh_token() {
        let credential = Credential {
            protocol: "http".into(),
            host: "h".into(),
            username: Some("u".into()),
            password: Some("p".into()),
            password_expiry_utc: Some(42),
            oauth_refresh_token: Some("rt".into()),
            ..Credential::default()
        };
        let expected: &[u8] =
            b"protocol=http\nhost=h\nusername=u\npassword=p\noauth_refresh_token=rt\npassword_expiry_utc=42\n";
        assert_eq!(input(&credential, Operation::Store), expected);
        assert_eq!(input(&credential, Operation::Erase), expected);
        assert!(!input(&credential, Operation::Get).contains_str("rt"));
    }

    #[test]
    fn fill_drops_an_expired_password_and_honours_quit() {
        let settings = |script: &str| CredentialSettings {
            helpers: vec![format!("!{script}")],
            ..CredentialSettings::default()
        };
        let mut credential = Credential::default();
        settings("echo username=a; echo password=b; echo password_expiry_utc=1; true")
            .fill(&mut credential)
            .unwrap();
        assert_eq!(
            credential.username.as_ref().map(|v| v.as_slice()),
            Some(&b"a"[..])
        );
        assert_eq!(credential.password, None);
        let mut credential = Credential::default();
        let error = settings("echo username=a; echo quit=1; true")
            .fill(&mut credential)
            .unwrap_err();
        assert_eq!(
            error,
            "credential helper '!echo username=a; echo quit=1; true' told us to quit"
        );
        let mut credential = Credential::default();
        settings("echo username=a; echo password=b; echo quit=1; true")
            .fill(&mut credential)
            .unwrap();
        assert_eq!(
            credential.password.as_ref().map(|v| v.as_slice()),
            Some(&b"b"[..])
        );
    }

    #[test]
    fn a_helper_url_replaces_the_credential() {
        let mut credential = Credential {
            username: Some("old".into()),
            ..Credential::default()
        };
        credential
            .reset_from_url("https://us%40er:p%3Aw@o.example:8443//a%20b/c/", true)
            .unwrap();
        assert_eq!(credential.protocol, "https");
        assert_eq!(credential.host, "o.example:8443");
        assert_eq!(credential.path.as_deref(), Some("a b/c"));
        assert_eq!(
            credential.username.as_ref().map(|v| v.as_slice()),
            Some(&b"us@er"[..])
        );
        assert_eq!(
            credential.password.as_ref().map(|v| v.as_slice()),
            Some(&b"p:w"[..])
        );
        credential
            .reset_from_url("http://h.example/x", false)
            .unwrap();
        assert!(credential.path.is_none() && credential.username.is_none());
        assert!(credential.reset_from_url("h.example/x", false).is_err());
        assert!(credential.reset_from_url("http://h/a%0ab", false).is_err());
    }

    #[test]
    fn the_debug_output_hides_the_password() {
        let credential = Credential {
            password: Some("hunter2".into()),
            ..Credential::default()
        };
        assert!(!format!("{credential:?}").contains("hunter2"));
    }
}
