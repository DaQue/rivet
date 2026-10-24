//! Git bundles: a pack of objects together with the refs (and the commits it builds on) that
//! describe it, in one file.
//!
//! The file starts with `# v2 git bundle`, then one `-<id> <comment>` line per prerequisite commit
//! the receiver must already have, then one `<id> <refname>` line per ref, an empty line, and the
//! pack. Version 3 adds `@capability` lines before the refs; only `@object-format=sha1` is
//! understood and any other capability is refused.

use std::io::Write;
use std::path::Path;

use bstr::{BStr, BString, ByteSlice};

use crate::error::{Error, Result};
use crate::hash::{ObjectFormat, ObjectId};
use crate::remote::{
    AdvertisedRef, CapabilitySet, ProtocolVersion, RefAdvertisement, RemoteUrl, Transport,
};
use crate::repo::Repository;

/// A commit the receiver of a bundle must already have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prerequisite {
    /// The required commit.
    pub oid: ObjectId,
    /// The text after the id on its header line (the commit's subject, as git writes it).
    pub comment: BString,
}

/// A ref a bundle carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleRef {
    /// What the ref points at.
    pub oid: ObjectId,
    /// The full ref name, or `HEAD`.
    pub name: BString,
}

/// A parsed bundle file, header and pack.
#[derive(Clone, Debug)]
pub struct Bundle {
    version: u32,
    prerequisites: Vec<Prerequisite>,
    refs: Vec<BundleRef>,
    data: Vec<u8>,
    pack_start: usize,
}

impl Bundle {
    /// Parse the bytes of a bundle file. `shown` is the name used in error messages.
    ///
    /// # Caveats
    ///
    /// Only the header is checked here; the pack is validated when it is stored. Ids are read at
    /// `format`'s width, so a SHA-256 bundle (always version 3) never gets this far.
    ///
    /// # Errors
    ///
    /// [`Error::Message`] with git's text for a file that does not start with `# v2 git bundle` or
    /// `# v3 git bundle` (`'<name>' does not look like a v2 or v3 bundle file`), for a capability
    /// other than `object-format=sha1` (`unknown capability '<cap>'`) and for a header line that is
    /// neither a prerequisite nor a ref.
    pub fn parse(data: Vec<u8>, format: ObjectFormat, shown: &str) -> Result<Self> {
        let not_a_bundle = || {
            Error::Message(format!(
                "'{shown}' does not look like a v2 or v3 bundle file"
            ))
        };
        let line_end = |from: usize, data: &[u8]| data[from..].find_byte(b'\n').map(|i| from + i);
        let first_end = line_end(0, &data).ok_or_else(not_a_bundle)?;
        let version = match &data[..first_end] {
            b"# v2 git bundle" => 2,
            b"# v3 git bundle" => 3,
            _ => return Err(not_a_bundle()),
        };
        let mut prerequisites = Vec::new();
        let mut refs = Vec::new();
        let mut at = first_end + 1;
        loop {
            let end = line_end(at, &data).ok_or_else(|| {
                Error::Message(format!(
                    "unrecognized header: {} ({})",
                    data[at..].as_bstr(),
                    data.len() - at
                ))
            })?;
            let line = &data[at..end];
            at = end + 1;
            if line.is_empty() {
                break;
            }
            if version == 3
                && let Some(capability) = line.strip_prefix(b"@")
            {
                if capability != format!("object-format={}", format.as_str()).as_bytes() {
                    return Err(Error::Message(format!(
                        "unknown capability '{}'",
                        capability.as_bstr()
                    )));
                }
                continue;
            }
            let unrecognized = || {
                Error::Message(format!(
                    "unrecognized header: {} ({})",
                    line.as_bstr(),
                    line.len()
                ))
            };
            let (hex, rest) = match line.strip_prefix(b"-") {
                Some(rest) => (rest, true),
                None => (line, false),
            };
            let width = format.hex_len();
            if hex.len() < width {
                return Err(unrecognized());
            }
            let oid =
                ObjectId::from_hex(format, BStr::new(&hex[..width])).map_err(|_| unrecognized())?;
            let tail = &hex[width..];
            let tail = match tail.first() {
                None => &tail[..0],
                Some(b' ') => &tail[1..],
                Some(_) => return Err(unrecognized()),
            };
            if rest {
                prerequisites.push(Prerequisite {
                    oid,
                    comment: BString::from(tail),
                });
            } else {
                refs.push(BundleRef {
                    oid,
                    name: BString::from(tail),
                });
            }
        }
        Ok(Self {
            version,
            prerequisites,
            refs,
            data,
            pack_start: at,
        })
    }

