//! Minimal LocalClient announce + LXMF text app for Hopspot.
//!
//! Starts Hopspot first (shared RNS host). This app never hosts — it only joins
//! `127.0.0.1:37428` via `connect_existing_shared_instance`.

mod backend;
#[cfg(feature = "live")]
mod engine;
#[cfg(feature = "live")]
mod lxmf;
mod model;
mod timeutil;
mod ui;

use dioxus::prelude::*;

use crate::ui::App;

const MAIN_CSS: Asset = asset!("/assets/main.css");

fn main() {
    backend::init_logging();
    #[cfg(feature = "desktop")]
    {
        use dioxus::desktop::{Config, WindowBuilder};
        dioxus::LaunchBuilder::desktop()
            .with_cfg(Config::new().with_window(
                WindowBuilder::new()
                    .with_title("Personal Text")
                    .with_always_on_top(false),
            ))
            .launch(Root);
    }
    #[cfg(not(feature = "desktop"))]
    {
        dioxus::launch(Root);
    }
}

#[component]
fn Root() -> Element {
    #[cfg(feature = "desktop")]
    {
        let window = dioxus::desktop::use_window();
        use_effect(move || {
            window.set_always_on_top(false);
        });
    }

    rsx! {
        document::Stylesheet { href: MAIN_CSS }
        document::Link {
            rel: "stylesheet",
            href: "https://fonts.googleapis.com/css2?family=IBM+Plex+Sans:wght@400;600;650&family=IBM+Plex+Mono:wght@500&display=swap",
        }
        App {}
    }
}
