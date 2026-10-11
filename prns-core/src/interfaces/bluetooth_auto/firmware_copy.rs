//! Plain GATT notes a MeshTower sends while it copies an image into flash.
//!
//! These are not Reticulum fragments. The first byte is `P` or `R`, never a fragment kind.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirmwareCopyNote {
    Progress { written: u32, total: u32 },
    Rebooting,
}

pub fn parse_firmware_copy_note(bytes: &[u8]) -> Option<FirmwareCopyNote> {
    let mut bytes = bytes;
    if let Some(stripped) = bytes.strip_suffix(b"\n") {
        bytes = stripped;
    }
    if let Some(stripped) = bytes.strip_suffix(b"\r") {
        bytes = stripped;
    }
    if bytes == b"R" {
        return Some(FirmwareCopyNote::Rebooting);
    }
    let rest = bytes.strip_prefix(b"P ")?;
    let (written, rest) = take_u32(rest)?;
    let rest = rest.strip_prefix(b" ")?;
    let (total, rest) = take_u32(rest)?;
    if !rest.is_empty() || total == 0 || written > total {
        return None;
    }
    Some(FirmwareCopyNote::Progress { written, total })
}

fn take_u32(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let mut value = 0u32;
    let mut index = 0;
    if bytes.first().is_none_or(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        if index == 10 {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(u32::from(bytes[index] - b'0'))?;
        index += 1;
    }
    Some((value, &bytes[index..]))
}

#[cfg(feature = "std")]
mod sink {
    use std::sync::{Mutex, OnceLock};

    use super::FirmwareCopyNote;

    fn slot() -> &'static Mutex<Option<Box<dyn Fn(FirmwareCopyNote) + Send + Sync>>> {
        static SINK: OnceLock<Mutex<Option<Box<dyn Fn(FirmwareCopyNote) + Send + Sync>>>> =
            OnceLock::new();
        SINK.get_or_init(|| Mutex::new(None))
    }

    pub fn install(sink: Box<dyn Fn(FirmwareCopyNote) + Send + Sync>) {
        *slot().lock().expect("firmware copy sink") = Some(sink);
    }

    pub fn clear() {
        if let Ok(mut sink) = slot().lock() {
            *sink = None;
        }
    }

    pub fn dispatch(bytes: &[u8]) -> bool {
        let Some(note) = super::parse_firmware_copy_note(bytes) else {
            return false;
        };
        if let Ok(sink) = slot().lock() {
            if let Some(sink) = sink.as_ref() {
                sink(note);
            }
        }
        true
    }
}

#[cfg(feature = "std")]
pub use sink::{
    clear as clear_firmware_copy_sink, dispatch as dispatch_firmware_copy_notification,
    install as install_firmware_copy_sink,
};

#[cfg(test)]
mod tests {
    use super::{parse_firmware_copy_note, FirmwareCopyNote};

    #[test]
    fn firmware_copy_notes() {
        assert_eq!(
            parse_firmware_copy_note(b"P 4096 746496\n"),
            Some(FirmwareCopyNote::Progress {
                written: 4096,
                total: 746496
            })
        );
        assert_eq!(
            parse_firmware_copy_note(b"R\n"),
            Some(FirmwareCopyNote::Rebooting)
        );
        assert_eq!(parse_firmware_copy_note(b"\x01\x00\x00\x00\x01"), None);
        assert_eq!(parse_firmware_copy_note(b"P 3 2\n"), None);
    }
}
