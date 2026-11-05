//! Repository maintenance: `gc`, `repack` and `count-objects`.

use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use anyhow::Result;
use bstr::BStr;

use crate::cli::GlobalArgs;

/// Arguments to `rivet gc`.
#[derive(Debug, clap::Args)]
pub struct GcArgs {
    /// Prune unreachable objects older than this date (`now`, `2.weeks.ago`, a timestamp).
    #[arg(long = "prune", value_name = "DATE")]
    pub prune: Option<String>,

    /// Keep every unreachable object.
    #[arg(long = "no-prune")]
    pub no_prune: bool,

    /// Do nothing unless the repository has too many loose objects or packs.
    #[arg(long = "auto")]
    pub auto: bool,

    /// Accepted for compatibility; Rivet's repack has no tuning.
    #[arg(long = "aggressive")]
    pub aggressive: bool,

    /// Accepted for compatibility.
    #[arg(long = "force")]
    pub force: bool,

    /// Pack unreachable objects that are not old enough to prune into a cruft pack.
    #[arg(long = "cruft", overrides_with = "no_cruft")]
    pub cruft: bool,

    /// Leave such objects loose instead.
    #[arg(long = "no-cruft", overrides_with = "cruft")]
    pub no_cruft: bool,
}

/// Placeholder clap stores for `--window` / `--depth` given without a value; the real check reads
/// the raw command line.
const MISSING_VALUE: &str = "missing";

/// Arguments to `rivet repack`.
#[derive(Debug, clap::Args)]
pub struct RepackArgs {
    /// Pack everything reachable into one pack.
    #[arg(short = 'a')]
    pub all: bool,

    /// Like `-a`, but turn the unreachable objects of the replaced packs loose.
    #[arg(short = 'A')]
    pub loosen: bool,

    /// Remove the packs the new one replaces, and the loose objects it now holds.
    #[arg(short = 'd')]
    pub delete: bool,

    /// With `-a`/`-A`: also pack the unreachable objects of the replaced packs.
    #[arg(short = 'k', long = "keep-unreachable")]
    pub keep_unreachable: bool,

    /// Compute deltas again instead of reusing the stored ones.
    #[arg(short = 'f')]
    pub no_reuse_delta: bool,

    /// Compute deltas and compress objects again.
    #[arg(short = 'F')]
    pub no_reuse_object: bool,

    /// Like `-a`; the unreachable objects go into a cruft pack of their own.
    #[arg(long = "cruft")]
    pub cruft: bool,

    /// With `--cruft`: leave out the unreachable objects not newer than this date.
    #[arg(long = "cruft-expiration", value_name = "DATE")]
    pub cruft_expiration: Option<String>,

    /// Roll the smallest packs up so that pack sizes grow by at least this factor.
    #[arg(long = "geometric", value_name = "FACTOR")]
    pub geometric: Option<String>,

    /// Size of the delta window; validated like git's option and otherwise ignored.
    #[arg(long = "window", value_name = "N", num_args = 0..=1, default_missing_value = MISSING_VALUE, action = clap::ArgAction::Append)]
    pub window: Vec<String>,

    /// Forget an earlier `--window`.
    #[arg(long = "no-window", action = clap::ArgAction::Count)]
    pub no_window: u8,

    /// Maximum delta depth; validated like git's option and otherwise ignored.
    #[arg(long = "depth", value_name = "N", num_args = 0..=1, default_missing_value = MISSING_VALUE, action = clap::ArgAction::Append)]
    pub depth: Vec<String>,

    /// Forget an earlier `--depth`.
    #[arg(long = "no-depth", action = clap::ArgAction::Count)]
    pub no_depth: u8,

    /// Largest output pack; validated like git's option, a pack is never split.
    #[arg(long = "max-pack-size", value_name = "N", num_args = 0..=1, default_missing_value = MISSING_VALUE, action = clap::ArgAction::Append)]
    pub max_pack_size: Vec<String>,

    /// Largest cruft pack; validated like git's option, a pack is never split.
    #[arg(long = "max-cruft-size", value_name = "N", num_args = 0..=1, default_missing_value = MISSING_VALUE, action = clap::ArgAction::Append)]
    pub max_cruft_size: Vec<String>,

    /// Accepted for compatibility: objects from alternates are never packed here.
    #[arg(short = 'l')]
    pub local: bool,

