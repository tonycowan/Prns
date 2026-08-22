//! Tabbed LocalClient UI: Me / Others / Chats.

use std::collections::{HashMap, HashSet};

use dioxus::prelude::*;

use crate::backend;
use crate::model::{ChatDirection, ChatLine, ConnectionPhase, HeardAnnounce, Snapshot};
use crate::timeutil::sleep_ms;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Me,
    Others,
    Chats,
}

#[component]
pub fn App() -> Element {
    let mut snap = use_signal(|| {
        backend::ensure_started();
        backend::poll_snapshot()
    });
    let mut toast = use_signal(|| None::<String>);
    let mut selected_peer = use_signal(String::new);
    let mut draft = use_signal(String::new);
    let mut tab = use_signal(|| Tab::Me);

    // Announce-arrival badge on the Others tab (clears when Others is visited).
    let mut others_announce_badge = use_signal(|| false);
    let mut known_peers = use_signal(HashSet::<String>::new);
    let mut peers_seeded = use_signal(|| false);

    // Unread inbound messages: per-peer highest viewed inbound seq.
    // Others tab + list items stay embellished until that peer is clicked.
    let mut viewed_inbound_seq = use_signal(HashMap::<String, u64>::new);

    use_future(move || async move {
        loop {
            sleep_ms(250).await;
            backend::ensure_started();
            let next = backend::poll_snapshot();
            update_announce_badges(
                &next,
                tab(),
                &mut peers_seeded,
                &mut known_peers,
                &mut others_announce_badge,
            );
            // Active chat: treat new inbound messages for this peer as already seen.
            if tab() == Tab::Chats {
                let peer = selected_peer();
                if !peer.is_empty() {
                    mark_peer_viewed(&peer, &next.messages, &mut viewed_inbound_seq);
                }
            }
            snap.set(next);
        }
    });

    let mut flash = move |message: String| {
        toast.set(Some(message));
        spawn(async move {
            sleep_ms(2_000).await;
            toast.set(None);
        });
    };

    let connected = matches!(snap.read().phase, ConnectionPhase::Connected);
    let peer_now = selected_peer();
    let current_tab = tab();

    let unread_peers = unread_peer_set(&snap.read().messages, &viewed_inbound_seq());
    let others_tab_embellished = others_announce_badge() || !unread_peers.is_empty();

    // Keep Chats scrolled to the newest message (bottom).
    let chat_len = snap
        .read()
        .messages
        .iter()
        .filter(|m| !peer_now.is_empty() && m.peer_hex == peer_now)
        .count();
    let scroll_peer = peer_now.clone();
    use_effect(move || {
        let _ = (current_tab, chat_len, scroll_peer.clone());
        if current_tab == Tab::Chats {
            spawn(async move {
                sleep_ms(50).await;
                let _ = document::eval(
                    "const e = document.getElementById('chat-scroll'); if (e) { e.scrollTop = e.scrollHeight; }",
                );
            });
        }
    });

    let open_others = move |_| {
        // Visiting Others clears the "new announce" tab badge and remembers everyone heard.
        others_announce_badge.set(false);
        let mut known = known_peers();
        for entry in snap.read().heard.iter() {
            known.insert(entry.destination_hex.clone());
        }
        known_peers.set(known);
        tab.set(Tab::Others);
    };

    let select_peer = move |hex: String| {
        mark_peer_viewed(&hex, &snap.read().messages, &mut viewed_inbound_seq);
        selected_peer.set(hex);
        tab.set(Tab::Chats);
    };

    rsx! {
        div { class: "app",
            header { class: "top",
                div { class: "brand", "Personal Text" }
                nav { class: "tabs", role: "tablist",
                    TabButton {
                        label: "Me".to_string(),
                        active: current_tab == Tab::Me,
                        embellished: false,
                        onclick: move |_| tab.set(Tab::Me),
                    }
                    TabButton {
                        label: "Others".to_string(),
                        active: current_tab == Tab::Others,
                        embellished: others_tab_embellished && current_tab != Tab::Others,
                        onclick: open_others,
                    }
                    TabButton {
                        label: "Chats".to_string(),
                        active: current_tab == Tab::Chats,
                        embellished: false,
                        onclick: move |_| tab.set(Tab::Chats),
                    }
                }
            }

            div { class: "tab-body",
                match current_tab {
                    Tab::Me => rsx! {
                        MeTab {
                            snap,
                            connected,
                            on_announce: move |_| {
                                match backend::request_announce() {
                                    Ok(()) => flash("Announce requested".to_string()),
                                    Err(error) => flash(error),
                                }
                            },
                        }
                    },
                    Tab::Others => rsx! {
                        OthersTab {
                            heard: snap.read().heard.clone(),
                            selected: peer_now.clone(),
                            unread: unread_peers.clone(),
                            on_select: select_peer,
                        }
                    },
                    Tab::Chats => rsx! {
                        ChatsTab {
                            peer: peer_now.clone(),
                            messages: snap.read().messages.clone(),
                            draft,
                            connected,
                            on_draft: move |value| draft.set(value),
                            on_send: move |_| {
                                let peer = selected_peer();
                                let text = draft();
                                match backend::request_send(peer, text) {
                                    Ok(()) => {
                                        draft.set(String::new());
                                        flash("Send requested".to_string());
                                    }
                                    Err(error) => flash(error),
                                }
                            },
                        }
                    },
                }
            }

            div { class: "toast",
                if let Some(message) = toast() {
                    "{message}"
                }
            }
        }
    }
}

