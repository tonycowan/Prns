//! Mobile-norm screens for the Hopspot management demo.

use dioxus::prelude::*;

use crate::model::{fmt_bytes, DemoState, InterfaceCard, InterfaceKind};

#[derive(Clone, Debug, PartialEq)]
enum Screen {
    Home,
    Interface { id: u32 },
    Limits,
    RnsConfig,
}

#[component]
pub fn App() -> Element {
    let mut state = use_signal(DemoState::sample);
    let mut screen = use_signal(|| Screen::Home);
    let mut sheet_open = use_signal(|| false);

    // Drop toasts after a short delay without a timer crate: clear on next interaction
    // and whenever the home screen re-renders after a signal write.
    state.write().clear_stale_notice();

    let notice = state.read().notice.clone();

    rsx! {
        div { class: "app",
            match screen() {
                Screen::Home => rsx! {
                    TopBar {
                        title: "Personal Hopspot",
                        on_menu: move |_| sheet_open.set(true),
                    }
                    div { class: "content",
                        StatusPanel { state }
                        h2 { class: "section-title", "Interfaces" }
                        InterfaceList {
                            state,
                            on_open: move |id| screen.set(Screen::Interface { id }),
                        }
                    }
                    div { class: "fab-bar",
                        button {
                            class: "btn primary",
                            onclick: move |_| state.write().announce(),
                            "Announce"
                        }
                        button {
                            class: "btn",
                            onclick: move |_| sheet_open.set(true),
                            "More"
                        }
                    }
                },
                Screen::Interface { id } => {
                    let card = state.read().cards.iter().find(|card| card.id == id).cloned();
                    match card {
                        Some(card) => rsx! {
                            TopBar {
                                title: "{card.kind.label()}",
                                show_back: true,
                                on_back: move |_| screen.set(Screen::Home),
                                on_menu: move |_| sheet_open.set(true),
                            }
                            InterfaceDetail {
                                state,
                                card,
                                on_rns_config: move |_| screen.set(Screen::RnsConfig),
                            }
                        },
                        None => rsx! {
                            TopBar {
                                title: "Missing interface",
                                show_back: true,
                                on_back: move |_| screen.set(Screen::Home),
                                on_menu: move |_| sheet_open.set(true),
                            }
                            div { class: "content muted", "That interface is no longer in the demo set." }
                        },
                    }
                },
                Screen::Limits => rsx! {
                    TopBar {
                        title: "Limits",
                        show_back: true,
                        on_back: move |_| screen.set(Screen::Home),
                        on_menu: move |_| sheet_open.set(true),
                    }
                    LimitsPage { state }
                },
                Screen::RnsConfig => rsx! {
                    TopBar {
                        title: "RNS Config",
                        show_back: true,
                        on_back: move |_| screen.set(Screen::Home),
                        on_menu: move |_| sheet_open.set(true),
                    }
                    RnsConfigPage { state }
                },
            }

            if sheet_open() {
                GlobalSheet {
                    state,
                    on_close: move |_| sheet_open.set(false),
                    on_limits: move |_| {
                        sheet_open.set(false);
                        screen.set(Screen::Limits);
                    },
                }
            }

            if let Some(notice) = notice {
                div { class: "toast", "{notice.message}" }
            }
        }
    }
}

#[component]
fn TopBar(
    title: String,
    #[props(default = false)] show_back: bool,
    #[props(default = None)] on_back: Option<EventHandler<()>>,
    on_menu: EventHandler<()>,
) -> Element {
    rsx! {
        header { class: "top-bar",
            if show_back {
                button {
                    class: "icon-btn",
                    aria_label: "Back",
                    onclick: move |_| {
                        if let Some(handler) = on_back {
                            handler.call(());
                        }
                    },
                    "←"
                }
            }
            h1 { "{title}" }
            button {
                class: "icon-btn",
                aria_label: "Menu",
                onclick: move |_| on_menu.call(()),
                "⋮"
            }
        }
    }
}

#[component]
fn StatusPanel(state: Signal<DemoState>) -> Element {
    let snap = state();
    rsx! {
        section { class: "status-card",
            div { class: "status-row",
                span { class: "status-label", "Node" }
                span { class: "chip {snap.engine.chip_class()}", "{snap.engine.label()}" }
            }
            if snap.sleeping {
                p { class: "muted", "Interfaces are sleeping. Wake from More." }
            }
            div { class: "metrics",
                div { class: "metric",
                    div { class: "k", "Uptime" }
                    div { class: "v", "{snap.uptime}" }
                }
                div { class: "metric",
                    div { class: "k", "Online" }
                    div { class: "v", "{snap.online_interface_count}/{snap.interface_count}" }
                }
                div { class: "metric",
                    div { class: "k", "RX" }
                    div { class: "v", "{fmt_bytes(snap.rx_bytes)}" }
                }
                div { class: "metric",
                    div { class: "k", "TX" }
                    div { class: "v", "{fmt_bytes(snap.tx_bytes)}" }
                }
            }
        }
    }
}

