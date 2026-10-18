//! Phase 11 — sideband demultiplexing: one stream, three channels, and a pack that must not be
//! mistaken for progress.
//!
//! Once a fetch or push has negotiated `side-band-64k`, the server stops writing a bare pack and
//! starts writing pkt-lines whose first payload byte names a channel:
//!
//! | Channel | Name | Carries |
//! |---|---|---|
//! | 1 | [`SidebandKind::Pack`] | the packfile, or on a push the `report-status` lines |
//! | 2 | [`SidebandKind::Progress`] | human text: `Counting objects`, `remote:`, hook output |
//! | 3 | [`SidebandKind::Error`] | a fatal message, after which the server closes |
//!
//! The framing exists because the three are interleaved *within* a single byte stream and there is no
//! other way to tell them apart. A client that ignored the channel byte and concatenated the payloads
//! would produce a file that begins with a pack header and has a sentence from a progress meter in
//! the middle of a compressed object.
//!
//! # The taxonomy is already in `pktline`
//!
//! [`crate::remote::SidebandKind`], [`crate::remote::SidebandPacket`] and
//! [`crate::remote::SidebandReader`] live in [`crate::remote::pktline`] because they are a
//! pkt-line-level concern: distinguishing a control packet from a data packet and a pack byte from a
//! progress byte are the same parse, and splitting them across two modules would mean two readers of
//! one stream. This module is the layer above — it takes a demultiplexed channel 1 and turns it into
//! something the pack layer accepts, and it is where the progress sink and the error channel are
//! given their behaviour.
//!
//! # Caveats that bite
//!
//! * **Channel 2 must be read even when nobody wants it.** The server writes progress into the same
//!   pipe as the pack; a client that stopped reading channel 2 would fill the pipe buffer, block the
//!   server mid-pack and deadlock with both sides waiting. A `None` sink therefore means "read and
//!   discard", never "do not read", and every function here enforces that by taking the sink rather
//!   than being callable without one.
//! * **Channel 3 ends the exchange** and the megabytes of pack already received are to be discarded,
//!   not indexed. A server that hits its own limit mid-transfer says so here, and keeping the partial
//!   pack would put a truncated object in the database that no checksum check would catch until it was
//!   read back.
//! * **The error text is the only diagnostic and it is not a pkt-line of its own after the first
//!   byte**; the rest of that packet's payload is the message, and a multi-line message arrives as
//!   several channel 3 packets that must be joined rather than reported one at a time.
//! * **A packet whose payload is empty is legal and means nothing happened.** A demultiplexer that
//!   treated an empty payload as end-of-stream, or indexed it as a zero-length chunk, breaks on a
//!   server that sends one as a keepalive — which is what a slow negotiation produces.
//! * **`side-band` and `side-band-64k` differ only in the maximum payload**, 999 versus 65516 bytes.
//!   The reader is written against the pkt-line maximum and so handles both; a caller that
//!   implemented the smaller limit as the framing rule would reject a legal 64k pack chunk.
//!
//! # What this module does not do
//!
//! It does not write to the object database. Demultiplexing produces a pack; deciding whether it is
//! thin, fixing it, indexing it and adding it to the odb are [`crate::pack`]'s and [`crate::odb`]'s
//! jobs, and the seam between them is a `Vec<u8>` — deliberately, because the received pack's size is
//! unbounded and the caller is the party that knows whether it may be held in memory.

use std::io::{Read, Write};

use crate::error::Result;
use crate::hash::ObjectFormat;
use crate::odb::Odb;
use crate::pack::{IndexPackResult, PackWriteSummary};

use crate::remote::ProtocolVersion;
use crate::remote::pktline::SidebandKind;

/// Phase 11 — the channels a caller may be offered, and which of them it wants.
///
/// # Caveats
///
/// * **This is a policy object, not a switch.** The transport always demultiplexes all three
///   channels; this says what the caller does with each, and the only one that can be disabled is
///   progress — which becomes a discard rather than a skip. A caller that tried to disable channel 1
///   would be asking to not receive its objects.
/// * **The error channel cannot be disabled and is not a preference**, so it has no field. Making it
///   configurable would invite a caller to ignore the one message that explains a failed transfer.
#[derive(Clone, Debug)]
pub struct SidebandOptions {
    /// Whether the underlying stream is sideband-framed at all. When false the reader passes the
    /// stream through, which is what a server without `side-band-64k` does.
    pub enabled: bool,
    /// The maximum data bytes in one sideband payload, from the negotiated capability: 65516 for
    /// `side-band-64k`, 999 for the older `side-band`.
    pub max_payload: usize,
    /// Whether progress on channel 2 should be forwarded to the sink or read and dropped.
    pub want_progress: bool,
    /// The protocol generation, which decides whether a response-end packet is expected after the
    /// pack. A v0 stream ends at the flush.
    pub version: ProtocolVersion,
}

