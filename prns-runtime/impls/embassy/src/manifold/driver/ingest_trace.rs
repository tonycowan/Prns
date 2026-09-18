use crate::engine::IgnoreReason;
use crate::interfaces::InterfaceId;
use crate::wire::WireContext;

pub(super) fn log_ingest_ignore(
    reason: IgnoreReason,
    context: Option<WireContext>,
    source: InterfaceId,
    payload_len: usize,
) {
    if !trace_ignore(reason, context) {
        return;
    }
    #[cfg(feature = "log")]
    log::info!(
        target: "personal_hopspot_esp32",
        "rc: ingest ignore={reason:?} ctx={context:?} if={} bytes={payload_len}",
        InterfaceTrace(source)
    );
    #[cfg(not(feature = "log"))]
    let _ = (source, payload_len, context);
}

fn trace_ignore(reason: IgnoreReason, context: Option<WireContext>) -> bool {
    matches!(
        reason,
        IgnoreReason::PermissionDenied
            | IgnoreReason::RequestTooLarge
            | IgnoreReason::DecryptFailed
            | IgnoreReason::LinkPhaseMismatch
            | IgnoreReason::UnknownLink
            | IgnoreReason::Malformed
            | IgnoreReason::LinkRequestsRefused
            | IgnoreReason::UnhandledContext
            | IgnoreReason::CapacityExhausted
            | IgnoreReason::NotForUs
            | IgnoreReason::Duplicate
            | IgnoreReason::NoRoute
            | IgnoreReason::Consumed
    ) || matches!(
        context,
        Some(
            WireContext::Request
                | WireContext::Response
                | WireContext::LinkIdentify
                | WireContext::LinkClose
                | WireContext::LinkRtt
                | WireContext::Channel
        )
    )
}

#[cfg(feature = "log")]
struct InterfaceTrace(InterfaceId);

#[cfg(feature = "log")]
impl core::fmt::Display for InterfaceTrace {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let bytes = self.0.as_bytes();
        write!(
            formatter,
            "{:02x}{:02x}{:02x}{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_handler_refusals_are_traced() {
        assert!(trace_ignore(
            IgnoreReason::PermissionDenied,
            Some(WireContext::Request)
        ));
        assert!(trace_ignore(IgnoreReason::PermissionDenied, None));
        assert!(trace_ignore(
            IgnoreReason::RequestTooLarge,
            Some(WireContext::Request)
        ));
        assert!(trace_ignore(
            IgnoreReason::DecryptFailed,
            Some(WireContext::Request)
        ));
    }

    #[test]
    fn ordinary_announce_noise_is_not_traced() {
        assert!(!trace_ignore(
            IgnoreReason::NotForUs,
            Some(WireContext::None)
        ));
        assert!(!trace_ignore(
            IgnoreReason::Duplicate,
            Some(WireContext::None)
        ));
        assert!(!trace_ignore(
            IgnoreReason::Consumed,
            Some(WireContext::None)
        ));
    }

    #[test]
    fn a_request_context_ignore_is_traced_even_when_the_reason_is_ordinary() {
        assert!(trace_ignore(
            IgnoreReason::Duplicate,
            Some(WireContext::Request)
        ));
    }
}