    /// Accepted for compatibility: no server info files are written.
    #[arg(short = 'n')]
    pub no_update_server_info: bool,
}

/// Arguments to `rivet count-objects`.
#[derive(Debug, clap::Args)]
pub struct CountObjectsArgs {
    /// Print every statistic, not just the loose-object summary.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    /// Print sizes in a human-readable unit.
    #[arg(short = 'H', long = "human-readable")]
    pub human_readable: bool,
}

fn now() -> i64 {
    super::pretty::now()
}

/// `now`, `never`, `N.unit.ago` / `N unit ago`, or an absolute date.
fn parse_expiry(text: &str) -> Result<Option<i64>> {
    let text = text.trim();
    match text {
        "now" => return Ok(Some(now())),
        "never" | "false" => return Ok(None),
        _ => {}
    }
    let words: Vec<&str> = text
        .split(|c: char| c == '.' || c.is_whitespace())
        .filter(|w| !w.is_empty() && *w != "ago")
        .collect();
    if words.len() == 2
        && let Ok(count) = words[0].parse::<i64>()
    {
        let unit = match words.get(1).map(|u| u.trim_end_matches('s')) {
            Some("second") => 1,
            Some("minute") => 60,
            Some("hour") => 3600,
            Some("day") => 86_400,
            Some("week") => 7 * 86_400,
            Some("month") => 30 * 86_400,
            Some("year") => 365 * 86_400,
            _ => anyhow::bail!("failed to parse expiry value {text}"),
        };
        return Ok(Some(now() - count * unit));
    }
    match rivet_core::ops::parse_approxidate(text) {
        Some(when) => Ok(Some(when)),
        None => anyhow::bail!("failed to parse expiry value {text}"),
    }
}

/// Handler for `rivet gc`.
///
/// # Exit codes
///
/// * `0` — done (or nothing to do under `--auto`).
/// * `128` — an object could not be read or a file could not be written; nothing was removed
///   before the failure.
pub fn run_gc(global: &GlobalArgs, args: &GcArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let get = |key: &str| repository.config_value(BStr::new(key)).ok().flatten();
    if args.auto {
        let threshold: usize = get("gc.auto")
            .and_then(|v| String::from_utf8_lossy(&v).trim().parse().ok())
            .unwrap_or(6700);
        let pack_limit: usize = get("gc.autoPackLimit")
            .and_then(|v| String::from_utf8_lossy(&v).trim().parse().ok())
            .unwrap_or(50);
        // Git estimates the loose count from one fan-out directory.
        let sample = std::fs::read_dir(repository.common_dir().join("objects/17"))
            .map(|d| {
                d.flatten()
                    .filter(|e| e.file_name().len() == repository.format().hex_len() - 2)
                    .count()
            })
            .unwrap_or(0);
        let too_many_loose = threshold > 0 && sample > threshold.div_ceil(256);
        let too_many_packs = pack_limit > 0 && repository.odb().packs().len() > pack_limit;
        if !too_many_loose && !too_many_packs {
            return Ok(0);
        }
        if !global.quiet {
            println!("Auto packing the repository for optimum performance.");
            println!("See \"git help gc\" for manual housekeeping.");
        }
    }
    let mut options = rivet_core::gc::GcOptions::new();
    options.prune_before = if args.no_prune {
        None
    } else {
        let text = args
            .prune
            .clone()
            .or_else(|| get("gc.pruneExpire").map(|v| String::from_utf8_lossy(&v).into_owned()))
            .unwrap_or_else(|| "2.weeks.ago".to_string());
        parse_expiry(&text)
            .map_err(|_| anyhow::anyhow!("failed to parse prune expiry value {text}"))?
    };
    let reflog_setting = |key: &str, default: &str| -> Result<Option<i64>> {
        let text = get(key)
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_else(|| default.to_string());
        parse_expiry(&text)
    };
    // `gc.cruftPacks` defaults to true, as in git 2.56; the flags win over the setting.
    options.cruft = if args.cruft {
        true
    } else if args.no_cruft {
        false
    } else {
        get("gc.cruftPacks")
            .and_then(|v| rivet_core::config::parse_bool(BStr::new(&v[..])).ok())
            .unwrap_or(true)
    };
    options.reflog_expire = reflog_setting("gc.reflogExpire", "90.days")?;
    options.reflog_expire_unreachable = reflog_setting("gc.reflogExpireUnreachable", "30.days")?;
    rivet_core::gc::gc(&repository, &options)?;
    Ok(0)
}

