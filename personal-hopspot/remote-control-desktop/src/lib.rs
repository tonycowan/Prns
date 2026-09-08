#[cfg(target_os = "android")]
mod android;
mod app;
mod backend;
mod edits;
#[cfg(not(target_os = "android"))]
mod flash;
mod identity_clone;
mod roster_sync;

pub fn launch_controller() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _enter = runtime.enter();

    #[cfg(all(feature = "mobile", target_os = "android"))]
    {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Info),
        );
    }

    #[cfg(feature = "desktop")]
    {
        use dioxus::desktop::{Config, WindowBuilder};
        dioxus::LaunchBuilder::desktop()
            .with_cfg(
                Config::new().with_window(
                    WindowBuilder::new()
                        .with_title("PRNS Controller")
                        .with_always_on_top(false),
                ),
            )
            .launch(app::App);
    }

    #[cfg(feature = "mobile")]
    {
        dioxus::LaunchBuilder::mobile().launch(app::App);
    }
}
