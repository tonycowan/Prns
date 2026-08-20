//! Demo Hopspot management UI in Dioxus.
//!
//! Surfaces the same *functionality* as the OLED/Android face (interfaces,
//! announce, limits, sleep, power toggles, RNS config export) with ordinary
//! mobile navigation patterns. State is mocked until wired to `PrnsService`.

mod model;
mod ui;

use dioxus::prelude::*;

use crate::ui::App;

const MAIN_CSS: Asset = asset!("/assets/main.css");

fn main() {
    dioxus::launch(Root);
}

#[component]
fn Root() -> Element {
    rsx! {
        document::Stylesheet { href: MAIN_CSS }
        document::Link {
            rel: "stylesheet",
            href: "https://fonts.googleapis.com/css2?family=IBM+Plex+Sans:wght@400;600;650&family=IBM+Plex+Mono:wght@500&display=swap",
        }
        App {}
    }
}
