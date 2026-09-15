//! Temporary path-request reply-chain tracing (feature `log`).

use crate::wire::DestinationHash;

pub(crate) struct DestHex<'a>(pub(crate) &'a DestinationHash);

impl core::fmt::Display for DestHex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in self.0.as_bytes() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

pub(crate) struct BytesHex<'a>(pub(crate) &'a [u8]);

impl core::fmt::Display for BytesHex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