impl Default for SidebandOptions {
    /// Phase 11 — `side-band-64k` with progress wanted, which is what a fetch over a capable server
    /// negotiates.
    ///
    /// # Caveats
    ///
    /// **`enabled` is true**, because a caller that constructed the options and forgot the field is
    /// far more likely to be running under the negotiated capability than outside it. A caller
    /// reading a raw stream sets it to false explicitly, and the field is public for that reason.
    fn default() -> Self {
        Self {
            enabled: true,
            max_payload: 65516,
            want_progress: true,
            version: ProtocolVersion::V2,
        }
    }
}

/// Phase 11 — read a sideband-framed stream, forwarding progress and returning the pack bytes.
///
/// # Caveats
///
/// * **The whole pack is returned in memory.** There is no size negotiation in the protocol and the
///   server may send a repository-sized pack, so a caller that cannot bound the transfer uses
///   [`stream_pack`] instead. This is the convenience for the common case and it is not the one a
///   clone of a large repository should go through.
/// * **`progress` is called with the channel 2 payloads in arrival order and with no buffering**. The
///   server's progress is not line-aligned: `Counting objects: 47%` may arrive split across two
///   packets, and joining them is the sink's job, not this function's — a sink that assumed one
///   packet per line prints a torn line at every packet boundary, which is most of them.
/// * **A channel 3 packet aborts with an error and discards what was read**, per the module caveat.
///   The error is [`crate::error::Error::Protocol`] carrying the server's text, and it is the only
///   place that text survives — the stream is closed afterwards.
/// * **`None` for the sink still reads channel 2**, which is not an optimisation to skip: see the
///   module caveat on the deadlock it causes.
/// * **An end-of-stream before the terminating flush is [`crate::error::Error::Protocol`]**, because
///   a truncated pack is indistinguishable from a complete one by inspection. The caller must not be
///   given a short pack and told it is fine.
///
/// # Errors
///
/// [`crate::error::Error::Protocol`] for an unknown channel, a channel 3 abort or a stream that ends
/// without its terminator; and whatever `reader` raises.
pub fn demultiplex<R: Read>(
    reader: &mut R,
    options: &SidebandOptions,
    progress: Option<&mut dyn FnMut(&[u8])>,
) -> Result<Vec<u8>> {
    todo!()
}

/// Phase 11 — read a sideband-framed stream, writing the pack bytes straight into `out`.
///
/// # Caveats
///
/// * **The pack is never held whole**, which is the point: this is the path a clone should take, and
///   the pack is handed to whatever the caller passed — a file, a hasher, a fixer. The caller is
///   responsible for the fact that nothing can be validated until it has been written, so a channel 3
///   abort leaves a partial file for the caller to remove, and this function's error says so by being
///   an error rather than a returned length.
/// * **The progress sink has the same contract as [`demultiplex`]'s**, including that it is called on
///   this thread and must not block indefinitely: the server is blocked while it runs.
/// * **`out` is written in channel-1 payload chunks**, which are not aligned to any pack structure.
///   A caller that needs object boundaries uses [`store_pack`], which goes through the pack writer.
/// * **A flush ends the stream only on channel 0**, and the packets before it are the entire pack. A
///   reader that treated a flush inside the pack as a terminator would stop at the first one, which
///   for a compressed pack body is a coincidence that happens within the first few kilobytes.
///
/// # Errors
///
/// As [`demultiplex`], plus whatever `out` raises.
pub fn stream_pack<R: Read, W: Write>(
    reader: &mut R,
    options: &SidebandOptions,
    out: &mut W,
    progress: Option<&mut dyn FnMut(&[u8])>,
) -> Result<usize> {
    todo!()
}

