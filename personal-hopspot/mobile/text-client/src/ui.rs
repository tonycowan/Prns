//! Tabbed LocalClient UI: Me / Others / Chats.

use std::collections::{HashMap, HashSet};

use dioxus::prelude::*;

use crate::aliases;
use crate::backend;
use crate::location;
use crate::model::{
    AutoRangeRole, AutoRangeSession, ChatDirection, ChatLine, ConnectionPhase, HeardAnnounce,
    RangePrompt, RangePromptKind, Snapshot,
};
use crate::range_check::{self, GeoPoint};
use crate::timeutil::{format_message_time, sleep_ms};

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
    let toast = use_signal(|| None::<String>);
    let mut selected_peer = use_signal(String::new);
    let mut draft = use_signal(String::new);
    let mut tab = use_signal(|| Tab::Me);

    let mut peer_aliases = use_signal(aliases::load);
    let mut alias_next = use_signal(|| 0u32);
    let mut editing_alias = use_signal(|| None::<String>);

    let mut others_announce_badge = use_signal(|| false);
    let mut known_peers = use_signal(HashSet::<String>::new);
    let mut peers_seeded = use_signal(|| false);
    let mut viewed_inbound_seq = use_signal(HashMap::<String, u64>::new);
    let mut range_busy = use_signal(|| false);

    use_future(move || async move {
        let mut saw_auto_peer = false;
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
            let extra_peers = message_only_peers(&next.heard, &next.messages);
            let (updated, next_num) = aliases::ensure_defaults(
                &next.heard,
                &extra_peers,
                peer_aliases(),
                alias_next(),
            );
            if updated != peer_aliases() {
                peer_aliases.set(updated.clone());
                alias_next.set(next_num);
                aliases::persist(&updated, next_num);
            }
            if tab() == Tab::Chats {
                let peer = selected_peer();
                if !peer.is_empty() {
                    mark_peer_viewed(&peer, &next.messages, &mut viewed_inbound_seq);
                }
            }

            let has_auto = next.auto_range.is_some();
            if saw_auto_peer && !has_auto {
                show_toast(toast, "Auto range check stopped".to_string());
            }
            saw_auto_peer = has_auto;

            if let Some(prompt) = backend::take_auto_reply() {
                if !range_busy() {
                    range_busy.set(true);
                    spawn(async move {
                        let _ = send_range_reply(toast, prompt, None, false).await;
                        range_busy.set(false);
                    });
                } else {
                    #[cfg(feature = "live")]
                    crate::engine::restore_auto_reply(prompt);
                }
            }

            snap.set(next);
        }
    });

    // Only the initiating Driver sends Range check every 10s.
    use_future(move || async move {
        loop {
            sleep_ms(range_check::AUTO_RANGE_INTERVAL_MS).await;
            let Some(peer) = snap.read().auto_range.as_ref().and_then(|session| {
                (session.role == AutoRangeRole::Driver).then(|| session.peer_hex.clone())
            }) else {
                continue;
            };
            if range_busy() {
                continue;
            }
            if !matches!(snap.read().phase, ConnectionPhase::Connected) {
                continue;
            }
            range_busy.set(true);
            spawn(async move {
                match location::current_fix().await {
                    Ok(point) => {
                        let wired = range_check::format_request(point);
                        match backend::request_send(peer, wired) {
                            Ok(()) => {}
                            Err(error) => show_toast(toast, error),
                        }
                    }
                    Err(error) => show_toast(toast, error.label()),
                }
                range_busy.set(false);
            });
        }
    });

    let flash = move |message: String| {
        show_toast(toast, message);
    };

    let connected = matches!(snap.read().phase, ConnectionPhase::Connected);
    let peer_now = selected_peer();
    let current_tab = tab();
    let alias_map = peer_aliases();
    let pending_range = snap.read().pending_range_prompt.clone();
    let auto_session = snap.read().auto_range.clone();

    let unread_peers = unread_peer_set(&snap.read().messages, &viewed_inbound_seq());
    let others_tab_embellished = others_announce_badge() || !unread_peers.is_empty();

    let open_others = move |_| {
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
        editing_alias.set(None);
        tab.set(Tab::Chats);
    };

    let set_alias = move |(hex, name): (String, String)| {
        let trimmed = name.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        let mut map = peer_aliases();
        map.insert(hex, trimmed);
        peer_aliases.set(map.clone());
        aliases::persist(&map, alias_next());
        editing_alias.set(None);
    };

    let send_chat = move |_| {
        if range_busy() {
            return;
        }
        let peer = selected_peer();
        let text = draft();

        if range_check::is_stop(&text) {
            match backend::request_send(peer, text) {
                Ok(()) => {
                    draft.set(String::new());
                    flash("Stop sent".to_string());
                }
                Err(error) => flash(error),
            }
            return;
        }

        if range_check::is_bare_auto_range_check(&text) {
            range_busy.set(true);
            spawn(async move {
                match location::current_fix().await {
                    Ok(point) => {
                        let wired = range_check::format_auto_request(point);
                        match backend::request_send(peer.clone(), wired) {
                            Ok(()) => {
                                backend::set_auto_range_session(Some(AutoRangeSession {
                                    peer_hex: peer,
                                    role: AutoRangeRole::Driver,
                                }));
                                draft.set(String::new());
                                show_toast(toast, "Auto range check started".to_string());
                            }
                            Err(error) => show_toast(toast, error),
                        }
                    }
                    Err(error) => show_toast(toast, error.label()),
                }
                range_busy.set(false);
            });
            return;
        }

        if range_check::is_bare_range_check(&text) {
            range_busy.set(true);
            spawn(async move {
                match location::current_fix().await {
                    Ok(point) => {
                        let wired = range_check::format_request(point);
                        match backend::request_send(peer, wired) {
                            Ok(()) => {
                                draft.set(String::new());
                                show_toast(toast, "Range check sent".to_string());
                            }
                            Err(error) => show_toast(toast, error),
                        }
                    }
                    Err(error) => show_toast(toast, error.label()),
                }
                range_busy.set(false);
            });
            return;
        }
        match backend::request_send(peer, text) {
            Ok(()) => {
                draft.set(String::new());
                flash("Send requested".to_string());
            }
            Err(error) => flash(error),
        }
    };

    let accept_range = move |_| {
        if range_busy() {
            return;
        }
        let Some(prompt) = backend::take_range_prompt() else {
            return;
        };
        let start_role = (prompt.kind == RangePromptKind::Auto).then_some(AutoRangeRole::Responder);
        let peer_for_ui = prompt.peer_hex.clone();
        range_busy.set(true);
        spawn(async move {
            let ok = send_range_reply(toast, prompt, start_role, true).await;
            if ok {
                selected_peer.set(peer_for_ui);
                tab.set(Tab::Chats);
            }
            range_busy.set(false);
        });
    };

    let deny_range = move |_| {
        let was_auto = snap
            .read()
            .pending_range_prompt
            .as_ref()
            .is_some_and(|p| p.kind == RangePromptKind::Auto);
        backend::clear_range_prompt();
        flash(if was_auto {
            "Auto range check declined".to_string()
        } else {
            "Range check declined".to_string()
        });
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
                            messages: snap.read().messages.clone(),
                            aliases: alias_map.clone(),
                            selected: peer_now.clone(),
                            unread: unread_peers.clone(),
                            editing: editing_alias(),
                            on_select: select_peer,
                            on_edit_alias: move |hex| editing_alias.set(Some(hex)),
                            on_save_alias: set_alias,
                            on_cancel_edit: move |_| editing_alias.set(None),
                        }
                    },
                    Tab::Chats => rsx! {
                        ChatsTab {
                            peer: peer_now.clone(),
                            peer_label: if peer_now.is_empty() {
                                "No peer selected — pick someone under Others.".to_string()
                            } else {
                                aliases::display_name(&peer_now, &alias_map)
                            },
                            my_hex: snap
                                .read()
                                .destination_hex
                                .clone()
                                .unwrap_or_default(),
                            messages: snap.read().messages.clone(),
                            draft,
                            connected,
                            busy: range_busy(),
                            auto_active: auto_session
                                .as_ref()
                                .is_some_and(|s| s.peer_hex == peer_now),
                            auto_driving: auto_session.as_ref().is_some_and(|s| {
                                s.peer_hex == peer_now && s.role == AutoRangeRole::Driver
                            }),
                            on_draft: move |value| draft.set(value),
                            on_send: send_chat,
                        }
                    },
                }
            }

            if let Some(prompt) = pending_range.clone() {
                RangeCheckModal {
                    peer_label: aliases::display_name(&prompt.peer_hex, &alias_map),
                    peer_coords: format!(
                        "({:.6}, {:.6})",
                        prompt.latitude, prompt.longitude
                    ),
                    auto_session: prompt.kind == RangePromptKind::Auto,
                    busy: range_busy(),
                    on_accept: accept_range,
                    on_deny: deny_range,
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

fn show_toast(mut toast: Signal<Option<String>>, message: String) {
    toast.set(Some(message));
    spawn(async move {
        sleep_ms(2_000).await;
        toast.set(None);
    });
}

async fn send_range_reply(
    toast: Signal<Option<String>>,
    prompt: RangePrompt,
    join_as: Option<AutoRangeRole>,
    from_modal: bool,
) -> bool {
    match location::current_fix().await {
        Ok(own) => match GeoPoint::try_new(prompt.latitude, prompt.longitude) {
            Ok(peer_point) => {
                let reply = range_check::format_reply(own, peer_point);
                match backend::request_send(prompt.peer_hex.clone(), reply) {
                    Ok(()) => {
                        if let Some(role) = join_as {
                            backend::set_auto_range_session(Some(AutoRangeSession {
                                peer_hex: prompt.peer_hex.clone(),
                                role,
                            }));
                            show_toast(toast, "Auto range check started".to_string());
                        } else if from_modal {
                            show_toast(toast, "Range reply sent".to_string());
                        }
                        true
                    }
                    Err(error) => {
                        show_toast(toast, error);
                        false
                    }
                }
            }
            Err(_) => {
                show_toast(toast, "Invalid peer coordinates in range check.".into());
                false
            }
        },
        Err(error) => {
            if from_modal {
                restore_range_prompt(prompt);
            } else {
                #[cfg(feature = "live")]
                crate::engine::restore_auto_reply(prompt);
            }
            show_toast(toast, error.label());
            false
        }
    }
}

fn restore_range_prompt(prompt: RangePrompt) {
    backend_set_range_prompt(prompt);
}

fn backend_set_range_prompt(prompt: RangePrompt) {
    #[cfg(feature = "live")]
    {
        crate::engine::restore_range_prompt(prompt);
    }
    #[cfg(not(feature = "live"))]
    {
        let _ = prompt;
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

#[derive(Clone)]
enum OtherRow {
    Announced(HeardAnnounce),
    MessageOnly { hex: String },
}

const MESSAGE_ONLY_NOTICE: &str =
    "Has not announced on the mesh yet. Return path may be unavailable.";

fn message_only_peers(heard: &[HeardAnnounce], messages: &[ChatLine]) -> Vec<String> {
    let heard_set: HashSet<String> = heard
        .iter()
        .map(|entry| entry.destination_hex.clone())
        .collect();
    let mut peers = HashSet::new();
    for message in messages {
        if message.peer_hex == "unknown" {
            continue;
        }
        if heard_set.contains(&message.peer_hex) {
            continue;
        }
        peers.insert(message.peer_hex.clone());
    }
    let mut list: Vec<String> = peers.into_iter().collect();
    list.sort();
    list
}

fn build_other_rows(heard: &[HeardAnnounce], messages: &[ChatLine]) -> Vec<OtherRow> {
    let mut rows: Vec<OtherRow> = heard
        .iter()
        .cloned()
        .map(OtherRow::Announced)
        .collect();

    let mut msg_only: Vec<(String, u64)> = message_only_peers(heard, messages)
        .into_iter()
        .map(|hex| {
            let latest = messages
                .iter()
                .filter(|m| m.peer_hex == hex)
                .map(|m| m.seq)
                .max()
                .unwrap_or(0);
            (hex, latest)
        })
        .collect();
    msg_only.sort_by_key(|(_, seq)| std::cmp::Reverse(*seq));
    for (hex, _) in msg_only {
        rows.push(OtherRow::MessageOnly { hex });
    }
    rows
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
    messages: Vec<ChatLine>,
    aliases: HashMap<String, String>,
    selected: String,
    unread: HashSet<String>,
    editing: Option<String>,
    on_select: EventHandler<String>,
    on_edit_alias: EventHandler<String>,
    on_save_alias: EventHandler<(String, String)>,
    on_cancel_edit: EventHandler<()>,
) -> Element {
    let rows = build_other_rows(&heard, &messages);

    rsx! {
        div { class: "tab-pane others-pane",
            if rows.is_empty() {
                p { class: "empty", "None yet — announce from another peer on the mesh." }
            } else {
                ul { class: "heard-list",
                    for row in rows.iter() {
                        {
                            let (hex, announced) = match row {
                                OtherRow::Announced(entry) => {
                                    (entry.destination_hex.clone(), Some(entry.clone()))
                                }
                                OtherRow::MessageOnly { hex } => (hex.clone(), None),
                            };
                            let hex_open = hex.clone();
                            let hex_edit = hex.clone();
                            let hex_save = hex.clone();
                            let is_selected = hex == selected;
                            let has_unread = unread.contains(&hex);
                            let alias = aliases
                                .get(&hex)
                                .cloned()
                                .unwrap_or_else(|| hex.clone());
                            let is_editing = editing.as_deref() == Some(hex.as_str());
                            let item_class = match (is_selected, has_unread, announced.is_some()) {
                                (true, true, true) => "heard-item selected embellished",
                                (true, false, true) => "heard-item selected",
                                (false, true, true) => "heard-item embellished",
                                (false, false, true) => "heard-item",
                                (true, true, false) => "heard-item message-only selected embellished",
                                (true, false, false) => "heard-item message-only selected",
                                (false, true, false) => "heard-item message-only embellished",
                                (false, false, false) => "heard-item message-only",
                            };
                            rsx! {
                                li {
                                    class: "{item_class}",
                                    onclick: move |_| {
                                        if !is_editing {
                                            on_select.call(hex_open.clone());
                                        }
                                    },
                                    div { class: "heard-alias-row",
                                        if is_editing {
                                            AliasEditor {
                                                initial: alias,
                                                on_save: move |name| on_save_alias.call((hex_save.clone(), name)),
                                                on_cancel: move |_| on_cancel_edit.call(()),
                                            }
                                        } else {
                                            button {
                                                class: "alias-button",
                                                onclick: move |event| {
                                                    event.stop_propagation();
                                                    on_edit_alias.call(hex_edit.clone());
                                                },
                                                "{alias}"
                                            }
                                            if has_unread {
                                                span { class: "item-dot", aria_label: "new messages" }
                                            }
                                        }
                                    }
                                    if let Some(entry) = announced {
                                        div { class: "heard-body",
                                            div { class: "heard-hash", "{entry.destination_hex}" }
                                            div { class: "heard-meta",
                                                "{entry.at} · hops {entry.hops} · {entry.interface}"
                                                if is_selected { " · selected" }
                                                if has_unread { " · new" }
                                            }
                                        }
                                    } else {
                                        div { class: "heard-body",
                                            div { class: "heard-hash", "{hex}" }
                                            div { class: "heard-meta",
                                                span { class: "status-error-text",
                                                    "{MESSAGE_ONLY_NOTICE}"
                                                }
                                                if is_selected {
                                                    span { class: "heard-meta-extra", " · selected" }
                                                }
                                                if has_unread {
                                                    span { class: "heard-meta-extra", " · new message" }
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
        }
    }
}

#[component]
fn AliasEditor(
    initial: String,
    on_save: EventHandler<String>,
    on_cancel: EventHandler<()>,
) -> Element {
    let mut value = use_signal(|| initial);

    use_effect(move || {
        spawn(async move {
            sleep_ms(0).await;
            let _ = document::eval(
                "const el = document.querySelector('.alias-input'); \
                 if (el) { el.focus(); el.select(); }",
            );
        });
    });

    rsx! {
        input {
            class: "alias-input",
            r#type: "text",
            value: "{value}",
            autofocus: true,
            oninput: move |event| value.set(event.value()),
            onfocus: move |_| {
                let _ = document::eval(
                    "const el = document.activeElement; \
                     if (el instanceof HTMLInputElement) el.select();",
                );
            },
            onkeydown: move |event| {
                if event.key() == Key::Enter {
                    on_save.call(value());
                } else if event.key() == Key::Escape {
                    on_cancel.call(());
                }
            },
        }
        button {
            class: "alias-save",
            onclick: move |event| {
                event.stop_propagation();
                on_save.call(value());
            },
            "Save"
        }
    }
}

fn chat_meta_is_ok(status: &str) -> bool {
    status == "sent" || status.starts_with("received (")
}

fn space_camel_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let chars: Vec<char> = text.chars().collect();
    for (i, ch) in chars.iter().enumerate() {
        if i > 0
            && ch.is_uppercase()
            && chars
                .get(i - 1)
                .is_some_and(|prev| prev.is_lowercase() || prev.is_ascii_digit())
        {
            out.push(' ');
        }
        out.push(*ch);
    }
    out
}

fn format_error_status(status: &str) -> String {
    space_camel_case(status)
}

fn chat_meta_stamp(line: &ChatLine) -> String {
    if line.at.is_empty() {
        format_message_time()
    } else {
        line.at.clone()
    }
}

fn address_tail(hex: &str) -> String {
    let hex = hex.trim();
    if hex.is_empty() {
        "...—".to_string()
    } else if hex.len() <= 4 {
        format!("...{hex}")
    } else {
        format!("...{}", &hex[hex.len() - 4..])
    }
}

#[component]
fn ChatsTab(
    peer: String,
    peer_label: String,
    my_hex: String,
    messages: Vec<ChatLine>,
    draft: Signal<String>,
    connected: bool,
    busy: bool,
    auto_active: bool,
    auto_driving: bool,
    on_draft: EventHandler<String>,
    on_send: EventHandler<MouseEvent>,
) -> Element {
    let mut thread: Vec<ChatLine> = messages
        .into_iter()
        .filter(|m| !peer.is_empty() && m.peer_hex == peer)
        .collect();
    thread.sort_by_key(|m| m.seq);

    let can_send = connected && !peer.is_empty() && !draft().trim().is_empty() && !busy;
    let peer_tail = address_tail(&peer);
    let my_tail = address_tail(&my_hex);

    rsx! {
        div { class: "tab-pane chats-pane",
            if !peer.is_empty() {
                div { class: "chat-columns-header",
                    span { class: "chat-col-them", "{peer_label}" }
                    span { class: "chat-col-you", "You" }
                }
                div { class: "chat-address-row",
                    span { class: "chat-col-them", "{peer_tail}" }
                    span { class: "chat-col-you", "{my_tail}" }
                }
                if auto_active {
                    div { class: "auto-range-banner",
                        if auto_driving {
                            "Auto range check: sending every 10s — send stop to end"
                        } else {
                            "Auto range check: auto-replying — send stop to end"
                        }
                    }
                }
            }

            div { class: "chat-scroll", id: "chat-scroll",
                if peer.is_empty() {
                    p { class: "empty", "Select a peer in Others to open a chat." }
                } else if thread.is_empty() {
                    p { class: "empty", "No messages with this peer yet." }
                } else {
                    ul { class: "chat-list",
                        for line in thread.iter().rev() {
                            {
                                let stamp = chat_meta_stamp(line);
                                let error = if chat_meta_is_ok(&line.status) {
                                    None
                                } else {
                                    Some(format_error_status(&line.status))
                                };
                                rsx! {
                                    li {
                                        class: if line.direction == ChatDirection::Out {
                                            "chat-item out"
                                        } else {
                                            "chat-item in"
                                        },
                                        div { class: "chat-meta",
                                            span { class: "chat-meta-time", "{stamp}" }
                                            if let Some(message) = error {
                                                span { class: "chat-meta-sep", " · " }
                                                span { class: "status-error-text", "{message}" }
                                            }
                                        }
                                        div { class: "chat-text", "{line.text}" }
                                    }
                                }
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
                    } else if busy {
                        "Getting GPS…"
                    } else if auto_active {
                        "Auto ranging… send stop to end"
                    } else {
                        "Type a short text… (Range check / Auto range check)"
                    },
                    value: "{draft}",
                    disabled: !connected || peer.is_empty() || busy,
                    oninput: move |event| on_draft.call(event.value()),
                }
                button {
                    class: "primary",
                    disabled: !can_send,
                    onclick: move |event| on_send.call(event),
                    if busy { "…" } else { "Send" }
                }
            }
        }
    }
}

#[component]
fn RangeCheckModal(
    peer_label: String,
    peer_coords: String,
    auto_session: bool,
    busy: bool,
    on_accept: EventHandler<MouseEvent>,
    on_deny: EventHandler<MouseEvent>,
) -> Element {
    let title = if auto_session {
        "Auto range check"
    } else {
        "Range check"
    };
    let body = if auto_session {
        format!(
            "{peer_label} started auto range check from {peer_coords}. Auto-reply with your location every ~10s until either side sends stop?"
        )
    } else {
        format!(
            "{peer_label} sent a range check from {peer_coords}. Share your location to reply with distance?"
        )
    };
    rsx! {
        div { class: "modal-backdrop", role: "dialog", aria_modal: "true",
            div { class: "modal-sheet",
                h2 { class: "modal-title", "{title}" }
                p { class: "modal-body", "{body}" }
                div { class: "modal-actions",
                    button {
                        class: "secondary",
                        disabled: busy,
                        onclick: move |event| on_deny.call(event),
                        "Deny"
                    }
                    button {
                        class: "primary",
                        disabled: busy,
                        onclick: move |event| on_accept.call(event),
                        if busy { "Getting GPS…" } else { "Accept" }
                    }
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