/// Handler for `rivet repack`.
///
/// Packs the loose reachable objects into a new pack (`-a`/`-A`: everything reachable) and, with
/// `-d`, removes what the new pack makes redundant.
///
/// # Exit codes
///
/// `0`; `128` outside a repository.
pub fn run_repack(global: &GlobalArgs, args: &RepackArgs) -> Result<i32> {
    let repository = super::open(global)?;
    if args.cruft && args.loosen {
        anyhow::bail!("options '-A' and '--cruft' cannot be used together");
    }
    if args.cruft && args.keep_unreachable {
        anyhow::bail!("options '-k/--keep-unreachable' and '--cruft' cannot be used together");
    }
    // git keeps `--window`/`--depth` as strings: only the last one standing (`--no-<name>` drops
    // it) is read, after the whole command line is parsed.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    for name in ["window", "depth"] {
        let mut last: Option<String> = None;
        let mut it = raw.iter().skip_while(|a| *a != "repack").skip(1);
        while let Some(arg) = it.next() {
            if arg == "--" {
                break;
            }
            if let Some(value) = arg.strip_prefix(&format!("--{name}=")) {
                last = Some(value.to_string());
            } else if *arg == format!("--{name}") {
                match it.next() {
                    Some(value) => last = Some(value.clone()),
                    None => {
                        eprintln!("error: option `{name}' requires a value");
                        return Ok(129);
                    }
                }
            } else if *arg == format!("--no-{name}") {
                last = None;
            }
        }
        let Some(text) = last else { continue };
        if text.is_empty() {
            eprintln!("error: option `{name}' expects a numerical value");
            return Ok(129);
        }
        match parse_factor(&text) {
            Ok(value) => {
                if name == "depth" && value > 4095 {
                    eprintln!("warning: delta chain depth {value} is too deep, forcing 4095");
                }
            }
            Err(FactorError::Syntax) => {
                eprintln!(
                    "error: option `{name}' expects an integer value with an optional k/m/g suffix"
                );
                return Ok(129);
            }
            Err(FactorError::Range) => {
                eprintln!(
                    "error: value {text} for option `{name}' not in range [{},{}]",
                    i32::MIN,
                    i32::MAX
                );
                return Ok(129);
            }
        }
    }
    // `--max-pack-size` and `--max-cruft-size` are magnitudes: every occurrence is read in
    // order, the last one is the limit, and a limit under 1 MiB (but not 0) is raised with a
    // warning (the cruft one only when `--cruft` is on, as it reaches `pack-objects` then).
    {
        let mut limits = [0u64; 2];
        let mut it = raw.iter().skip_while(|a| *a != "repack").skip(1);
        while let Some(arg) = it.next() {
            if arg == "--" {
                break;
            }
            let mut found = None;
            for (slot, name) in ["max-pack-size", "max-cruft-size"].iter().enumerate() {
                if let Some(value) = arg.strip_prefix(&format!("--{name}=")) {
                    found = Some((slot, *name, value.to_string()));
                } else if *arg == format!("--{name}") {
                    match it.next() {
                        Some(value) => found = Some((slot, *name, value.clone())),
                        None => {
                            eprintln!("error: option `{name}' requires a value");
                            return Ok(129);
                        }
                    }
                }
            }
            let Some((slot, name, text)) = found else {
                continue;
            };
            if text.is_empty() {
                eprintln!("error: option `{name}' expects a numerical value");
                return Ok(129);
            }
            match parse_magnitude(&text) {
                Ok(value) => limits[slot] = value,
                Err(FactorError::Syntax) => {
                    eprintln!(
                        "error: option `{name}' expects a non-negative integer value with an optional k/m/g suffix"
                    );
                    return Ok(129);
                }
                Err(FactorError::Range) => {
                    eprintln!("error: value {text} for option `{name}' not in range [0,-1]");
                    return Ok(129);
                }
            }
        }
        // The cruft pack takes its own limit, or the general one when that is 0.
        let cruft_limit = if limits[1] > 0 { limits[1] } else { limits[0] };
        for (applies, limit) in [(true, limits[0]), (args.cruft, cruft_limit)] {
            if applies && limit > 0 && limit < 1 << 20 {
                eprintln!("warning: minimum pack size limit is 1 MiB");
            }
        }
    }
    let geometric = match &args.geometric {
        None => None,
        Some(text) => match parse_factor(text) {
            Ok(factor) => Some(factor),
            Err(FactorError::Syntax) => {
                eprintln!(
                    "error: option `geometric' expects an integer value with an optional k/m/g suffix"
                );
                return Ok(129);
            }
            Err(FactorError::Range) => {
                eprintln!(
                    "error: value {text} for option `geometric' not in range [{},{}]",
                    i32::MIN,
                    i32::MAX
                );
                return Ok(129);
            }
        },
    };
    if geometric.is_some_and(|factor| factor != 0) && (args.all || args.loosen || args.cruft) {
        anyhow::bail!("options '--geometric' and '-A/-a' cannot be used together");
    }
    if args.all && args.loosen {
        anyhow::bail!("repack: -a and -A cannot be combined");
    }

    let options = rivet_core::gc::RepackOptions {
        all: args.all,
        loosen_unreachable: args.loosen,
        delete_redundant: args.delete,
        keep_unreachable: args.keep_unreachable,
        no_reuse_delta: args.no_reuse_delta || args.no_reuse_object,
        geometric: geometric.map(|factor| factor.max(0) as u64),
        cruft: args.cruft,
        cruft_expiration: match &args.cruft_expiration {
            Some(text) if args.cruft => parse_expiry(text)?,
            _ => None,
        },
    };
    let report = rivet_core::gc::repack(&repository, &options)?;
    if report.objects_packed == 0 && !global.quiet {
        println!("Nothing new to pack.");
    }
    Ok(0)
}

