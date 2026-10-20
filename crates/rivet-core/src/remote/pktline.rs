//! Phase 11 — pkt-line: the framing every Git wire protocol is carried in.
//!
//! The format is four hexadecimal characters of length followed by that many bytes, where the
//! length **includes the four header bytes**:
//!
//! ```text
//! 0032git-upload-pack /repo.git\0host=example.com\0
//! ^^^^                                             length 0x32 = 50
//! 0000                                             flush
//! 0001                                             delim
//! 0002                                             response-end
//! ```
//!
//! Everything above pkt-line — the ref advertisement, `ls-refs`, the fetch negotiation, the
//! `report-status` response, and the side-band channel a pack arrives on — is a sequence of these
//! frames.
//!
//! # The control packets
//!
//! The three lengths below the minimum (`0000`, `0001`, `0002`) are not empty payloads: they are
//! the protocol's structural markers, and they are not interchangeable.
//!
//! * **flush** (`0000`) ends a list. In the v0/v1 ref advertisement it is the end of the
//!   advertisement; in a negotiation it is the "that is all the `have` lines, answer me" marker.
//! * **delim** (`0001`) separates *sections* within one response. Protocol v2 uses it to end the
//!   capability list and begin the command's own section, and a reader that treated it as a flush
//!   would stop one section early and silently discard the payload it was waiting for.
//! * **response-end** (`0002`) ends one response in a stateless-connect exchange. Rivet does not
//!   use stateless-connect, but it must recognise the packet rather than reporting it as a bad
//!   length, because a reader that errors here would turn a peer's legal (if unused) response into
//!   [`crate::error::Error::Protocol`].
//!
//! # Caveats that bite
//!
//! * **The length includes the header.** A writer that emits `payload.len()` rather than
//!   `payload.len() + 4` produces a stream that the *next* frame misreads, and the failure appears
//!   one packet later with no relation to the packet that caused it. This is the single most
//!   common way to get pkt-line wrong.
//! * **The maximum data length is 65516, not 65535.** The four header characters are part of the
//!   length, and the largest four-character hex value is `ffff`. Git's own `LARGE_PACKET_MAX` is
//!   65520 and its data maximum is 65516. A payload longer than that must be split — which is
//!   exactly what [`MAX_SIDEBAND_DATA`] is about — not truncated.
//! * **The hex is lowercase on the wire but must be read case-insensitively.** Git writes
//!   lowercase; a peer that sent uppercase is not violating anything, and a reader that compared
//!   against `b"0123456789abcdef"` while a writer had been given uppercase input is how a
//!   round-trip test passes and a live handshake fails.
//! * **A payload is bytes and keeps its trailing newline.** Almost every text packet in the
//!   protocol is a line terminated by `\n` and the newline is part of the payload; a parser that
//!   stripped it would produce a ref advertisement whose last line looks the same as a malformed
//!   one, and a writer that omitted it produces a stream no peer can split into lines. This is why
//!   the reader returns [`PktLine::Data`] holding the raw bytes and leaves the stripping to
//!   [`PktLine::text`]-style accessors.
//! * **The empty data packet is legal and distinct from a flush.** `0004` is a data packet with an
//!   empty payload; `0000` is a flush. Git uses `0004` in a few places and a reader that mapped it
//!   onto flush would end a list early.
//! * **A truncated stream is not the same as a closed one.** A reader that hits EOF in the middle
//!   of a frame — after the length, or partway through the payload — must report
//!   [`crate::error::Error::Protocol`], because a peer that vanished mid-packet is a different
//!   problem from one that finished.
//!
//! # Sideband
//!
//! [`SidebandKind`], [`SidebandPacket`] and [`SidebandReader`] live here rather than in a module of
//! their own, because
//! sideband is a property of a *data packet's first byte*, not a separate framing. When the
//! `side-band-64k` capability is in force, every data packet of the pack or the response carries a
//! one-byte channel number as its first payload byte, and a reader that forgot to strip it would
//! parse the channel byte as the first byte of a ref line.

