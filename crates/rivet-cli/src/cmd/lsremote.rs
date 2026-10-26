//! `ls-remote`: list the refs a repository, remote or bundle file advertises.

use anyhow::Result;
use bstr::BStr;

use crate::cli::GlobalArgs;

/// Arguments to `rivet ls-remote`.
#[derive(Debug, clap::Args)]
pub struct LsRemoteArgs {
    /// Show only `refs/heads`.
    #[arg(long = "heads")]
    pub heads: bool,

    /// Show only `refs/tags`.
    #[arg(short = 't', long = "tags")]
    pub tags: bool,

    /// Leave out `HEAD` and peeled tags.
    #[arg(long = "refs")]
    pub refs: bool,

    /// Print the url of the remote and stop.
    #[arg(long = "get-url")]
    pub get_url: bool,

    /// Show the branch each symbolic ref points at.
    #[arg(long = "symref")]
    pub symref: bool,

    /// Exit with status 2 when no ref matched.
    #[arg(long = "exit-code")]
    pub exit_code: bool,

    /// Do not print the `From <url>` line.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Order the output by a ref-name key (`refname`, with `lstrip`/`rstrip`/`strip`; `-` reverses).
    #[arg(long = "sort", value_name = "KEY")]
    pub sort: Vec<String>,

    /// Accepted so the error can name it.
    #[arg(long = "upload-pack", visible_alias = "exec", value_name = "PATH")]
    pub upload_pack: Option<String>,

    /// Accepted so the error can name it.
    #[arg(short = 'o', long = "server-option", value_name = "OPTION")]
    pub server_option: Vec<String>,

    /// The repository: a url, a path, a bundle file or a configured remote.
    #[arg(value_name = "REPOSITORY")]
    pub repository: Option<String>,

    /// Only refs whose name ends in one of these patterns (`*` and `?` allowed).
    #[arg(value_name = "PATTERN")]
    pub patterns: Vec<String>,
}