    /// Read and parse a bundle file.
    ///
    /// # Errors
    ///
    /// [`Error::Message`] `could not open '<path>'` when the file cannot be read, and the errors
    /// of [`Bundle::parse`].
    pub fn read(path: &Path, format: ObjectFormat, shown: &str) -> Result<Self> {
        let data =
            std::fs::read(path).map_err(|_| Error::Message(format!("could not open '{shown}'")))?;
        Self::parse(data, format, shown)
    }

    /// The bundle format version, 2 or 3.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The commits the receiver must already have, in header order.
    pub fn prerequisites(&self) -> &[Prerequisite] {
        &self.prerequisites
    }

    /// The refs the bundle carries, in header order.
    pub fn refs(&self) -> &[BundleRef] {
        &self.refs
    }

    /// The pack that follows the header.
    pub fn pack(&self) -> &[u8] {
        &self.data[self.pack_start..]
    }

    /// The prerequisites whose commit `repository` lacks.
    pub fn missing_prerequisites(&self, repository: &Repository) -> Vec<&Prerequisite> {
        self.prerequisites
            .iter()
            .filter(|p| !repository.odb().contains(&p.oid))
            .collect()
    }

    /// Store the bundle's objects in `repository`.
    ///
    /// # Caveats
    ///
    /// Prerequisites are not checked; call [`Bundle::missing_prerequisites`] first. A thin pack
    /// (deltas against objects in the prerequisites) is completed from the repository.
    ///
    /// # Errors
    ///
    /// The errors of storing a pack: a corrupt or truncated pack, a missing delta base.
    pub fn unpack(&self, repository: &mut Repository) -> Result<()> {
        repository.store_downloaded(self.pack())
    }
}

/// The header of a bundle of `version` (2, or 3 which adds the `@object-format` capability) for
/// these prerequisites and refs.
pub fn header(
    version: u32,
    format: ObjectFormat,
    prerequisites: &[Prerequisite],
    refs: &[BundleRef],
) -> Vec<u8> {
    let mut out = format!("# v{version} git bundle\n").into_bytes();
    if version == 3 {
        out.extend_from_slice(format!("@object-format={}\n", format.as_str()).as_bytes());
    }
    for p in prerequisites {
        out.push(b'-');
        out.extend_from_slice(p.oid.to_string().as_bytes());
        if !p.comment.is_empty() {
            out.push(b' ');
            out.extend_from_slice(&p.comment);
        }
        out.push(b'\n');
    }
    for r in refs {
        out.extend_from_slice(r.oid.to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(&r.name);
        out.push(b'\n');
    }
    out.push(b'\n');
    out
}

/// Whether `path` is a regular file that starts like a bundle.
pub fn looks_like_bundle(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut start = [0u8; 15];
    file.read_exact(&mut start).is_ok()
        && (&start == b"# v2 git bundle" || &start == b"# v3 git bundle")
}

/// A transport that serves the refs and the pack of a bundle file, for `fetch`.
#[derive(Debug)]
pub(crate) struct BundleTransport {
    path: std::path::PathBuf,
    shown: String,
    advertisement: Option<RefAdvertisement>,
    pack: Vec<u8>,
}

impl BundleTransport {
    pub(crate) fn open(url: &RemoteUrl) -> Self {
        use std::os::unix::ffi::OsStringExt;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(url.path.to_vec()));
        Self {
            shown: path.display().to_string(),
            path,
            advertisement: None,
            pack: Vec::new(),
        }
    }
}