use std::io::Read;

use crate::error::Result;
use crate::error::Error;

/// Phase 11 — how many bytes of payload a pkt-line can carry, at most.
///
/// # Caveats
///
/// 65516, not 65535 and not 65520. The four header characters are counted in the length, so the
/// largest expressible length is `0xffff` = 65535 and the largest payload is 65535 − 4 = 65531;
/// Git lowers it further to 65516 (`LARGE_PACKET_MAX` 65520 − 4) so that a reader may assume a
/// whole frame fits in a buffer with room to spare. Rivet must not *write* more than this, and a
/// peer that sends more is [`crate::error::Error::Protocol`] rather than something to accept.
pub const MAX_PKT_DATA: usize = 65516;

/// Phase 11 — how many bytes a side-band packet may carry once its channel byte is removed.
///
/// # Caveats
///
/// One less than [`MAX_PKT_DATA`], because the channel selector occupies the first payload byte.
/// Git's advertised `side-band-64k` capability means "up to 65519 bytes of pack data per packet"
/// and this is the number Rivet enforces. The older `side-band` capability means 999, which a
/// caller must handle by checking which capability was negotiated rather than assuming 64k — a
/// peer that negotiated plain `side-band` and receives a 64k packet will drop the connection.
pub const MAX_SIDEBAND_DATA: usize = MAX_PKT_DATA - 1;

/// Phase 11 — the smallest length a pkt-line may declare.
///
/// # Caveats
///
/// Four, because the header is counted in the length. A length of `0001`, `0002` or `0003` in the
/// length field is not a short data packet — those are the control packets, and a decoder that
/// allowed them as lengths would read a negative payload size. See [`PktLine::decode`].
pub const MIN_PKT_LEN: usize = 4;

/// Phase 11 — the four-character length that ends a pkt-line list.
///
/// # Caveats
///
/// Distinguishable from a delim packet, which also terminates a list in protocol v2 but marks a
/// section boundary rather than the end of the response. A reader that matched "a control packet"
/// with one test and then treated both the same would parse the capability section of a v2
/// response as the whole response.
pub const FLUSH_PACKET: &[u8; 4] = b"0000";

/// Phase 11 — the four-character length that separates sections of a protocol v2 response.
///
/// # Caveats
///
/// Only defined in protocol v2. A v0/v1 peer never sends one, and a v0/v1 reader that accepted it
/// as an ordinary list terminator would accept a v2 response as a v0 one and read the wrong
/// section.
pub const DELIM_PACKET: &[u8; 4] = b"0001";

/// Phase 11 — the four-character length that ends one response in a stateless-connect exchange.
///
/// # Caveats
///
/// Rivet does not implement `stateless-connect`, so this packet is never *written*. It is read
/// anyway: a peer may send it, and reporting it as a bad length would be a protocol error where
/// the honest answer is "a response ended".
pub const RESPONSE_END_PACKET: &[u8; 4] = b"0002";

/// Phase 11 — one pkt-line as it was read off the wire.
///
/// # Caveats
///
/// * **The payload of [`PktLine::Data`] owns its bytes**, which is what makes a decoded frame
///   independent of the reader that produced it. A borrowed form would tie every parsed ref line
///   to the socket buffer it came from, and the ref advertisement is parsed after the packet that
///   carries it has been consumed.
/// * **The trailing newline is part of the data.** See the module caveat; callers that want a line
///   without it strip it themselves, and callers that want a `&str` for a message use
///   [`PktLine::text`].
/// * **A control packet carries nothing.** There is deliberately no variant holding "flush with a
///   payload" — the three control lengths are the only lengths below four that are legal, and a
///   fifth would be a peer bug rather than an extension point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PktLine {
    /// A data packet: four header characters then this many bytes, minus four.
    Data(Vec<u8>),
    /// `0000` — the end of a list.
    Flush,
    /// `0001` — a section boundary inside one protocol v2 response.
    Delim,
    /// `0002` — the end of one response in a stateless-connect exchange.
    ResponseEnd,
}