/// Phase 11 — demultiplex a fetch response and store the pack in the object database.
///
/// # Caveats
///
/// * **The thin-pack fix happens before the index**, and a pack that needs it is detected from the
///   index attempt rather than from the capability. A server is allowed to send a thin pack whenever
///   it believes the client has the bases, even if `thin-pack` was the caller's idea, so the client
///   cannot assume a non-thin pack and a client that did gets
///   [`crate::error::Error::ThinPack`] from the index rather than corrupt objects.
/// * **Nothing is added to the odb on a checksum failure.** The pack's own trailing checksum and the
///   index's are both verified by [`crate::pack::index_pack`], and a mismatch means the transfer was
///   corrupted — writing it anyway would put objects in the database that can never be read back
///   correctly, which is worse than losing the fetch.
/// * **The index is stored alongside the pack**, which is what makes the objects readable without
///   rebuilding it. A caller that stored the pack alone has objects that only a full repository
///   reindex would find, and the difference is invisible until someone runs `fsck`.
/// * **The return is the index result rather than the pack**, because the caller wants the object
///   count and the checksum for its report and does not want a second copy of the bytes. It is
///   [`IndexPackResult`] as the pack layer defines it, unchanged — this function adds no vocabulary of
///   its own, deliberately, so that a caller holding a pack from anywhere handles it the same way.
/// * **A `side-band`-less server is handled by the same call**, with [`SidebandOptions::enabled`]
///   false, so the caller does not branch on the capability. The branch is in the reader, once.
///
/// # Errors
///
/// [`crate::error::Error::Protocol`] for the framing, [`crate::error::Error::ThinPack`] if a thin pack
/// cannot be completed against the local odb, [`crate::error::Error::CorruptPack`] for a corrupt pack, and
/// [`crate::error::Error::Io`] if the result cannot be written.
pub fn store_pack<R: Read>(
    reader: &mut R,
    options: &SidebandOptions,
    odb: &Odb,
    format: ObjectFormat,
    progress: Option<&mut dyn FnMut(&[u8])>,
) -> Result<IndexPackResult> {
    todo!()
}

/// Phase 11 — read a sideband-framed exchange that carries no pack, as a push's status does.
///
/// # Caveats
///
/// * **Channel 1 here is not a pack.** On a push response the same channel carries the
///   `report-status` pkt-lines, and a caller that fed them to a pack reader would report a corrupt
///   pack. This function returns the channel 1 bytes for [`crate::remote::push::parse_report_status`]
///   and is separate from [`demultiplex`] for exactly that reason.
/// * **The status lines inside channel 1 may themselves be pkt-framed *or* raw** depending on the
///   server, and Git accepts both. The caller parses the result with the same function either way,
///   because that parser works on the status vocabulary rather than on the framing — which is why it
///   can be given a buffer that has already been demultiplexed.
/// * **Progress is forwarded as it arrives, before the status is complete.** A push's hook output is
///   the usual reason a push is slow, and a sink that waited for the status would show the user
///   nothing until it was over.
/// * **An empty channel 1 means no status was reported**, which is not the same as an empty status:
///   the caller distinguishes them with [`crate::remote::PushOutcome::status_reported`], and this
///   function returns the empty buffer rather than inventing a status.
///
/// # Errors
///
/// As [`demultiplex`], minus the pack-specific cases.
pub fn read_status_stream<R: Read>(
    reader: &mut R,
    options: &SidebandOptions,
    progress: Option<&mut dyn FnMut(&[u8])>,
) -> Result<Vec<u8>> {
    todo!()
}

/// Phase 11 — frame `payload` for writing on a channel, for the client's own sideband writes.
///
/// # Caveats
///
/// * **The client rarely needs this**, because a fetch request and a push's command lines are written
///   unframed and only the *pack* a client sends in a push is sideband-wrapped, and only when the
///   server negotiated it. It exists so that the one case does not need its own pkt-line code, and a
///   caller that used it for the command lines would produce a server that reads a channel byte as the
///   start of an object id.
/// * **The payload is split at `max_payload`**, which must be the negotiated limit rather than the
///   pkt-line maximum: a server that offered `side-band` and receives a 65516-byte payload has been
///   sent a frame larger than it agreed to read, and its behaviour is unspecified.
/// * **An empty payload produces no packet**, because a zero-length data packet is a legal keepalive
///   and emitting one for an empty write would put a spurious packet in the middle of a pack.
///
/// # Errors
///
/// [`crate::error::Error::Protocol`] if `max_payload` exceeds the pkt-line maximum, which is a caller
/// bug that would otherwise produce a frame no peer can parse.
pub fn frame(params: ChannelWrite, payload: &[u8]) -> Result<Vec<u8>> {
    todo!()
}

