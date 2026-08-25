//! Backend: mock on web, live LocalClient engine on desktop/mobile.

use crate::model::{AutoRangeSession, RangePrompt, Snapshot};

#[cfg(feature = "live")]
use crate::engine;

pub fn init_logging() {
    #[cfg(all(feature = "live", target_os = "android"))]
    {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("PersonalText"),
        );
    }
}

#[allow(dead_code)]
pub fn is_live() -> bool {
    cfg!(feature = "live")
}

pub fn ensure_started() {
    #[cfg(feature = "live")]
    engine::ensure_started();
}

pub fn poll_snapshot() -> Snapshot {
    #[cfg(feature = "live")]
    {
        return engine::snapshot();
    }
    #[cfg(not(feature = "live"))]
    {
        Snapshot::sample()
    }
}

pub fn request_announce() -> Result<(), String> {
    #[cfg(feature = "live")]
    {
        return engine::request_announce();
    }
    #[cfg(not(feature = "live"))]
    {
        Err("Announce needs the desktop/mobile build (Hopspot LocalClient).".into())
    }
}

pub fn request_send(peer_hex: String, text: String) -> Result<(), String> {
    #[cfg(feature = "live")]
    {
        return engine::request_send(peer_hex, text);
    }
    #[cfg(not(feature = "live"))]
    {
        let _ = (peer_hex, text);
        Err("Send needs the desktop/mobile build (Hopspot LocalClient).".into())
    }
}

pub fn clear_range_prompt() {
    #[cfg(feature = "live")]
    engine::clear_range_prompt();
}

pub fn take_range_prompt() -> Option<RangePrompt> {
    #[cfg(feature = "live")]
    {
        return engine::take_range_prompt();
    }
    #[cfg(not(feature = "live"))]
    {
        None
    }
}

pub fn take_auto_reply() -> Option<RangePrompt> {
    #[cfg(feature = "live")]
    {
        return engine::take_auto_reply();
    }
    #[cfg(not(feature = "live"))]
    {
        None
    }
}

pub fn set_auto_range_session(session: Option<AutoRangeSession>) {
    #[cfg(feature = "live")]
    engine::set_auto_range_session(session);
    #[cfg(not(feature = "live"))]
    {
        let _ = session;
    }
}