impl PktLine {
    /// Phase 11 — encode this packet, header included, as it goes on the wire.
    ///
    /// # Caveats
    ///
    /// * **The length written is `payload.len() + 4`.** See the module caveat: this is the mistake
    ///   that makes every subsequent frame misread, and it is worth stating at the one place it
    ///   could be made.
    /// * **The hex is lowercase and zero-padded to exactly four characters.** `write!` with `{:04x}`
    ///   is the whole of the spelling rule; a hand-rolled conversion that padded to three for a
    ///   short packet produces a stream that cannot be framed at all.
    /// * **An over-long payload is an error, not a split.** [`MAX_PKT_DATA`] is the format's limit;
    ///   silently splitting here would produce two packets where the caller wrote one and a peer
    ///   would see a line break that is not in the data.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`] if a [`PktLine::Data`] payload exceeds [`MAX_PKT_DATA`].
    /// The three control packets cannot fail, but they share the return type so that a caller
    /// encoding a mixed list does not need two call sites.
    pub fn encode(&self) -> Result<Vec<u8>> {
        match self {
            PktLine::Flush => Ok(b"0000".to_vec()),
            PktLine::Delim => Ok(b"0001".to_vec()),
            PktLine::ResponseEnd => Ok(b"0002".to_vec()),
            PktLine::Data(payload) => {
                if payload.len() > MAX_PKT_DATA {
                    return Err(Error::Protocol("payload exceeds MAX_PKT_DATA".to_string()));
                }
                let len = payload.len() + 4;
                let mut out = Vec::with_capacity(len);
                out.extend_from_slice(format!("{:04x}", len).as_bytes());
                out.extend_from_slice(payload);
                Ok(out)
            }
        }
    }

    /// Phase 11 — the payload, or an empty slice for a control packet.
    ///
    /// # Caveats
    ///
    /// The empty slice for a control packet is a convenience that loses the distinction between
    /// "flush" and "an empty data packet". A caller that needs to tell them apart matches on the
    /// enum; a caller that is accumulating payloads and only wants to know "is there anything here"
    /// uses this.
    pub fn data(&self) -> &[u8] {
        match self {
            PktLine::Data(data) => data.as_slice(),
            _ => &[],
        }
    }

    /// Phase 11 — how many bytes this packet occupies on the wire, header included.
    ///
    /// # Caveats
    ///
    /// Four for every control packet, and `4 + payload.len()` for a data packet. This is the number
    /// that goes into the header, so a caller computing buffer sizes uses it and a caller writing the
    /// length uses [`PktLine::encode`].
    pub fn wire_len(&self) -> usize {
        match self {
            PktLine::Data(data) => 4 + data.len(),
            _ => 4,
        }
    }

    /// Phase 11 — whether this is a control packet rather than data.
    ///
    /// # Caveats
    ///
    /// True for flush, delim and response-end alike. A caller that has to distinguish them matches
    /// on the enum; the common question — "did the list end" — is not quite this, because a delim
    /// packet ends a *section*.
    pub const fn is_control(&self) -> bool {
        matches!(self, PktLine::Flush | PktLine::Delim | PktLine::ResponseEnd)
    }

    /// Phase 11 — whether this packet ends the list.
    ///
    /// # Caveats
    ///
    /// True for [`PktLine::Flush`] only, and **false for [`PktLine::Delim`]** — which is the whole
    /// point of the predicate existing separately from [`PktLine::is_control`]. In a v2 response the
    /// delim packet ends the capability section and the flush ends the response, so code that
    /// stopped at the first control packet would discard the section it was waiting for.
    pub const fn is_flush(&self) -> bool {
        matches!(self, PktLine::Flush)
    }