/// Run `rivet ls-remote`.
pub fn run_ls_remote(global: &GlobalArgs, args: &LsRemoteArgs) -> Result<i32> {
    if args.upload_pack.is_some() || !args.server_option.is_empty() {
        anyhow::bail!("ls-remote: --upload-pack and --server-option are not implemented");
    }
    let start = super::base_dir(global)?;
    let mut repository = rivet_core::Repository::open_or_discover(&start).ok();

    // Without a repository argument git uses the upstream remote of the branch, then `origin`.
    let mut announce = false;
    let name = match &args.repository {
        Some(name) => name.clone(),
        None => {
            announce = true;
            let upstream = match repository.as_mut() {
                Some(repo) => {
                    let branch = repo.head()?.branch().map(|b| b.short_name().to_string());
                    match branch {
                        Some(b) => repo.branch_upstream(&b)?,
                        None => None,
                    }
                }
                None => None,
            };
            let remotes = match repository.as_ref() {
                Some(repo) => repo.remote_names(),
                None => Vec::new(),
            };
            match upstream {
                Some((remote, _)) if remote != "." => remote,
                _ if remotes.iter().any(|r| r.as_slice() == b"origin") => "origin".to_string(),
                _ if remotes.len() == 1 => remotes[0].to_string(),
                _ => {
                    eprintln!("fatal: No remote configured to list refs from.");
                    return Ok(128);
                }
            }
        }
    };
    let configured = match repository.as_mut() {
        Some(repo) => repo.remote_config(&name)?,
        None => None,
    };
    let url_text = match configured.as_ref().and_then(|c| c.urls.first()) {
        Some(url) => url.to_string(),
        None => name.clone(),
    };
    if args.get_url {
        println!("{url_text}");
        return Ok(0);
    }

    let died = |message: String| {
        eprintln!("fatal: {message}");
        eprintln!("fatal: Could not read from remote repository.");
        eprintln!();
        eprintln!("Please make sure you have the correct access rights");
        eprintln!("and the repository exists.");
        128
    };
    let Ok(url) = rivet_core::RemoteUrl::parse(BStr::new(url_text.as_bytes())) else {
        return Ok(died(format!(
            "'{name}' does not appear to be a git repository"
        )));
    };
    if url.scheme == rivet_core::remote::RemoteScheme::File {
        let path = std::path::PathBuf::from(String::from_utf8_lossy(&url.path).into_owned());
        if path.is_file() && !rivet_core::bundle::looks_like_bundle(&path) {
            // Git takes any regular file for a bundle and then trips over it as a gitfile.
            return Ok(died(format!("invalid gitfile format: {name}")));
        }
        let usable = if path.is_file() {
            true
        } else {
            // Discovery walks upwards, so a path that does not exist would find the repository
            // around the current directory.
            path.is_dir() && rivet_core::Repository::discover(&path).is_ok()
        };
        if !usable {
            return Ok(died(format!(
                "'{name}' does not appear to be a git repository"
            )));
        }
    }
    let format = repository
        .as_ref()
        .map_or(rivet_core::ObjectFormat::Sha1, |r| r.format());
    let mut transport = rivet_core::remote_ops::open_transport(&url)?;
    if let Some(repo) = &repository {
        transport.configure_credentials(
            rivet_core::remote::credential::CredentialSettings::for_url(repo.config(), &url)?,
        );
    }
    let advertisement = transport.connect(&url, format)?;
    if announce && !args.quiet {
        eprintln!(
            "From {}",
            rivet_core::remote_ops::display_remote_url(&url_text)
        );
    }

    // Git parses the keys only once the remote has answered.
    let sort_keys = match args
        .sort
        .iter()
        .map(|key| SortKey::parse(key))
        .collect::<Result<Vec<_>, SortError>>()
    {
        Ok(keys) => keys,
        Err(SortError::Refused(key)) => {
            anyhow::bail!("ls-remote: --sort={key} is not implemented")
        }
        Err(SortError::Fatal(message)) => {
            eprintln!("fatal: {message}");
            return Ok(128);
        }
    };
    let symrefs = if args.symref {
        advertisement.capabilities.symrefs()?
    } else {
        Vec::new()
    };
    let limit_to_kinds = args.heads || args.tags;
    let patterns: Vec<Vec<u8>> = args
        .patterns
        .iter()
        .map(|p| format!("*/{p}").into_bytes())
        .collect();
    // A peeled tag is a line of its own as far as the filters are concerned.
    let mut entries: Vec<(Vec<u8>, rivet_core::ObjectId, bool)> = Vec::new();
    for advertised in &advertisement.refs {
        entries.push((advertised.name.to_vec(), advertised.oid, false));
        if let Some(peeled) = advertised.peeled {
            let mut name = advertised.name.to_vec();
            name.extend_from_slice(b"^{}");
            entries.push((name, peeled, true));
        }
    }
    if !sort_keys.is_empty() {
        let suffixes = repository
            .as_ref()
            .map(super::foreachref::version_suffixes)
            .unwrap_or_default();
        entries.sort_by(|a, b| {
            // The last `--sort` is the most significant; ties fall back to the plain name.
            sort_keys
                .iter()
                .rev()
                .map(|key| key.compare(&a.0, &b.0, &suffixes))
                .find(|ordering| ordering.is_ne())
                .unwrap_or_else(|| a.0.cmp(&b.0))
        });
    }
    let mut shown = 0;
    for (ref_name, oid, peeled) in &entries {
        if args.refs && (*peeled || !ref_name.starts_with(b"refs/")) {
            continue;
        }
        if limit_to_kinds
            && !((args.heads && ref_name.starts_with(b"refs/heads/"))
                || (args.tags && ref_name.starts_with(b"refs/tags/")))
        {
            continue;
        }
        if !patterns.is_empty() {
            let mut subject = b"/".to_vec();
            subject.extend_from_slice(ref_name);
            if !patterns
                .iter()
                .any(|p| super::branching::glob_match(p, &subject))
            {
                continue;
            }
        }
        shown += 1;
        let printable = String::from_utf8_lossy(ref_name);
        if let Some((_, target)) = symrefs
            .iter()
            .find(|(source, _)| source.as_slice() == ref_name.as_slice())
        {
            println!("ref: {}\t{printable}", String::from_utf8_lossy(target));
        }
        println!("{oid}\t{printable}");
    }
    Ok(if args.exit_code && shown == 0 { 2 } else { 0 })
}