impl Transport for BundleTransport {
    fn connect(&mut self, _url: &RemoteUrl, format: ObjectFormat) -> Result<RefAdvertisement> {
        let bundle = Bundle::read(&self.path, format, &self.shown)?;
        let mut caps = CapabilitySet::default();
        for name in ["ofs-delta", "thin-pack"] {
            caps.insert(name.as_bytes())?;
        }
        caps.insert(format!("object-format={}", format.as_str()).as_bytes())?;
        let mut refs: Vec<AdvertisedRef> = bundle
            .refs()
            .iter()
            .map(|r| AdvertisedRef {
                name: r.name.clone(),
                oid: r.oid,
                peeled: None,
            })
            .collect();
        // Git lists a bundle's refs back to front; fetch maps them in that order.
        refs.reverse();
        self.pack = bundle.pack().to_vec();
        let advertisement = RefAdvertisement {
            capabilities: caps,
            refs,
            format,
            agent: None,
            version: ProtocolVersion::V0,
        };
        self.advertisement = Some(advertisement.clone());
        Ok(advertisement)
    }

    fn capabilities(&self) -> &CapabilitySet<'_> {
        self.advertisement
            .as_ref()
            .map(|ad| &ad.capabilities)
            .unwrap_or_else(|| {
                static DEFAULT: std::sync::OnceLock<CapabilitySet<'static>> =
                    std::sync::OnceLock::new();
                DEFAULT.get_or_init(CapabilitySet::default)
            })
    }

    fn fetch(
        &mut self,
        _wants: &[ObjectId],
        _haves: &[ObjectId],
        into: &mut dyn Write,
    ) -> Result<()> {
        into.write_all(&self.pack)
            .map_err(|e| Error::Message(e.to_string()))
    }

    fn push(
        &mut self,
        _updates: &[crate::remote::RefUpdate],
        _pack: &[u8],
    ) -> Result<Vec<crate::remote::PushStatus>> {
        Err(Error::Message("cannot push to a bundle".to_string()))
    }

    fn ls_refs(&mut self, _prefixes: &[BString], _peel: bool) -> Result<Vec<AdvertisedRef>> {
        Err(Error::Message(
            "a bundle has no protocol v2 ls-refs".to_string(),
        ))
    }

    fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_hex(
            ObjectFormat::Sha1,
            BStr::new(format!("{:02x}", byte).repeat(20).as_bytes()),
        )
        .unwrap()
    }

    #[test]
    fn header_round_trips_prerequisites_refs_and_version() {
        let prerequisites = vec![Prerequisite {
            oid: oid(1),
            comment: BString::from("subject line"),
        }];
        let refs = vec![BundleRef {
            oid: oid(2),
            name: BString::from("refs/heads/main"),
        }];
        for version in [2, 3] {
            let mut bytes = header(version, ObjectFormat::Sha1, &prerequisites, &refs);
            bytes.extend_from_slice(b"PACK-bytes");
            let bundle = Bundle::parse(bytes, ObjectFormat::Sha1, "b").unwrap();
            assert_eq!(bundle.version(), version);
            assert_eq!(bundle.prerequisites(), prerequisites.as_slice());
            assert_eq!(bundle.refs(), refs.as_slice());
            assert_eq!(bundle.pack(), b"PACK-bytes");
        }
    }

    #[test]
    fn a_prerequisite_without_comment_and_an_unknown_capability_are_handled_as_git_does() {
        let line = format!("# v3 git bundle\n@object-format=sha1\n-{}\n\n", oid(3));
        let bundle = Bundle::parse(line.into_bytes(), ObjectFormat::Sha1, "b").unwrap();
        assert!(bundle.prerequisites()[0].comment.is_empty());
        let err = Bundle::parse(
            b"# v3 git bundle\n@filter=blob:none\n\n".to_vec(),
            ObjectFormat::Sha1,
            "b",
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "unknown capability 'filter=blob:none'");
    }

    #[test]
    fn files_that_are_not_bundles_or_have_bad_header_lines_are_refused_by_name() {
        let err = Bundle::parse(b"hello\n".to_vec(), ObjectFormat::Sha1, "x.bundle").unwrap_err();
        assert_eq!(
            err.to_string(),
            "'x.bundle' does not look like a v2 or v3 bundle file"
        );
        let err = Bundle::parse(
            b"# v2 git bundle\nzzzz\n\n".to_vec(),
            ObjectFormat::Sha1,
            "x",
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "unrecognized header: zzzz (4)");
    }
}