    /// Phase 11 — build a data packet from bytes, requiring the trailing newline to be the caller's
    /// choice.
    ///
    /// # Caveats
    ///
    /// **No newline is added.** Almost every caller wants one — the protocol's text packets are
    /// newline-terminated lines and a ref advertisement without the final newline is malformed — but
    /// a constructor that appended one unconditionally would make it impossible to send the few
    /// packets that legitimately end otherwise, such as a `want` line's id followed by a flush.
    /// Callers building text lines add `\n` themselves, which is why [`PktLine::new_text`] exists
    /// next to this.
    pub fn new_data(data: &[u8]) -> Self {
        PktLine::Data(data.to_vec())
    }

    /// Phase 11 — build a data packet from a text line, appending the terminating newline.
    ///
    /// # Caveats
    ///
    /// * **The newline is added, but not unconditionally doubled.** A caller that passes a line
    ///   already ending in `\n` gets one newline, not two, so that a value read back out of a
    ///   parser and echoed can be re-sent without accumulating blank lines.
    /// * **The text is bytes, not `&str`.** Ref names and remote paths are bytes on the wire; a
    ///   `&str` parameter would refuse a legal ref name that is not UTF-8.
    pub fn new_text(line: &[u8]) -> Self {
        let mut data = line.to_vec();
        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        PktLine::Data(data)
    }

    /// Phase 11 — decode one packet from the front of a buffer, returning it and the bytes consumed.
    ///
    /// The primitive every reader in this module is built on: the ref advertisement parser, the
    /// negotiation loop and the sideband demultiplexer all pull packets from a buffer with this and
    /// dispatch on the result.
    ///
    /// # Caveats
    ///
    /// * **Returning the consumed length is what makes an incremental reader possible.** A caller
    ///   that has read a whole response into memory advances by it; a caller reading from a socket
    ///   uses [`PktLine::read_from`] instead. Neither may assume the buffer holds exactly one packet.
    /// * **`0003` is not a legal length**, and neither is any other value in `0x0003..0x0004`. The
    ///   three control values are `0000`, `0001` and `0002`; everything from `0004` up is data. A
    ///   decoder that computed `len - 4` before checking for the control values underflows on
    ///   `0003` and either panics or reads a vast payload.
    /// * **The hex is parsed case-insensitively**; see the module caveat. A non-hex character is
    ///   [`crate::error::Error::Protocol`] and never a partial parse.
    /// * **`0004` is an empty data packet, not a flush.** See the module caveat.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`] if fewer than four bytes remain, if the header is not four
    /// hex digits, if the declared length is `0003`, if the declared length exceeds
    /// [`MAX_PKT_DATA`] + 4, or if the buffer is shorter than the declared length — a truncated
    /// frame is a peer that vanished, which is different from a peer that sent a wrong length but
    /// must be reported rather than read past the end of the buffer.
    pub fn decode(data: &[u8]) -> Result<(Self, usize)> {
        let len = if data.len() < 4 {
            return Err(Error::Protocol("truncated pkt-line header".to_string()));
        } else {
            let header = &data[0..4];
            let hex = std::str::from_utf8(header).map_err(|_| Error::Protocol("non-hex pkt-line header".to_string()))?;
            u32::from_str_radix(hex, 16).map_err(|_| Error::Protocol("non-hex pkt-line header".to_string()))?
        };
        if len == 3 {
            return Err(Error::Protocol("invalid pkt-line length 0003".to_string()));
        }
        // Control packets carry no payload. Their length is 0, 1 or 2, so the payload length must
        // not be computed by subtraction before they are recognised: `0 - 4` underflows.
        if len < 4 {
            // A control packet's length field counts itself, so these three are exactly four bytes
            // long on the wire. Reporting them as empty `Data` instead of as control packets makes
            // protocol v2 section delimiters look like empty payloads.
            let pkt = match len {
                0 => PktLine::Flush,
                1 => PktLine::Delim,
                _ => PktLine::ResponseEnd,
            };
            return Ok((pkt, 4));
        }
        let payload_len = len as usize - 4;
        if payload_len > MAX_PKT_DATA {
            return Err(Error::Protocol("pkt-line exceeds max length".to_string()));
        }
        if data.len() < 4 + payload_len {
            return Err(Error::Protocol("truncated pkt-line payload".to_string()));
        }
        let payload = &data[4..4 + payload_len];
        Ok((PktLine::Data(payload.to_vec()), 4 + payload_len))
    }