/// Why [`parse_factor`] refused a `--geometric` value.
enum FactorError {
    /// Not an integer with an optional `k`/`m`/`g` suffix.
    Syntax,
    /// An integer, but outside a C `int` once the suffix is applied.
    Range,
}

/// An integer with an optional `k`/`m`/`g` suffix, as git's option parser reads it: the result
/// must fit a C `int`, and a number too big for any integer type is out of range, not malformed.
/// git's unsigned magnitude (`strtoumax` with base 0, then an optional `k`/`m`/`g`).
fn parse_magnitude(text: &str) -> Result<u64, FactorError> {
    let s = text.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let s = s.strip_prefix('+').unwrap_or(s);
    let bytes = s.as_bytes();
    let (radix, start) = if bytes.len() > 2
        && bytes[0] == b'0'
        && (bytes[1] | 0x20) == b'x'
        && bytes[2].is_ascii_hexdigit()
    {
        (16, 2)
    } else if bytes.first() == Some(&b'0') {
        (8, 0)
    } else {
        (10, 0)
    };
    let digits_end = bytes[start..]
        .iter()
        .position(|b| !(*b as char).is_digit(radix))
        .map_or(bytes.len(), |n| n + start);
    if digits_end == start {
        return Err(FactorError::Syntax);
    }
    let mut value: u64 = 0;
    let mut overflow = false;
    for b in &bytes[start..digits_end] {
        let digit = u64::from((*b as char).to_digit(radix).unwrap_or(0));
        match value
            .checked_mul(u64::from(radix))
            .and_then(|v| v.checked_add(digit))
        {
            Some(v) => value = v,
            None => overflow = true,
        }
    }
    if overflow {
        return Err(FactorError::Range);
    }
    let factor: u64 = match &s[digits_end..] {
        "" => 1,
        "k" | "K" => 1 << 10,
        "m" | "M" => 1 << 20,
        "g" | "G" => 1 << 30,
        _ => return Err(FactorError::Syntax),
    };
    value.checked_mul(factor).ok_or(FactorError::Range)
}

fn parse_factor(text: &str) -> Result<i64, FactorError> {
    let last = text.chars().last().ok_or(FactorError::Syntax)?;
    let (digits, unit) = match last {
        'k' | 'K' => (&text[..text.len() - 1], 1i128 << 10),
        'm' | 'M' => (&text[..text.len() - 1], 1 << 20),
        'g' | 'G' => (&text[..text.len() - 1], 1 << 30),
        _ => (text, 1),
    };
    let digits = digits.trim();
    let body = digits.strip_prefix(['+', '-']).unwrap_or(digits);
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit()) {
        return Err(FactorError::Syntax);
    }
    let value = digits
        .parse::<i128>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .filter(|n| i32::try_from(*n).is_ok())
        .ok_or(FactorError::Range)?;
    Ok(value as i64)
}