fn update_announce_badges(
    next: &Snapshot,
    current_tab: Tab,
    peers_seeded: &mut Signal<bool>,
    known_peers: &mut Signal<HashSet<String>>,
    others_announce_badge: &mut Signal<bool>,
) {
    if !peers_seeded() {
        let mut known = HashSet::new();
        for entry in next.heard.iter() {
            known.insert(entry.destination_hex.clone());
        }
        known_peers.set(known);
        peers_seeded.set(true);
        return;
    }

    if current_tab == Tab::Others {
        // Already looking at the list — absorb arrivals so leaving doesn't re-badge them.
        let mut known = known_peers();
        for entry in next.heard.iter() {
            known.insert(entry.destination_hex.clone());
        }
        known_peers.set(known);
        return;
    }

    let known = known_peers();
    let discovered = next
        .heard
        .iter()
        .any(|entry| !known.contains(&entry.destination_hex));
    if discovered {
        others_announce_badge.set(true);
    }
}

fn unread_peer_set(
    messages: &[ChatLine],
    viewed_inbound_seq: &HashMap<String, u64>,
) -> HashSet<String> {
    let mut unread = HashSet::new();
    for message in messages {
        if message.direction != ChatDirection::In {
            continue;
        }
        let viewed = viewed_inbound_seq
            .get(&message.peer_hex)
            .copied()
            .unwrap_or(0);
        if message.seq > viewed {
            unread.insert(message.peer_hex.clone());
        }
    }
    unread
}

fn mark_peer_viewed(
    peer: &str,
    messages: &[ChatLine],
    viewed_inbound_seq: &mut Signal<HashMap<String, u64>>,
) {
    let max_seq = messages
        .iter()
        .filter(|m| m.direction == ChatDirection::In && m.peer_hex == peer)
        .map(|m| m.seq)
        .max()
        .unwrap_or(0);
    let mut map = viewed_inbound_seq();
    map.insert(peer.to_string(), max_seq);
    viewed_inbound_seq.set(map);
}

#[component]
fn TabButton(
    label: String,
    active: bool,
    embellished: bool,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    let class = match (active, embellished) {
        (true, _) => "tab active",
        (false, true) => "tab embellished",
        (false, false) => "tab",
    };
    rsx! {
        button {
            class: "{class}",
            role: "tab",
            aria_selected: active,
            onclick: move |event| onclick.call(event),
            span { class: "tab-label", "{label}" }
            if embellished {
                span { class: "tab-dot", aria_hidden: true }
            }
        }
    }
}

#[component]
fn MeTab(
    snap: Signal<Snapshot>,
    connected: bool,
    on_announce: EventHandler<MouseEvent>,
) -> Element {
    let phase_label = snap.read().phase.label().to_string();
    let phase_class = snap.read().phase.class_name().to_string();
    let bus = snap.read().bus.clone();
    let dest = snap
        .read()
        .destination_hex
        .clone()
        .unwrap_or_else(|| "—".to_string());
    let announce_count = snap.read().announce_count.to_string();
    let last_announce = snap.read().last_announce.clone();
    let mode = if snap.read().live {
        "live".to_string()
    } else {
        "mock (web)".to_string()
    };
    let fail_detail = match &snap.read().phase {
        ConnectionPhase::Failed(err) => Some(err.clone()),
        _ => None,
    };

    rsx! {
        div { class: "tab-pane me-pane",
            p { class: "sub",
                "LocalClient of Hopspot — never hosts. Announce, then chat with a heard peer."
            }

            div { class: "panel",
                StatusRow {
                    label: "Status".to_string(),
                    value: phase_label,
                    class_name: phase_class,
                }
                if let Some(err) = fail_detail {
                    StatusRow {
                        label: "Detail".to_string(),
                        value: err,
                        class_name: "status-bad".to_string(),
                    }
                }
                StatusRow {
                    label: "Bus".to_string(),
                    value: bus,
                    class_name: "value".to_string(),
                }
                StatusRow {
                    label: "LXMF dest".to_string(),
                    value: dest,
                    class_name: "value".to_string(),
                }
                StatusRow {
                    label: "Announces".to_string(),
                    value: announce_count,
                    class_name: "value".to_string(),
                }
                if let Some(last) = last_announce {
                    StatusRow {
                        label: "Last announce".to_string(),
                        value: last,
                        class_name: "value".to_string(),
                    }
                }
                StatusRow {
                    label: "Mode".to_string(),
                    value: mode,
                    class_name: "value".to_string(),
                }
            }

            button {
                class: "primary",
                disabled: !connected,
                onclick: move |event| on_announce.call(event),
                "Announce"
            }
        }
    }
}