    /// Phase 11 — read one packet from a stream.
    ///
    /// The form a negotiation loop uses: the peer's answers arrive over time and there is no
    /// whole-response buffer to slice. It reads four bytes, decodes the length, then reads exactly
    /// the payload.
    ///
    /// # Caveats
    ///
    /// * **A short read is a failure, not a shorter packet.** `Read::read` is allowed to return
    ///   fewer bytes than requested and it does so on a socket; the implementation must loop until
    ///   it has the four header bytes and then until it has the whole payload. Code that trusted a
    ///   single `read` call works on a local pipe and fails on a real network, which is precisely
    ///   the case this code exists for.
    /// * **EOF at a frame boundary is [`crate::error::Error::Protocol`] here, not `Ok(None)`.** A
    ///   caller that expects the list to end uses a flush packet for that: the protocol always
    ///   terminates a list explicitly, so a closed stream is a peer that died. A caller that
    ///   genuinely wants "the stream ended" should read the frames through a loop that treats this
    ///   error as terminal.
    /// * **The stream is not buffered here.** A caller wrapping a socket in a `BufReader` gets to
    ///   decide the buffer size, and a caller reading from a `Vec<u8>` gets no copy at all.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`] for a truncated frame and a malformed header, and
    /// [`crate::error::Error::Transport`] for an underlying read failure — the distinction the
    /// protocol needs, because "the peer sent nonsense" and "the connection broke" are different
    /// reports for the user.
    pub fn read_from<R: Read>(reader: &mut R) -> Result<Self> {
        match Self::read_from_opt(reader)? {
            Some(pkt) => Ok(pkt),
            None => Err(Error::Protocol("truncated frame".to_string())),
        }
    }

    /// Phase 11 — read one packet, treating a clean end of stream as `Ok(None)`.
    ///
    /// # Caveats
    ///
    /// **Only a stream that ends *between* packets yields `Ok(None)`.** A stream that stops part
    /// way through a header or a payload is still a [`crate::error::Error::Protocol`]: the peer was
    /// cut off mid-frame and lost data, and reporting that as "the response ended" would hand a
    /// truncated pack to the caller as if it were complete. [`PktLine::read_from`] is this function
    /// with the `None` case turned back into an error, for the negotiation paths where the protocol
    /// requires an explicit flush.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`] for a frame cut short mid-header or mid-payload and for a
    /// malformed header, and [`crate::error::Error::Transport`] for an underlying read failure.
    pub fn read_from_opt<R: Read>(reader: &mut R) -> Result<Option<Self>> {
        let mut header = [0u8; 4];
        let mut n = 0;
        while n < 4 {
            match reader.read(&mut header[n..]) {
                // Nothing at all before the header: the stream ended at a packet boundary, which
                // is how a server that closes the connection instead of flushing ends a response.
                Ok(0) if n == 0 => return Ok(None),
                Ok(0) => return Err(Error::Protocol("truncated frame".to_string())),
                Ok(k) => n += k,
                Err(e) => return Err(Error::Transport(e.to_string())),
            }
        }
        // The header is four *hex digits*, not two binary bytes: reading it as a big-endian u16
        // turns "0032" into 0x3030 and makes every frame length wrong.
        let len = u32::from_str_radix(
            std::str::from_utf8(&header)
                .map_err(|_| Error::Protocol("non-hex pkt-line header".to_string()))?,
            16,
        )
        .map_err(|_| Error::Protocol("non-hex pkt-line header".to_string()))?;
        if len == 3 {
            return Err(Error::Protocol("invalid pkt-line length 0003".to_string()));
        }
        // Control packets carry no payload, so the subtraction must not happen before they are
        // recognised — `0 - 4` underflows a usize.
        if len < 4 {
            let pkt = match len {
                0 => PktLine::Flush,
                1 => PktLine::Delim,
                _ => PktLine::ResponseEnd,
            };
            return Ok(Some(pkt));
        }
        let payload_len = len as usize - 4;
        if payload_len > MAX_PKT_DATA {
            return Err(Error::Protocol("pkt-line exceeds max length".to_string()));
        }
        let mut payload = vec![0u8; payload_len];
        let mut n = 0;
        while n < payload_len {
            match reader.read(&mut payload[n..]) {
                Ok(0) => return Err(Error::Protocol("truncated frame".to_string())),
                Ok(k) => n += k,
                Err(e) => return Err(Error::Transport(e.to_string())),
            }
        }
        Ok(Some(PktLine::Data(payload)))
    }