/// One `--sort` key: `refname` with an optional component strip, reversed by a leading `-`.
struct SortKey {
    reverse: bool,
    /// `version:` / `v:` prefix: compare with git's `versioncmp` instead of bytewise.
    version: bool,
    strip: Option<Strip>,
}

enum Strip {
    Left(i64),
    Right(i64),
}

enum SortError {
    /// Git reports this one and exits 128.
    Fatal(String),
    /// A key git accepts that rivet does not implement.
    Refused(String),
}

impl SortKey {
    /// Parse a key the way git's `ref-filter` does, with its error texts. Keys that need object
    /// data, `*` (ignore case) and `:short` are refused by name.
    fn parse(key: &str) -> Result<SortKey, SortError> {
        let (reverse, atom) = match key.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, key),
        };
        let (version, atom) = match atom
            .strip_prefix("version:")
            .or_else(|| atom.strip_prefix("v:"))
        {
            Some(rest) => (true, rest),
            None => (false, atom),
        };
        if atom.is_empty() {
            return Err(SortError::Fatal("malformed field name: ".to_string()));
        }
        if atom.starts_with('*')
            || atom.starts_with("refname:short")
            || atom.starts_with("refname:dir")
            || atom.starts_with("refname:base")
        {
            return Err(SortError::Refused(key.to_string()));
        }
        let Some(modifiers) = atom
            .strip_prefix("refname")
            .filter(|m| m.is_empty() || m.starts_with(':'))
        else {
            return Err(SortError::Fatal(format!("unknown field name: {atom}")));
        };
        let unrecognized =
            |m: &str| SortError::Fatal(format!("unrecognized %(refname) argument: {m}"));
        let mut strip = None;
        for modifier in modifiers
            .strip_prefix(':')
            .into_iter()
            .filter(|m| !m.is_empty())
        {
            let (name, value) = match modifier.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (modifier, None),
            };
            let build: fn(i64) -> Strip = match name {
                "lstrip" | "strip" => Strip::Left,
                "rstrip" => Strip::Right,
                _ => return Err(unrecognized(modifier)),
            };
            let Some(count) = value else {
                return Err(unrecognized(modifier));
            };
            let count: i64 = count.parse().map_err(|_| {
                SortError::Fatal(format!("Integer value expected refname:{modifier}"))
            })?;
            strip = Some(build(count));
        }
        Ok(SortKey {
            reverse,
            version,
            strip,
        })
    }

    fn compare(&self, a: &[u8], b: &[u8], suffixes: &[String]) -> std::cmp::Ordering {
        let (left, right) = (self.value(a), self.value(b));
        let ordering = if self.version {
            super::foreachref::versioncmp(
                &String::from_utf8_lossy(&left),
                &String::from_utf8_lossy(&right),
                suffixes,
            )
        } else {
            left.cmp(&right)
        };
        if self.reverse {
            ordering.reverse()
        } else {
            ordering
        }
    }

    fn value(&self, name: &[u8]) -> Vec<u8> {
        let parts: Vec<&[u8]> = name.split(|&c| c == b'/').collect();
        let kept: &[&[u8]] = match self.strip {
            None => &parts,
            Some(Strip::Left(n)) => {
                let drop = if n >= 0 {
                    n as usize
                } else {
                    parts.len().saturating_sub(n.unsigned_abs() as usize)
                };
                &parts[drop.min(parts.len())..]
            }
            Some(Strip::Right(n)) => {
                let keep = if n >= 0 {
                    parts.len().saturating_sub(n as usize)
                } else {
                    (n.unsigned_abs() as usize).min(parts.len())
                };
                &parts[..keep]
            }
        };
        kept.join(&b'/')
    }
}