#[component]
fn InterfaceList(state: Signal<DemoState>, on_open: EventHandler<u32>) -> Element {
    let cards = state.read().cards.clone();
    rsx! {
        div { class: "list",
            for card in cards {
                {
                    let id = card.id;
                    rsx! {
                        button {
                            class: "row",
                            key: "{id}",
                            onclick: move |_| on_open.call(id),
                            div { class: "row-icon", "{card.kind.short_icon()}" }
                            div { class: "row-body",
                                div { class: "row-title", "{card.kind.label()}" }
                                div { class: "row-sub", "{card.subtitle()}" }
                            }
                            span { class: "chip {card.connection.chip_class()}", "{card.connection.label()}" }
                            span { class: "chevron", "›" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn InterfaceDetail(
    state: Signal<DemoState>,
    card: InterfaceCard,
    on_rns_config: EventHandler<()>,
) -> Element {
    let id = card.id;
    let powered = card.connection.is_powered_on();
    let power_label = if powered { "Turn off" } else { "Turn on" };

    rsx! {
        div { class: "content",
            section { class: "status-card detail-header",
                div { class: "status-row",
                    span { class: "status-label", "Status" }
                    span { class: "chip {card.connection.chip_class()}", "{card.connection.label()}" }
                }
                if let Some(reason) = card.failure_reason {
                    p { class: "muted", "{reason}" }
                }
                if card.connection == crate::model::ConnectionState::Connected {
                    div { class: "metrics",
                        div { class: "metric",
                            div { class: "k", "TX" }
                            div { class: "v", "{fmt_bytes(card.tx_bytes)}" }
                        }
                        div { class: "metric",
                            div { class: "k", "RX" }
                            div { class: "v", "{fmt_bytes(card.rx_bytes)}" }
                        }
                        div { class: "metric",
                            div { class: "k", "Links" }
                            div { class: "v", "{card.links}" }
                        }
                        div { class: "metric",
                            div { class: "k", "Peers" }
                            div { class: "v", "{card.peers.unwrap_or(card.destinations)}" }
                        }
                    }
                }
                if let Some(age) = card.activity_age {
                    p { class: "muted", "Last activity {age} ago" }
                }
                for line in card.detail_lines.iter() {
                    p { class: "muted", "{line}" }
                }
            }

            h2 { class: "section-title", "Actions" }
            div { class: "detail-actions",
                button {
                    class: if powered { "btn danger" } else { "btn primary" },
                    onclick: move |_| state.write().toggle_power(id),
                    "{power_label}"
                }
                if card.kind == InterfaceKind::Local {
                    button {
                        class: "btn",
                        onclick: move |_| on_rns_config.call(()),
                        "RNS Config"
                    }
                }
            }
        }
    }
}

#[component]
fn LimitsPage(state: Signal<DemoState>) -> Element {
    let limits = state.read().limits.clone();
    rsx! {
        div { class: "content",
            p { class: "muted", "Storage and transport limits for this node (mock values)." }
            div { class: "limits",
                for row in limits {
                    div { class: "limit-row",
                        span { class: "name", "{row.name}" }
                        span { class: "value", "{row.value}" }
                    }
                }
            }
        }
    }
}

#[component]
fn RnsConfigPage(state: Signal<DemoState>) -> Element {
    let config = state.read().rns_config.clone();
    rsx! {
        div { class: "content",
            p { class: "muted",
                "Paste into Sideband Utilities → Advanced Reticulum settings. Live builds will fill the device RPC key."
            }
            pre { class: "config-box mono", "{config}" }
            button {
                class: "btn primary",
                onclick: move |_| state.write().copy_rns_config(),
                "Copy to clipboard"
            }
        }
    }
}

#[component]
fn GlobalSheet(
    state: Signal<DemoState>,
    on_close: EventHandler<()>,
    on_limits: EventHandler<()>,
) -> Element {
    let sleep_label = if state.read().sleeping {
        "Wake interfaces"
    } else {
        "Sleep interfaces"
    };

    rsx! {
        div {
            class: "sheet-backdrop",
            onclick: move |_| on_close.call(()),
            div {
                class: "sheet",
                onclick: move |event| event.stop_propagation(),
                div { class: "sheet-handle" }
                h2 { "Node actions" }
                button {
                    class: "btn",
                    onclick: move |_| {
                        state.write().announce();
                        on_close.call(());
                    },
                    "Announce"
                }
                button {
                    class: "btn",
                    onclick: move |_| on_limits.call(()),
                    "Limits"
                }
                button {
                    class: "btn",
                    onclick: move |_| {
                        state.write().toggle_sleep();
                        on_close.call(());
                    },
                    "{sleep_label}"
                }
                button {
                    class: "btn",
                    onclick: move |_| on_close.call(()),
                    "Close"
                }
            }
        }
    }
}