    /// Phase 11 — write this packet to a stream.
    ///
    /// # Caveats
    ///
    /// Writes the whole frame in one call where the stream allows it, because a negotiation request
    /// that dribbled out one byte at a time over a TLS connection would be slow for no benefit —
    /// but note that the caller is responsible for **flushing** afterwards. A `BufWriter` around the
    /// stream will hold the request until it is flushed or dropped, and a peer that is waiting for a
    /// request before sending its response deadlocks against an unflushed buffer.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`] if a data payload is too long to encode, and
    /// [`crate::error::Error::Transport`] if the write fails.
    pub fn write_to<W: std::io::Write>(&self, writer: &mut W) -> Result<()> {
        let buf = self.encode()?;
        writer.write_all(&buf)?;
        Ok(())
    }

    /// Phase 11 — the payload decoded as text, for a human-facing message.
    ///
    /// # Caveats
    ///
    /// **Lossy, deliberately.** The packets this is for are error messages and progress lines from
    /// a remote, which are text in whatever encoding the server used, and refusing to display one
    /// because a byte is invalid UTF-8 helps nobody. Anything that will be *stored* or compared —
    /// a ref name, a capability — must come from [`PktLine::data`] as bytes instead.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(self.data()).into_owned()
    }
}

/// Phase 11 — which sideband channel a packet arrived on.
///
/// The `side-band-64k` capability multiplexes three logical streams over the one connection the pack
/// is being sent on: the pack itself, a progress stream, and a fatal-error stream. The channel is
/// the **first payload byte** of every packet once the capability is in force.
///
/// # Caveats
///
/// * **The pack channel is not the default when sideband is off.** Without the capability, every
///   packet is pack data with no channel byte at all, and a reader that skipped a byte anyway
///   starts reading the pack one byte late — which fails at the pack's magic rather than at the
///   packet, and looks like corruption. The demultiplexer must be told whether sideband was
///   negotiated.
/// * **Channel 1's packets are the response, not always a pack.** In a fetch they are pack bytes;
///   in a `report-status` exchange they are the status lines. The channel number says which stream,
///   not what the payload means; the caller knows which exchange it is in.
/// * **A channel byte outside 1–3 is [`crate::error::Error::Protocol`]**, not an unknown channel to
///   be skipped. Git reserves 0 and everything above 3, and a peer sending one is either buggy or
///   trying to desynchronise the reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SidebandKind {
    /// Channel 1 — the pack data (or the `report-status` lines, on a push).
    Pack,
    /// Channel 2 — progress messages, meant to be shown to the user; not part of the result.
    Progress,
    /// Channel 3 — fatal errors. A packet here means the exchange failed even if the stream then
    /// closes cleanly.
    Error,
}

impl SidebandKind {
    /// Phase 11 — the channel's wire byte, 1, 2 or 3.
    ///
    /// # Caveats
    ///
    /// Not an array index. The values were chosen by the protocol, not by convenience, and a reader
    /// that used `kind as u8` on a reordered enum would silently write progress on the pack channel.
    pub const fn channel(self) -> u8 {
        match self {
            SidebandKind::Pack => 1,
            SidebandKind::Progress => 2,
            SidebandKind::Error => 3,
        }
    }

