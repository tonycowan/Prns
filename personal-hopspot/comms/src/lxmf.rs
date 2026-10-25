//! An LXMF note: the time it was written, an empty title, the words, and fields.
//!
//! A place, when the note still has one, is a `loc` field. Direct delivery is
//! the packed message on a link. The link's own receipt is the acknowledgement.
//! Propagation is not used here.

use crate::model::Place;

/// A note opened from an LXMF message.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteLetter {
    pub words: String,
    /// Unix seconds carried in the message.
    pub written_at: f64,
    pub place: Option<Place>,
}

/// Packs the words the way LXMF packs a message.
pub fn pack_note(words: &str, written_at: f64) -> Vec<u8> {
    pack_placed_note(words, written_at, None)
}

/// Packs a note that may also carry a place in its fields.
pub fn pack_placed_note(words: &str, written_at: f64, place: Option<Place>) -> Vec<u8> {
    let words = words.trim().as_bytes();
    let mut out = Vec::with_capacity(16 + words.len());
    out.push(0x94);
    out.push(0xcb);
    out.extend_from_slice(&written_at.to_be_bytes());
    push_bin(&mut out, b"");
    push_bin(&mut out, words);
    match place {
        Some(place) => {
            let text = place.text();
            out.push(0x81);
            push_bin(&mut out, b"loc");
            push_bin(&mut out, text.as_bytes());
        }
        None => out.push(0x80),
    }
    out
}

/// A reaction to a note, matched by the time it was written, or by its words.
#[derive(Debug, Clone, PartialEq)]
pub struct ReactionLetter {
    pub written_at: f64,
    /// The words of the note this mark belongs to.
    pub words: String,
    pub reaction: Option<crate::model::Reaction>,
}

/// Packs a reaction. The message body is empty, so an older app leaves it closed.
pub fn pack_reaction(
    target_written_at: f64,
    words: &str,
    reaction: Option<crate::model::Reaction>,
) -> Vec<u8> {
    let name = reaction.map(crate::model::Reaction::name).unwrap_or("-");
    let on = format!("{:016x}", target_written_at.to_bits());
    let mut out = Vec::new();
    out.push(0x94);
    out.push(0xcb);
    out.extend_from_slice(&unix_now().to_be_bytes());
    push_bin(&mut out, b"");
    push_bin(&mut out, b"");
    out.push(0x83);
    push_bin(&mut out, b"on");
    push_bin(&mut out, on.as_bytes());
    push_bin(&mut out, b"rx");
    push_bin(&mut out, name.as_bytes());
    push_bin(&mut out, b"of");
    push_bin(&mut out, words.trim().as_bytes());
    out
}

/// The most bytes one link packet can carry.
const LINK_PACKET: usize = 431;
const FRAGMENT_MARK: u8 = 0xF1;
const FRAGMENT_HEADER: usize = 13;
const MAX_FRAGMENTS: usize = 48;

/// One piece of a note that did not fit in a single packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    pub group: u64,
    pub index: u16,
    pub count: u16,
    pub bytes: Vec<u8>,
}

/// Splits a packed note into link packets. A note that already fits is left as one packet.
pub fn link_packets(packed: &[u8]) -> Vec<Vec<u8>> {
    if packed.is_empty() {
        return Vec::new();
    }
    if packed.len() <= LINK_PACKET {
        return vec![packed.to_vec()];
    }
    let room = LINK_PACKET - FRAGMENT_HEADER;
    let count = packed.len().div_ceil(room);
    if count > MAX_FRAGMENTS {
        return Vec::new();
    }
    let group = group_id(packed);
    let mut packets = Vec::with_capacity(count);
    for (index, chunk) in packed.chunks(room).enumerate() {
        let mut packet = Vec::with_capacity(FRAGMENT_HEADER + chunk.len());
        packet.push(FRAGMENT_MARK);
        packet.extend_from_slice(&group.to_be_bytes());
        packet.extend_from_slice(&(index as u16).to_be_bytes());
        packet.extend_from_slice(&(count as u16).to_be_bytes());
        packet.extend_from_slice(chunk);
        packets.push(packet);
    }
    packets
}

/// Opens one piece of a split note. A whole note is left closed.
pub fn open_fragment(bytes: &[u8]) -> Option<Fragment> {
    if bytes.first() != Some(&FRAGMENT_MARK) || bytes.len() <= FRAGMENT_HEADER {
        return None;
    }
    let group = u64::from_be_bytes(bytes[1..9].try_into().ok()?);
    let index = u16::from_be_bytes(bytes[9..11].try_into().ok()?);
    let count = u16::from_be_bytes(bytes[11..13].try_into().ok()?);
    let count_usize = usize::from(count);
    if count_usize == 0 || count_usize > MAX_FRAGMENTS || usize::from(index) >= count_usize {
        return None;
    }
    let piece = bytes[FRAGMENT_HEADER..].to_vec();
    if piece.is_empty() || piece.len() > LINK_PACKET - FRAGMENT_HEADER {
        return None;
    }
    Some(Fragment {
        group,
        index,
        count,
        bytes: piece,
    })
}