#[component]
fn OthersTab(
    heard: Vec<HeardAnnounce>,
    selected: String,
    unread: HashSet<String>,
    on_select: EventHandler<String>,
) -> Element {
    rsx! {
        div { class: "tab-pane others-pane",
            if heard.is_empty() {
                p { class: "empty", "None yet — announce from another peer on the mesh." }
            } else {
                ul { class: "heard-list",
                    for entry in heard.iter() {
                        {
                            let hex = entry.destination_hex.clone();
                            let hex_click = hex.clone();
                            let is_selected = hex == selected;
                            let has_unread = unread.contains(&hex);
                            let item_class = match (is_selected, has_unread) {
                                (true, true) => "heard-item selected embellished",
                                (true, false) => "heard-item selected",
                                (false, true) => "heard-item embellished",
                                (false, false) => "heard-item",
                            };
                            rsx! {
                                li {
                                    class: "{item_class}",
                                    onclick: move |_| on_select.call(hex_click.clone()),
                                    div { class: "heard-hash-row",
                                        div { class: "heard-hash", "{entry.destination_hex}" }
                                        if has_unread {
                                            span { class: "item-dot", aria_label: "new messages" }
                                        }
                                    }
                                    div { class: "heard-meta",
                                        "hops {entry.hops} · {entry.interface} · #{entry.seq}"
                                        if is_selected { " · selected" }
                                        if has_unread { " · new" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ChatsTab(
    peer: String,
    messages: Vec<ChatLine>,
    draft: Signal<String>,
    connected: bool,
    on_draft: EventHandler<String>,
    on_send: EventHandler<MouseEvent>,
) -> Element {
    let mut thread: Vec<ChatLine> = messages
        .into_iter()
        .filter(|m| !peer.is_empty() && m.peer_hex == peer)
        .collect();
    // Engine stores newest-first; show oldest at top, newest at bottom.
    thread.sort_by_key(|m| m.seq);

    let can_send = connected && !peer.is_empty() && !draft().trim().is_empty();
    let peer_label = if peer.is_empty() {
        "No peer selected — pick someone under Others.".to_string()
    } else {
        peer.clone()
    };

    rsx! {
        div { class: "tab-pane chats-pane",
            div { class: "chat-peer",
                span { class: "label", "With" }
                span { class: "value peer", "{peer_label}" }
            }

            div { class: "chat-scroll", id: "chat-scroll",
                if peer.is_empty() {
                    p { class: "empty", "Select a peer in Others to open a chat." }
                } else if thread.is_empty() {
                    p { class: "empty", "No messages with this peer yet." }
                } else {
                    ul { class: "chat-list",
                        for line in thread.iter() {
                            li {
                                class: if line.direction == ChatDirection::Out {
                                    "chat-item out"
                                } else {
                                    "chat-item in"
                                },
                                div { class: "chat-meta",
                                    if line.direction == ChatDirection::Out { "you · " } else { "them · " }
                                    "{line.status}"
                                }
                                div { class: "chat-text", "{line.text}" }
                            }
                        }
                    }
                }
            }

            div { class: "compose",
                textarea {
                    class: "draft",
                    placeholder: if peer.is_empty() {
                        "Select a peer first…"
                    } else {
                        "Type a short text…"
                    },
                    value: "{draft}",
                    disabled: !connected || peer.is_empty(),
                    oninput: move |event| on_draft.call(event.value()),
                }
                button {
                    class: "primary",
                    disabled: !can_send,
                    onclick: move |event| on_send.call(event),
                    "Send"
                }
            }
        }
    }
}

#[component]
fn StatusRow(label: String, value: String, class_name: String) -> Element {
    rsx! {
        div { class: "row",
            span { class: "label", "{label}" }
            span { class: "value {class_name}", "{value}" }
        }
    }
}