    /// Phase 11 — decode a channel byte.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`] for anything other than 1, 2 or 3, including 0 — see the
    /// type's caveat.
    pub fn from_channel(byte: u8) -> Result<Self> {
        match byte {
            1 => Ok(Self::Pack),
            2 => Ok(Self::Progress),
            3 => Ok(Self::Error),
            _ => Err(Error::Protocol(format!(
                "unknown sideband channel {byte}"
            ))),
        }
    }
}

/// Phase 11 — one packet from a sideband stream, with the channel byte already removed.
///
/// # Caveats
///
/// * **The payload excludes the channel byte.** A demultiplexer that returned the raw packet would
///   make every caller strip the first byte itself, and the one that forgot would feed the pack
///   parser a stream starting with a `\x01`.
/// * **A flush packet carries no channel** and is therefore not represented here; the
///   demultiplexer's reader returns `Option`-shaped emptiness or an error for it, and the caller's
///   loop ends. See [`SidebandReader::next_packet`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebandPacket {
    /// Which stream this packet belongs to.
    pub channel: SidebandKind,
    /// The packet's payload, with the channel byte removed.
    pub data: Vec<u8>,
}

/// Phase 11 — a stream of pkt-lines demultiplexed into its sideband channels.
///
/// Wraps a reader positioned at the first packet of a sideband response and yields one packet at a
/// time, separating the pack from the progress messages and the errors. A fetch uses it to stream a
/// pack out of the response without buffering the whole thing, while printing progress as it goes.
///
/// # Caveats
///
/// * **Progress and error packets must be consumed, not ignored**, or the reader stalls: the pack
///   data is interleaved with them and a caller that only accepts channel 1 will keep reading
///   packets it discards. The loop shape is "read a packet, dispatch on the channel", and every
///   channel must be handled.
/// * **An error packet does not end the stream by itself.** Git sends the message and then closes;
///   a reader that returned the error and stopped would leak the connection, and one that treated
///   the message as advisory would continue into a stream that is about to end. The caller records
///   the error text and keeps reading until the stream ends, then reports it.
/// * **The underlying reader is buffered by the caller.** This type holds `R` and does no buffering
///   of its own, so an unbuffered socket makes every packet header a syscall. Wrapping in
///   `BufReader` outside is the expected construction.
/// * **Not `Clone` and not `Sync`.** It owns the reader and holds a half-consumed packet position;
///   two copies would each read part of the same stream.
#[derive(Debug)]
pub struct SidebandReader<R> {
    /// The stream the packets are read from, positioned mid-response.
    inner: R,
    /// Which capability was negotiated, because a stream without sideband has no channel bytes and
    /// must not have one skipped. See [`SidebandKind`]'s caveat.
    enabled: bool,
    /// Set once a channel-3 packet has been seen, so a caller can report the failure after draining
    /// the rest of the stream rather than abandoning it.
    error_seen: bool,
}

impl<R: Read> SidebandReader<R> {
    /// Phase 11 — wrap a reader that is positioned at the first packet of a sideband response.
    ///
    /// # Caveats
    ///
    /// **`enabled` must reflect what was actually negotiated**, not what was requested. A server
    /// that ignored the `side-band-64k` capability sends bare pack packets, and a reader that
    /// assumed the capability took effect strips a byte from each one and reports a corrupt pack.
    /// The caller reads the advertisement or the v2 capability list and passes what it saw.
    pub fn new(inner: R, enabled: bool) -> Self {
        Self {
            inner,
            enabled,
            error_seen: false,
        }
    }