/// Phase 11 — the parameters of a client-side sideband write.
///
/// # Caveats
///
/// * **`channel` is always [`SidebandKind::Pack`] in practice** and the field exists so that the
///   function is not lying about what the protocol allows. A client writing progress to a server is
///   legal and no server acts on it.
/// * **`max_payload` comes from the negotiation**, not from a constant; see [`frame`]'s caveat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelWrite {
    /// The channel to write on.
    pub channel: SidebandKind,
    /// The negotiated maximum data bytes per packet.
    pub max_payload: usize,
}

/// Phase 11 — copy `bytes` from one writer to another through a sideband framing.
///
/// # Caveats
///
/// **This is the streaming counterpart of [`frame`]**, used to send a push's pack without holding the
/// framing of the whole pack in memory: the pack is read in `max_payload` chunks and each becomes a
/// packet as it is written. A caller that used [`frame`] on a large pack got a `Vec` twice the pack's
/// size, which is the difference between pushing a large repository and running out of memory.
///
/// # Errors
///
/// [`crate::error::Error::Protocol`] for a bad `max_payload`, and whatever either stream raises.
pub fn write_framed<R: Read, W: Write>(
    input: &mut R,
    out: &mut W,
    params: ChannelWrite,
) -> Result<u64> {
    todo!()
}

/// Phase 11 — where the pack bytes of a fetch should go, for a caller that wants the choice.
///
/// # Caveats
///
/// * **[`PackSink::Memory`] is the default for a small transfer and the wrong choice for a large
///   one**, which is a judgement this enum cannot make for the caller. The plan's rule is that the
///   clone path uses [`PackSink::Odb`] and the tests use memory; a caller that does not know the
///   repository's size uses the odb.
/// * **[`PackSink::Odb`] does not write to the odb itself** — it names the intent, and
///   [`store_pack`] is what performs it. The variant carries the odb so that a caller does not have
///   to thread it separately, and cannot accidentally name an odb it did not mean.
/// * **A `PackSink::File` variant is not here.** Writing a pack to a path is `git index-pack --stdin`'s
///   behaviour and Rivet has no such command; adding it would be a durability feature with its own
///   crash semantics, and a variant that existed and was unimplemented would be worse than none.
pub enum PackSink<'a> {
    /// Collect the pack into a `Vec`, for a caller that will index it itself.
    Memory,
    /// Fix, index and add the pack to this object database.
    Odb(&'a Odb),
}

/// Phase 11 — the summary of a pack that was stored, as the caller reports it.
///
/// # Caveats
///
/// * **It is [`PackWriteSummary`]'s shape rather than a new one**, so that a pack Rivet built locally
///   and a pack it received are described by the same type — which is the whole reason the pack layer
///   defined it. The `index` field is the serialised index, present in both cases and written by the
///   same code.
/// * **`entry_count` counts objects, not deltas**, so a thin pack that was fixed reports more entries
///   than the server sent. That is the correct number for a user asking how many objects arrived, and
///   a caller comparing it against the advertisement's count will find it larger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPack {
    /// The pack's own checksum, from its trailer.
    pub checksum: crate::hash::ObjectId,
    /// How many objects the pack contains.
    pub entry_count: u32,
    /// The serialised index for the pack.
    pub index: Vec<u8>,
}

impl From<PackWriteSummary> for StoredPack {
    /// Phase 11 — the identity conversion, so that a locally-built pack and a received one are
    /// described by one type without a wrapper.
    ///
    /// # Caveats
    ///
    /// **It is a real conversion rather than a type alias because the two are conceptually different
    /// packs** — one written by Rivet, one written by a peer — and giving them one name would make a
    /// signature that takes either ambiguous to a reader. The representation being identical is the
    /// happy accident that makes the conversion free, not the reason for the type.
    fn from(summary: PackWriteSummary) -> Self {
        todo!()
    }
}