fn is_loose_name(name: &str, hex_len: usize) -> bool {
    name.len() == hex_len - 2 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn humanise(bytes: u64) -> String {
    if bytes > 1 << 30 {
        format!("{:.2} GiB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes > 1 << 20 {
        format!("{:.2} MiB", bytes as f64 / (1u64 << 20) as f64)
    } else if bytes > 1 << 10 {
        format!("{:.2} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

/// Handler for `rivet count-objects`.
///
/// Counts follow `git count-objects`: `size` is disk usage of the loose files in KiB, `in-pack`
/// the entries of every pack, `prune-packable` the loose objects a pack also holds, and
/// `garbage` the unrecognised files inside `objects/`.
pub fn run_count_objects(global: &GlobalArgs, args: &CountObjectsArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let hex_len = repository.format().hex_len();
    let objects = repository.odb().objects_dir().to_path_buf();

    let mut loose = 0u64;
    let mut loose_bytes = 0u64;
    let mut prunable = 0u64;
    let mut garbage = 0u64;
    let mut garbage_bytes = 0u64;
    let mut note_garbage = |path: &Path, size: u64| {
        garbage += 1;
        garbage_bytes += size;
        if args.verbose || global.verbose {
            let shown = std::env::current_dir()
                .ok()
                .and_then(|cwd| path.strip_prefix(cwd).ok().map(Path::to_path_buf))
                .unwrap_or_else(|| path.to_path_buf());
            eprintln!("warning: garbage found: {}", shown.display());
        }
    };

    let mut fanout: Vec<_> = std::fs::read_dir(&objects)?.flatten().collect();
    fanout.sort_by_key(std::fs::DirEntry::file_name);
    for dir in fanout {
        let name = dir.file_name().to_string_lossy().into_owned();
        if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let file_name = file.file_name().to_string_lossy().into_owned();
            let meta = file.metadata()?;
            if is_loose_name(&file_name, hex_len) {
                loose += 1;
                loose_bytes += meta.blocks() * 512;
                let hex = format!("{name}{file_name}");
                if let Ok(oid) =
                    rivet_core::ObjectId::from_hex(repository.format(), BStr::new(hex.as_bytes()))
                    && repository.odb().packs().iter().any(|p| p.contains(&oid))
                {
                    prunable += 1;
                }
            } else {
                note_garbage(&file.path(), meta.len());
            }
        }
    }

    let mut in_pack = 0u64;
    let mut pack_count = 0u64;
    let mut pack_bytes = 0u64;
    for pack in repository.odb().packs() {
        in_pack += pack.len() as u64;
        pack_count += 1;
        let stem = format!("pack-{}", pack.checksum());
        for extension in ["pack", "idx"] {
            let path = objects.join("pack").join(format!("{stem}.{extension}"));
            pack_bytes += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        }
    }
    if let Ok(entries) = std::fs::read_dir(objects.join("pack")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let known = [
                ".pack",
                ".idx",
                ".rev",
                ".keep",
                ".bitmap",
                ".promisor",
                ".mtimes",
            ]
            .iter()
            .any(|ext| name.ends_with(ext));
            if !known {
                note_garbage(
                    &entry.path(),
                    entry.metadata().map(|m| m.len()).unwrap_or(0),
                );
            }
        }
    }

    let size = |bytes: u64| -> String {
        if args.human_readable {
            humanise(bytes)
        } else {
            (bytes / 1024).to_string()
        }
    };
    if args.verbose {
        println!("count: {loose}");
        println!("size: {}", size(loose_bytes));
        println!("in-pack: {in_pack}");
        println!("packs: {pack_count}");
        println!("size-pack: {}", size(pack_bytes));
        println!("prune-packable: {prunable}");
        println!("garbage: {garbage}");
        println!("size-garbage: {}", size(garbage_bytes));
    } else if args.human_readable {
        println!("{loose} objects, {}", humanise(loose_bytes));
    } else {
        println!("{loose} objects, {} kilobytes", loose_bytes / 1024);
    }
    Ok(0)
}
