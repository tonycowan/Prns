//! Platform sleep for Dioxus futures (gloo is wasm-only).

use std::time::Duration;

pub async fn sleep_ms(ms: u64) {
    #[cfg(target_arch = "wasm32")]
    {
        gloo_timers::future::TimeoutFuture::new(ms as u32).await;
    }
    #[cfg(all(not(target_arch = "wasm32"), feature = "live"))]
    {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    #[cfg(all(not(target_arch = "wasm32"), not(feature = "live")))]
    {
        // Desktop/mobile always enable `live`; this branch is for odd feature combos.
        let _ = ms;
        std::future::ready(()).await;
    }
}