/// Pieces of one note, joined when every piece has arrived.
#[derive(Debug, Default)]
pub struct Assembly {
    count: u16,
    parts: Vec<Option<Vec<u8>>>,
}

impl Assembly {
    /// Stores this piece. The packed note comes back once every piece is here.
    pub fn offer(&mut self, fragment: Fragment) -> Option<Vec<u8>> {
        let count = usize::from(fragment.count);
        if self.count != fragment.count || self.parts.len() != count {
            self.count = fragment.count;
            self.parts = vec![None; count];
        }
        self.parts[usize::from(fragment.index)] = Some(fragment.bytes);
        if self.parts.iter().any(Option::is_none) {
            return None;
        }
        let mut whole = Vec::new();
        for part in &mut self.parts {
            whole.extend(part.take().unwrap_or_default());
        }
        self.count = 0;
        self.parts.clear();
        Some(whole)
    }
}

fn group_id(packed: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in packed {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

struct ReadMessage {
    words: String,
    written_at: f64,
    fields: MessageFields,
}

struct MessageFields {
    place: Option<Place>,
    reaction: Option<(f64, String, Option<crate::model::Reaction>)>,
}

fn read_message(bytes: &[u8]) -> Option<ReadMessage> {
    let mut reader = Reader { bytes };
    let len = reader.array_len()?;
    if len < 4 {
        return None;
    }
    let written_at = reader.number()?;
    let _title = reader.bytes_or_text()?;
    let words = reader.bytes_or_text()?;
    let fields = reader.fields()?;
    for _ in 4..len {
        reader.skip()?;
    }
    if !reader.bytes.is_empty() {
        return None;
    }
    let words = String::from_utf8(words).ok()?;
    Some(ReadMessage {
        words: words.trim().to_string(),
        written_at,
        fields,
    })
}

/// Opens a packed LXMF message. Anything that is not one is left closed.
pub fn open_note(bytes: &[u8]) -> Option<NoteLetter> {
    let read = read_message(bytes)?;
    if read.fields.reaction.is_some() || (read.words.is_empty() && read.fields.place.is_none()) {
        return None;
    }
    Some(NoteLetter {
        words: read.words,
        written_at: read.written_at,
        place: read.fields.place,
    })
}

/// Opens a reaction. A note, and anything else, is left closed.
pub fn open_reaction(bytes: &[u8]) -> Option<ReactionLetter> {
    let read = read_message(bytes)?;
    let (written_at, words, reaction) = read.fields.reaction?;
    Some(ReactionLetter {
        written_at,
        words,
        reaction,
    })
}

/// Seconds since 1970-01-01 00:00:00 UTC.
pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

fn push_bin(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len();
    if len <= u8::MAX as usize {
        out.push(0xc4);
        out.push(len as u8);
    } else {
        out.push(0xc5);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    }
    out.extend_from_slice(bytes);
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn array_len(&mut self) -> Option<usize> {
        let marker = self.u8()?;
        if (0x90..=0x9f).contains(&marker) {
            return Some((marker & 0x0f) as usize);
        }
        match marker {
            0xdc => Some(self.u16()? as usize),
            0xdd => usize::try_from(self.u32()?).ok(),
            _ => None,
        }
    }

    fn number(&mut self) -> Option<f64> {
        let marker = self.u8()?;
        if marker <= 0x7f {
            return Some(f64::from(marker));
        }
        match marker {
            0xcc => Some(f64::from(self.u8()?)),
            0xcd => Some(f64::from(self.u16()?)),
            0xce => Some(self.u32()? as f64),
            0xcf => Some(self.u64()? as f64),
            0xd0 => Some(f64::from(self.u8()? as i8)),
            0xd1 => Some(f64::from(self.u16()? as i16)),
            0xd2 => Some(f64::from(self.u32()? as i32)),
            0xd3 => Some(self.u64()? as i64 as f64),
            0xca => Some(f64::from(f32::from_be_bytes(
                self.take(4)?.try_into().ok()?,
            ))),
            0xcb => Some(f64::from_be_bytes(self.take(8)?.try_into().ok()?)),
            0xe0..=0xff => Some(f64::from(marker as i8)),
            _ => None,
        }
    }

    fn bytes_or_text(&mut self) -> Option<Vec<u8>> {
        let marker = self.u8()?;
        let len = if (0xa0..=0xbf).contains(&marker) {
            (marker & 0x1f) as usize
        } else {
            match marker {
                0xc4 | 0xd9 => self.u8()? as usize,
                0xc5 | 0xda => self.u16()? as usize,
                0xc6 | 0xdb => usize::try_from(self.u32()?).ok()?,
                _ => return None,
            }
        };
        Some(self.take(len)?.to_vec())
    }

    fn fields(&mut self) -> Option<MessageFields> {
        let marker = self.u8()?;
        let pairs = if (0x80..=0x8f).contains(&marker) {
            (marker & 0x0f) as usize
        } else if marker == 0xde {
            self.u16()? as usize
        } else if marker == 0xdf {
            usize::try_from(self.u32()?).ok()?
        } else {
            self.skip_marked(marker)?;
            return Some(MessageFields {
                place: None,
                reaction: None,
            });
        };
        let mut place = None;
        let mut mark = None;
        let mut on = None;
        let mut of = String::new();
        for _ in 0..pairs {
            match self.text_or_skip()? {
                Some(key) if key == b"loc" => {
                    if let Some(value) = self.text_or_skip()? {
                        if let Ok(text) = std::str::from_utf8(&value) {
                            place = Place::parse(text);
                        }
                    }
                }
                Some(key) if key == b"rx" => {
                    if let Some(value) = self.text_or_skip()? {
                        mark = String::from_utf8(value).ok();
                    }
                }
                Some(key) if key == b"on" => {
                    if let Some(value) = self.text_or_skip()? {
                        if let Ok(text) = std::str::from_utf8(&value) {
                            on = u64::from_str_radix(text, 16).ok().map(f64::from_bits);
                        }
                    }
                }
                Some(key) if key == b"of" => {
                    if let Some(value) = self.text_or_skip()? {
                        if let Ok(text) = String::from_utf8(value) {
                            of = text;
                        }
                    }
                }
                _ => self.skip()?,
            }
        }
        let reaction = match mark {
            Some(name) => {
                let written_at = on?;
                let reaction = if name == "-" {
                    None
                } else {
                    Some(crate::model::Reaction::parse(&name)?)
                };
                Some((written_at, of, reaction))
            }
            None => None,
        };
        Some(MessageFields { place, reaction })
    }

    fn text_or_skip(&mut self) -> Option<Option<Vec<u8>>> {
        let marker = *self.bytes.first()?;
        if (0xa0..=0xbf).contains(&marker)
            || matches!(marker, 0xc4 | 0xc5 | 0xc6 | 0xd9 | 0xda | 0xdb)
        {
            Some(Some(self.bytes_or_text()?))
        } else {
            self.skip()?;
            Some(None)
        }
    }

    fn skip(&mut self) -> Option<()> {
        let marker = self.u8()?;
        self.skip_marked(marker)
    }

    fn skip_marked(&mut self, marker: u8) -> Option<()> {
        if marker <= 0x7f || (0xe0..=0xff).contains(&marker) {
            return Some(());
        }
        match marker {
            0xc0 | 0xc2 | 0xc3 => Some(()),
            0xcc | 0xd0 => self.take(1).map(|_| ()),
            0xcd | 0xd1 => self.take(2).map(|_| ()),
            0xce | 0xd2 | 0xca => self.take(4).map(|_| ()),
            0xcf | 0xd3 | 0xcb => self.take(8).map(|_| ()),
            0xd4 => self.take(2).map(|_| ()),
            0xd5 => self.take(3).map(|_| ()),
            0xd6 => self.take(5).map(|_| ()),
            0xd7 => self.take(9).map(|_| ()),
            0xd8 => self.take(17).map(|_| ()),
            0xc4 | 0xd9 => {
                let len = self.u8()? as usize;
                self.take(len).map(|_| ())
            }
            0xc5 | 0xda => {
                let len = self.u16()? as usize;
                self.take(len).map(|_| ())
            }
            0xc6 | 0xdb => {
                let len = usize::try_from(self.u32()?).ok()?;
                self.take(len).map(|_| ())
            }
            0xc7 => {
                let len = self.u8()? as usize;
                self.take(1 + len).map(|_| ())
            }
            0xc8 => {
                let len = self.u16()? as usize;
                self.take(1 + len).map(|_| ())
            }
            0xc9 => {
                let len = usize::try_from(self.u32()?).ok()?;
                self.take(1 + len).map(|_| ())
            }
            marker if (0xa0..=0xbf).contains(&marker) => {
                self.take((marker & 0x1f) as usize).map(|_| ())
            }
            marker if (0x90..=0x9f).contains(&marker) => self.skip_n((marker & 0x0f) as usize),
            0xdc => {
                let n = self.u16()? as usize;
                self.skip_n(n)
            }
            0xdd => {
                let n = usize::try_from(self.u32()?).ok()?;
                self.skip_n(n)
            }
            marker if (0x80..=0x8f).contains(&marker) => {
                self.skip_n(((marker & 0x0f) as usize) * 2)
            }
            0xde => {
                let n = self.u16()? as usize;
                self.skip_n(n * 2)
            }
            0xdf => {
                let n = usize::try_from(self.u32()?).ok()?;
                self.skip_n(n * 2)
            }
            _ => None,
        }
    }

    fn skip_n(&mut self, count: usize) -> Option<()> {
        for _ in 0..count {
            self.skip()?;
        }
        Some(())
    }

    fn u8(&mut self) -> Option<u8> {
        let (&value, rest) = self.bytes.split_first()?;
        self.bytes = rest;
        Some(value)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.bytes.len() < len {
            return None;
        }
        let (head, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Some(head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_round_trips_as_an_lxmf_message() {
        let packed = pack_note("  The pears are ready.  ", 1_700_000_000.0);
        let note = open_note(&packed).expect("note");
        assert_eq!(note.words, "The pears are ready.");
        assert_eq!(note.written_at, 1_700_000_000.0);
        assert!(note.place.is_none());
        assert_eq!(packed[0], 0x94);
        assert_eq!(packed[1], 0xcb);
        assert_eq!(*packed.last().unwrap(), 0x80);
    }

    #[test]
    fn a_long_note_travels_as_packets_and_comes_back_together() {
        let words = "a".repeat(1400);
        let packed = pack_note(&words, 1_700_000_000.0);
        #[cfg(not(target_arch = "wasm32"))]
        assert_eq!(
            LINK_PACKET,
            personal_rns::engine::MAX_SEND_TO_LINK_PLAINTEXT_LEN
        );
        assert!(packed.len() > LINK_PACKET);
        let packets = link_packets(&packed);
        assert!(packets.len() > 1);
        assert!(packets.iter().all(|packet| packet.len() <= LINK_PACKET));
        assert!(open_note(&packets[0]).is_none());
        let mut assembly = Assembly::default();
        let mut whole = None;
        for packet in &packets {
            let fragment = open_fragment(packet).expect("a piece");
            whole = assembly.offer(fragment);
        }
        let whole = whole.expect("the note");
        assert_eq!(whole, packed);
        let note = open_note(&whole).expect("note");
        assert_eq!(note.words, words);
        let short = pack_note("The pears are ready.", 1_700_000_000.0);
        let one = link_packets(&short);
        assert_eq!(one, vec![short]);
    }

    #[test]
    fn plain_words_are_not_a_message() {
        assert!(open_note(b"The pears are ready.").is_none());
    }

    #[test]
    fn a_reaction_round_trips_and_is_not_a_note() {
        let packed = pack_reaction(
            1_700_000_000.0,
            "The pears are ready.",
            Some(crate::model::Reaction::Love),
        );
        assert!(open_note(&packed).is_none());
        let letter = open_reaction(&packed).expect("reaction");
        assert_eq!(letter.written_at.to_bits(), 1_700_000_000.0f64.to_bits());
        assert_eq!(letter.words, "The pears are ready.");
        assert_eq!(letter.reaction, Some(crate::model::Reaction::Love));
        let cleared = pack_reaction(1_700_000_000.0, "The pears are ready.", None);
        let letter = open_reaction(&cleared).expect("cleared");
        assert_eq!(letter.written_at.to_bits(), 1_700_000_000.0f64.to_bits());
        assert!(letter.reaction.is_none());
        assert!(open_reaction(&pack_note("The pears are ready.", 1.0)).is_none());
    }

    #[test]
    fn a_message_with_a_field_still_opens() {
        // timestamp as an integer, title as text, one string field after the words.
        let mut packed = vec![0x94, 0xce];
        packed.extend_from_slice(&1_700_000_000u32.to_be_bytes());
        packed.extend_from_slice(&[0xa0, 0xa2, b'H', b'i', 0x81, 0x01, 0xa3, b'y', b'e', b's']);
        let note = open_note(&packed).expect("note");
        assert_eq!(note.words, "Hi");
        assert_eq!(note.written_at, 1_700_000_000.0);
        assert!(note.place.is_none());
    }

    #[test]
    fn a_place_round_trips_in_the_fields() {
        let place = Place::from_degrees(40.7484, -73.9857).expect("place");
        let packed = pack_placed_note("Meet me 📍 Location", 1_700_000_000.0, Some(place));
        let note = open_note(&packed).expect("note");
        assert_eq!(note.words, "Meet me 📍 Location");
        assert_eq!(note.place, Some(place));
        let without = pack_placed_note("Meet me", 1_700_000_000.0, None);
        assert!(open_note(&without).expect("note").place.is_none());
    }
}