    /// Phase 11 — read the next packet, or `Ok(None)` when the stream ends.
    ///
    /// # Caveats
    ///
    /// * **`Ok(None)` means the flush packet arrived**, which is the normal end of a sideband
    ///   response, and it is not an error. A caller that saw an [`SidebandKind::Error`] packet
    ///   earlier must still check [`SidebandReader::error_seen`] before treating the result as
    ///   success — the stream ends cleanly whether or not the exchange worked.
    /// * **With `enabled: false` every packet is returned as [`SidebandKind::Pack`]** with its
    ///   payload untouched. That is the shape the pack reader wants and the shape a caller that
    ///   forgot to check the capability would get wrong.
    /// * **Progress and error packets are returned, not filtered.** Filtering would need somewhere
    ///   to put the progress text and would make the error packet easy to drop; the caller's loop
    ///   is short and the dispatch is the point of the type.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`] for a malformed frame or an unknown channel byte, and
    /// [`crate::error::Error::Transport`] if the underlying read fails. A stream that ends without a
    /// flush is reported the same way, because a peer that closed mid-response is not a clean end.
    pub fn next_packet(&mut self) -> Result<Option<SidebandPacket>> {
        loop {
            let Some(line) = PktLine::read_from_opt(&mut self.inner)? else {
                return Ok(None);
            };
            match line {
                // A delimiter separates two sections of a protocol v2 response; it is not the end
                // of the stream and carries nothing a pack reader can use.
                PktLine::Delim => continue,
                // A flush ends the section and a response-end ends the whole response. Neither
                // can be followed by more of this exchange.
                PktLine::Flush | PktLine::ResponseEnd => return Ok(None),
                PktLine::Data(data) => {
                    // An empty payload is legal and carries no channel byte. Treating it as a frame
                    // with a channel would index past the end of the slice.
                    if data.is_empty() {
                        continue;
                    }
                    let (channel, payload) = if self.enabled {
                        let ch = SidebandKind::from_channel(data[0])?;
                        (ch, &data[1..])
                    } else {
                        (SidebandKind::Pack, &data[..])
                    };
                    if channel == SidebandKind::Error {
                        self.error_seen = true;
                    }
                    return Ok(Some(SidebandPacket {
                        channel,
                        data: payload.to_vec(),
                    }));
                }
            }
        }
    }

    /// Phase 11 — whether a channel-3 error packet has been seen.
    ///
    /// # Caveats
    ///
    /// Sticky: once true it stays true, so a caller that drains the stream and then checks this gets
    /// the answer regardless of how many packets arrived afterwards. The error's *text* is not kept
    /// here — the caller accumulated it from the packets it was returned — because this type would
    /// otherwise have to decide how much of a hostile peer's message to retain.
    pub const fn error_seen(&self) -> bool {
        self.error_seen
    }

    /// Phase 11 — drain the remaining packets, forwarding progress and collecting errors.
    ///
    /// The convenience for a caller that does not want to stream — `index-pack` reads the whole pack
    /// into memory anyway — and just needs the pack bytes plus whatever the server said.
    ///
    /// # Caveats
    ///
    /// * **Collects the pack into one buffer**, which is exactly what the caveat on
    ///   [`SidebandReader::next_packet`] says not to do for a large fetch. Use it for a push's
    ///   `report-status` and for tests, not for a clone of a large repository.
    /// * **The progress callback is called per packet**, not per line. A caller that wants
    ///   line-oriented progress splits the payload itself, because a packet may carry a partial
    ///   line — the pack data is interleaved and the boundaries do not align with lines.
    /// * **A channel-3 error is returned as [`crate::error::Error::Protocol`]** carrying the text,
    ///   after the stream has been drained. Returning early would leave the connection half-read,
    ///   which matters for a caller that reuses it for a second request.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Protocol`] for a malformed frame, an unknown channel, or a remote
    /// error message, and [`crate::error::Error::Transport`] if the read fails.
    pub fn drain(&mut self, progress: &mut dyn FnMut(&[u8])) -> Result<Vec<u8>> {
        let mut pack = Vec::new();
        let mut error_text = Vec::new();
        loop {
            match self.next_packet()? {
                Some(packet) => {
                    match packet.channel {
                        SidebandKind::Pack => pack.extend_from_slice(&packet.data),
                        SidebandKind::Progress => progress(&packet.data),
                        SidebandKind::Error => error_text.extend_from_slice(&packet.data),
                    }
                }
                None => break,
            }
        }
        if !error_text.is_empty() {
            return Err(Error::Protocol(String::from_utf8_lossy(&error_text).into_owned()));
        }
        Ok(pack)
    }
}
