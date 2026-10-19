//! An LXMF note: the time it was written, an empty title, the words, and no fields.
//!
//! Direct delivery is the packed message on a link. The link's own receipt is the
//! acknowledgement. Propagation is not used here.

/// A note opened from an LXMF message.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteLetter {
    pub words: String,
    /// Unix seconds carried in the message.
    pub written_at: f64,
}

/// Packs the words the way LXMF packs a message.
pub fn pack_note(words: &str, written_at: f64) -> Vec<u8> {
    let words = words.trim().as_bytes();
    let mut out = Vec::with_capacity(16 + words.len());
    out.push(0x94);
    out.push(0xcb);
    out.extend_from_slice(&written_at.to_be_bytes());
    push_bin(&mut out, b"");
    push_bin(&mut out, words);
    out.push(0x80);
    out
}

/// Opens a packed LXMF message. Anything that is not one is left closed.
pub fn open_note(bytes: &[u8]) -> Option<NoteLetter> {
    let mut reader = Reader { bytes };
    let len = reader.array_len()?;
    if len < 4 {
        return None;
    }
    let written_at = reader.number()?;
    let _title = reader.bytes_or_text()?;
    let words = reader.bytes_or_text()?;
    for _ in 3..len {
        reader.skip()?;
    }
    if !reader.bytes.is_empty() {
        return None;
    }
    let words = String::from_utf8(words).ok()?;
    let words = words.trim();
    if words.is_empty() {
        return None;
    }
    Some(NoteLetter {
        words: words.to_string(),
        written_at,
    })
}

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

    fn skip(&mut self) -> Option<()> {
        let marker = self.u8()?;
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
        assert_eq!(packed[0], 0x94);
        assert_eq!(packed[1], 0xcb);
        assert_eq!(*packed.last().unwrap(), 0x80);
    }

    #[test]
    fn plain_words_are_not_a_message() {
        assert!(open_note(b"The pears are ready.").is_none());
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
    }
}
