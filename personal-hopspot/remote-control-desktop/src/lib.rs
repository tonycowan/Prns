#[cfg(target_os = "android")]
mod android;
mod app;
mod backend;
mod edits;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod flash;
#[cfg(target_os = "ios")]
mod ios;
mod identity_clone;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod image_catalog;
mod roster_sync;

/// macOS secondary threads default to 512 KiB. The host node future needs a
/// main-sized stack or `controller-node` aborts on launch.
pub(crate) const CONTROLLER_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

pub fn launch_controller() {
    init_controller_logging();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(CONTROLLER_THREAD_STACK_BYTES)
        .build()
        .expect("tokio runtime");
    let _enter = runtime.enter();

    #[cfg(all(feature = "mobile", target_os = "android"))]
    {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Debug)
                .with_filter(
                    android_logger::FilterBuilder::new()
                        .parse("warn,prns_interfaces_tokio::wifi_auto=debug,prns_ffi::mdns=debug")
                        .build(),
                ),
        );
    }

    #[cfg(feature = "desktop")]
    {
        use dioxus::desktop::{Config, WindowBuilder};
        let mut config = Config::new().with_window(
            WindowBuilder::new()
                .with_title("PRNS Controller")
                .with_always_on_top(false),
        );
        if let Some(icon) = controller_window_icon() {
            config = config.with_icon(icon);
        }
        dioxus::LaunchBuilder::desktop()
            .with_cfg(config)
            .launch(app::App);
    }

    #[cfg(feature = "mobile")]
    {
        #[cfg(target_os = "ios")]
        ios::adopt_scene_lifecycle();
        dioxus::LaunchBuilder::mobile().launch(app::App);
    }
}

#[cfg(feature = "desktop")]
fn controller_window_icon() -> Option<dioxus::desktop::tao::window::Icon> {
    use dioxus::desktop::tao::window::Icon;
    dioxus::desktop::icon_from_memory::<Icon>(include_bytes!("../assets/icon-256.png")).ok()
}

fn init_controller_logging() {
    #[cfg(not(target_os = "android"))]
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        // Prefer Wi-Fi Auto / mDNS over BLE and runtime mesh chatter while debugging LL discovery.
        // Override with RUST_LOG. BLE wire stderr is opt-in via PRNS_BLE_WIRE_LOG=1.
        let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new(
                "warn,prns_interfaces_tokio::wifi_auto=debug,prns_ffi::mdns=debug",
            )
        });
        let _ = tracing_log::LogTracer::init();
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .try_init();
        eprintln!("wifi-diag: controller logging initialized (wifi_auto/mdns debug)");
    }
}
