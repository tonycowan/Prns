#![forbid(unsafe_code)]

mod tree;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use dioxus::prelude::*;
use obstore::catalog::{
    entry_from_envelope, locate_store, read_catalog, read_data_prefix, read_object_record,
    CatalogEntry, ObjectPresence, ObjectRecord,
};
use obstore::protocol::{request_catalog, request_preview, socket_path};
use tree::{group_entries, group_key, parse_group_path, TreeGroup};

const PREVIEW_LIMIT: usize = 4096;

const CSS: &str = r#"
:root {
  color-scheme: dark;
  --bg: #101411;
  --panel: #171c18;
  --panel-2: #1e2420;
  --line: #2c332e;
  --text: #e7e3d6;
  --muted: #8f968b;
  --accent: #e0a15a;
  --complete: #8fbf9f;
  --partial: #e0a15a;
  --mismatch: #e07a6a;
  --empty: #8f968b;
  font-family: "Avenir Next", "Segoe UI", sans-serif;
}
* { box-sizing: border-box; }
html, body, #main { height: 100%; margin: 0; background: var(--bg); color: var(--text); }
button, input { font: inherit; color: inherit; }
button { cursor: pointer; }
button:disabled { cursor: default; opacity: 0.45; }
.app { height: 100%; display: flex; flex-direction: column; }
.top {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 16px 18px 14px;
  border-bottom: 1px solid var(--line);
  background: #141916;
}
.brand { display: flex; align-items: baseline; gap: 12px; }
.brand h1 { margin: 0; font-size: 18px; font-weight: 600; letter-spacing: -0.02em; }
.brand p { margin: 0; color: var(--muted); font-size: 13px; }
.controls { display: flex; flex-wrap: wrap; gap: 8px; }
.path {
  flex: 1;
  min-width: 0;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 8px;
  padding: 8px 10px;
  outline: none;
}
.path:focus, .filter:focus { border-color: #6d5a3e; }
.controls button {
  background: var(--panel-2);
  border: 1px solid var(--line);
  border-radius: 8px;
  padding: 8px 12px;
}
.controls button.primary { background: #3c3224; border-color: #6d5a3e; color: #f3e6d0; }
.controls button:hover:not(:disabled) { border-color: #6d5a3e; }
.notice { margin: 0; color: var(--mismatch); font-size: 13px; }
.body { flex: 1; min-height: 0; display: flex; }
.list {
  width: 440px;
  flex: none;
  border-right: 1px solid var(--line);
  display: flex;
  flex-direction: column;
  min-height: 0;
  background: var(--panel);
}
.list-head {
  display: flex;
  flex-direction: column;
  align-items: stretch;
  gap: 8px;
  padding: 12px;
  border-bottom: 1px solid var(--line);
}
.list-head span, .meta { color: var(--muted); font-size: 12px; }
.filter {
  flex: 1;
  min-width: 0;
  background: var(--bg);
  border: 1px solid var(--line);
  border-radius: 8px;
  padding: 7px 9px;
  outline: none;
}
.rows { overflow: auto; padding: 6px; }
.row {
  width: 100%;
  text-align: left;
  display: flex;
  flex-direction: column;
  gap: 3px;
  padding: 10px 10px 10px 12px;
  margin: 0 0 4px;
  border: 0;
  border-left: 2px solid transparent;
  border-radius: 8px;
  background: transparent;
}
.row:hover { background: var(--panel-2); }
.row.selected { background: #26241d; border-left-color: var(--accent); }
.group {
  width: 100%;
  display: flex;
  align-items: baseline;
  gap: 8px;
  text-align: left;
  background: transparent;
  border: 0;
  border-radius: 8px;
  padding: 6px 8px;
  margin: 0 0 2px;
  color: var(--text);
}
.group:hover { background: var(--panel-2); }
.twist { width: 12px; flex: none; color: var(--muted); font-size: 10px; }
.group .value { font-weight: 600; }
.group .none-value { color: var(--muted); font-weight: 500; }
.tree-id {
  width: 100%;
  display: block;
  text-align: left;
  background: transparent;
  border: 0;
  border-left: 2px solid transparent;
  border-radius: 6px;
  padding: 4px 8px;
  margin: 0 0 2px;
  font-family: "SF Mono", ui-monospace, Menlo, monospace;
  font-size: 12px;
}
.tree-id:hover { background: var(--panel-2); }
.tree-id.selected { background: #26241d; border-left-color: var(--accent); }
.row-top, .claim-row { display: flex; justify-content: space-between; gap: 12px; }
.id { font-family: "SF Mono", ui-monospace, Menlo, monospace; font-size: 13px; }
.summary { color: var(--text); font-size: 13px; }
.presence { font-size: 11px; letter-spacing: 0.04em; text-transform: uppercase; }
.complete { color: var(--complete); }
.partial { color: var(--partial); }
.mismatch { color: var(--mismatch); }
.empty { color: var(--empty); }
.detail { flex: 1; min-width: 0; overflow: auto; padding: 22px 28px 40px; }
.detail h2 {
  margin: 0 0 8px;
  font-family: "SF Mono", ui-monospace, Menlo, monospace;
  font-size: 15px;
  font-weight: 500;
  line-height: 1.45;
  word-break: break-all;
}
.facts { display: flex; flex-wrap: wrap; gap: 8px; margin: 14px 0 22px; }
.pill {
  border: 1px solid var(--line);
  border-radius: 999px;
  padding: 4px 10px;
  color: var(--muted);
  font-size: 12px;
}
.section { margin: 0 0 22px; }
.section h3 {
  margin: 0 0 8px;
  font-size: 12px;
  font-weight: 600;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--muted);
}
.claims { margin: 0; }
.claim-row { padding: 7px 0; border-top: 1px solid var(--line); font-size: 14px; }
.claim-row dt { color: var(--muted); }
.claim-row dd { margin: 0; text-align: right; word-break: break-all; }
pre {
  margin: 0;
  padding: 12px;
  overflow: auto;
  max-height: 280px;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 8px;
  font-family: "SF Mono", ui-monospace, Menlo, monospace;
  font-size: 12px;
  line-height: 1.5;
  white-space: pre-wrap;
  word-break: break-word;
}
.empty-copy { color: var(--muted); line-height: 1.5; max-width: 46rem; }
.empty-copy h2 {
  font-family: "Avenir Next", "Segoe UI", sans-serif;
  font-size: 22px;
  font-weight: 600;
  color: var(--text);
}
"#;

#[derive(Clone)]
struct Session {
    summary: String,
    source: SessionSource,
    entries: Vec<CatalogEntry>,
}

#[derive(Clone)]
enum SessionSource {
    Local(PathBuf),
    Remote {
        socket: PathBuf,
        destination: String,
        envelopes: HashMap<String, String>,
        previews: HashMap<String, Vec<u8>>,
    },
}

#[derive(Clone, PartialEq)]
struct Detail {
    entry: CatalogEntry,
    record: ObjectRecord,
    prefix: Option<Vec<u8>>,
    remote: bool,
}

fn main() {
    dioxus::LaunchBuilder::new()
        .with_cfg(
            dioxus::desktop::Config::new().with_window(
                dioxus::desktop::WindowBuilder::new()
                    .with_title("Object store")
                    .with_inner_size(dioxus::desktop::LogicalSize::new(1120.0, 760.0)),
            ),
        )
        .launch(App);
}

#[component]
fn App() -> Element {
    let mut path_text = use_signal(String::new);
    let mut destination_text = use_signal(String::new);
    let busy = use_signal(|| false);
    let notice = use_signal(|| None::<String>);
    let session = use_signal(|| None::<Session>);
    let mut filter = use_signal(String::new);
    let mut group_path = use_signal(String::new);
    let mut expanded = use_signal(HashSet::<String>::new);
    let selected = use_signal(|| None::<String>);
    let mut booted = use_signal(|| false);
    let fields = OpenFields {
        path_text,
        destination_text,
        notice,
        session,
        selected,
        busy,
    };

    use_effect(move || {
        if booted() {
            return;
        }
        booted.set(true);
        if let Some(path) = std::env::args().nth(1) {
            begin_open(path, String::new(), false, fields);
        }
    });

    let current = session();
    let summary = current
        .as_ref()
        .map(|loaded| loaded.summary.clone())
        .unwrap_or_else(|| "Read a local object store".to_string());
    let total = current.as_ref().map(|loaded| loaded.entries.len());
    let query = filter().trim().to_ascii_lowercase();
    let claims = parse_group_path(&group_path());
    let shown = current.as_ref().map(|loaded| {
        loaded
            .entries
            .iter()
            .filter(|entry| query.is_empty() || entry_matches(entry, &query))
            .cloned()
            .collect::<Vec<_>>()
    });
    let tree = if claims.is_empty() {
        None
    } else {
        shown
            .as_ref()
            .map(|entries| group_entries(entries, &claims))
    };
    let selected_id = selected();
    let row_selection = selected_id.clone();
    let (open_detail, open_detail_error) = match (&current, &selected_id) {
        (Some(loaded), Some(id)) => match detail_for(loaded, id) {
            Ok(detail) => (Some(detail), None),
            Err(error) => (None, Some(error)),
        },
        _ => (None, None),
    };
    let notice_text = notice();
    let shown_count = shown.as_ref().map(Vec::len);
    let opened = current.is_some();

    rsx! {
        document::Title { "Object store" }
        style { "{CSS}" }
        div { class: "app",
            header { class: "top",
                div { class: "brand",
                    h1 { "Object store" }
                    p { "{summary}" }
                }
                div { class: "controls",
                    input {
                        class: "path",
                        spellcheck: false,
                        placeholder: "Stack config directory, store root, or data directory",
                        value: "{path_text}",
                        oninput: move |event| path_text.set(event.value()),
                        onkeydown: move |event| {
                            if event.key() == Key::Enter {
                                begin_open(
                                    path_text(),
                                    destination_text(),
                                    false,
                                    fields,
                                );
                            }
                        },
                    }
                    input {
                        class: "path",
                        spellcheck: false,
                        placeholder: "Remote address, 32 hex characters",
                        value: "{destination_text}",
                        oninput: move |event| destination_text.set(event.value()),
                        onkeydown: move |event| {
                            if event.key() == Key::Enter {
                                begin_open(
                                    path_text(),
                                    destination_text(),
                                    false,
                                    fields,
                                );
                            }
                        },
                    }
                    button {
                        class: "primary",
                        disabled: busy(),
                        onclick: move |_| {
                            begin_open(
                                path_text(),
                                destination_text(),
                                false,
                                fields,
                            );
                        },
                        "Open"
                    }
                    button {
                        disabled: busy(),
                        onclick: move |_| {
                            if let Some(path) = pick_folder(path_text().trim()) {
                                begin_open(
                                    path.display().to_string(),
                                    destination_text(),
                                    false,
                                    fields,
                                );
                            }
                        },
                        "Browse"
                    }
                    button {
                        disabled: !opened || busy(),
                        onclick: move |_| {
                            begin_open(
                                path_text(),
                                destination_text(),
                                true,
                                fields,
                            );
                        },
                        "Refresh"
                    }
                }
                if let Some(message) = notice_text {
                    p { class: "notice", "{message}" }
                }
            }
            div { class: "body",
                if let Some(entries) = shown {
                    aside { class: "list",
                        div { class: "list-head",
                            span { {list_count(shown_count.unwrap_or(0), total.unwrap_or(0))} }
                            input {
                                class: "filter",
                                spellcheck: false,
                                placeholder: "Group by /object-type/board/",
                                value: "{group_path}",
                                oninput: move |event| {
                                    group_path.set(event.value());
                                    expanded.set(HashSet::new());
                                },
                            }
                            input {
                                class: "filter",
                                spellcheck: false,
                                placeholder: "Filter by id or claim",
                                value: "{filter}",
                                oninput: move |event| filter.set(event.value()),
                            }
                        }
                        div { class: "rows",
                            if entries.is_empty() {
                                p { class: "empty-copy", style: "padding: 12px;", "No objects to show." }
                            } else if let Some(groups) = tree {
                                ClaimTree {
                                    groups,
                                    selected_id,
                                    expanded,
                                    selected,
                                    session,
                                    notice,
                                }
                            } else {
                                for entry in entries {
                                    ObjectRow {
                                        key: "{entry.id}",
                                        entry: entry.clone(),
                                        selected: row_selection.as_deref() == Some(entry.id.as_str()),
                                        on_select: move |id: String| {
                                            choose_object(id, session, selected, notice);
                                        },
                                    }
                                }
                            }
                        }
                    }
                    main { class: "detail",
                        if let Some(detail) = open_detail {
                            ObjectView { detail }
                        } else if let Some(error) = open_detail_error {
                            p { class: "notice", "{error}" }
                        } else {
                            p { class: "empty-copy", "Select an object to read its claims, documents, and a short preview of the stored bytes." }
                        }
                    }
                } else {
                    main { class: "detail",
                        div { class: "empty-copy",
                            h2 { "Open a store" }
                            p { "Point this browser at a stack config directory, the object store root, or that store's data directory. A config directory is the one that contains the stack config file. The store root is the object-store-directory. The data directory sits inside it." }
                            p { "Nothing is imported, signed, or created. Refresh rereads the directory after the object service writes. To browse another stack, put its object-transfer address in the remote field. The path is then the config directory of the local object-services process." }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ClaimTree(
    groups: Vec<TreeGroup>,
    selected_id: Option<String>,
    expanded: Signal<HashSet<String>>,
    selected: Signal<Option<String>>,
    session: Signal<Option<Session>>,
    notice: Signal<Option<String>>,
) -> Element {
    rsx! {
        TreeLevel {
            groups,
            depth: 0,
            prefix: String::new(),
            selected_id,
            expanded,
            selected,
            session,
            notice,
        }
    }
}

#[component]
fn TreeLevel(
    groups: Vec<TreeGroup>,
    depth: usize,
    prefix: String,
    selected_id: Option<String>,
    expanded: Signal<HashSet<String>>,
    selected: Signal<Option<String>>,
    session: Signal<Option<Session>>,
    notice: Signal<Option<String>>,
) -> Element {
    rsx! {
        for group in groups {
            GroupBlock {
                key: "{group_key(&prefix, &group)}",
                group,
                depth,
                prefix: prefix.clone(),
                selected_id: selected_id.clone(),
                expanded,
                selected,
                session,
                notice,
            }
        }
    }
}

#[component]
fn GroupBlock(
    group: TreeGroup,
    depth: usize,
    prefix: String,
    selected_id: Option<String>,
    expanded: Signal<HashSet<String>>,
    selected: Signal<Option<String>>,
    session: Signal<Option<Session>>,
    notice: Signal<Option<String>>,
) -> Element {
    let node_key = group_key(&prefix, &group);
    let label = group.label().to_string();
    let value_class = if group.is_missing() {
        "value none-value"
    } else {
        "value"
    };
    let open = expanded.read().contains(&node_key);
    let child_groups = group.groups.clone();
    let objects = group.objects.clone();
    let indent = 8 + depth * 16;
    let object_indent = 8 + (depth + 1) * 16;
    let twist = if open { "▾" } else { "▸" };
    let toggle_key = node_key.clone();
    let child_prefix = node_key.clone();

    rsx! {
        button {
            class: "group",
            style: "padding-left: {indent}px",
            onclick: move |_| toggle_group(&toggle_key, expanded),
            span { class: "twist", "{twist}" }
            span { class: "{value_class}", "{label}" }
        }
        if open {
            if !child_groups.is_empty() {
                TreeLevel {
                    groups: child_groups,
                    depth: depth + 1,
                    prefix: child_prefix.clone(),
                    selected_id: selected_id.clone(),
                    expanded,
                    selected,
                    session,
                    notice,
                }
            }
            for entry in objects {
                TreeObject {
                    key: "{entry.id}",
                    entry: entry.clone(),
                    inset: object_indent,
                    selected: selected_id.as_deref() == Some(entry.id.as_str()),
                    on_select: move |id: String| {
                        choose_object(id, session, selected, notice);
                    },
                }
            }
        }
    }
}

fn toggle_group(key: &str, mut expanded: Signal<HashSet<String>>) {
    let mut set = expanded();
    if set.contains(key) {
        set.remove(key);
    } else {
        set.insert(key.to_string());
    }
    expanded.set(set);
}

#[component]
fn TreeObject(
    entry: CatalogEntry,
    inset: usize,
    selected: bool,
    on_select: EventHandler<String>,
) -> Element {
    let id = entry.id.to_string();
    let short = short_id(&id);
    let class = if selected {
        "tree-id selected"
    } else {
        "tree-id"
    };
    rsx! {
        button {
            class: "{class}",
            style: "padding-left: {inset}px",
            title: "{id}",
            onclick: move |_| on_select.call(id.clone()),
            "{short}"
        }
    }
}

#[component]
fn ObjectRow(entry: CatalogEntry, selected: bool, on_select: EventHandler<String>) -> Element {
    let id = entry.id.to_string();
    let short = short_id(&id);
    let summary = summary_line(&entry);
    let length = display_length(&entry);
    let presence = presence_name(entry.presence);
    let label = presence_label(entry.presence);
    let class = if selected {
        "row selected".to_string()
    } else {
        "row".to_string()
    };
    rsx! {
        button {
            class: "{class}",
            onclick: move |_| on_select.call(id.clone()),
            div { class: "row-top",
                span { class: "id", "{short}" }
                span { class: "presence {presence}", "{label}" }
            }
            if !summary.is_empty() {
                span { class: "summary", "{summary}" }
            }
            span { class: "meta", "{length}" }
        }
    }
}

#[component]
fn ObjectView(detail: Detail) -> Element {
    let id = detail.entry.id.to_string();
    let presence = presence_name(detail.entry.presence);
    let label = presence_label(detail.entry.presence);
    let length = display_length(&detail.entry);
    let piece_count = detail.entry.piece_count;
    let has_loa = detail.entry.has_loa_envelope;
    let has_transfer_manifest = detail.entry.has_transfer_manifest;
    let has_transfer_envelope = detail.entry.has_transfer_envelope;
    let claims = detail.entry.claims;
    let manifest = detail.record.manifest;
    let loa_envelope = detail.record.loa_envelope;
    let transfer_manifest = detail.record.transfer_manifest;
    let transfer_envelope = detail.record.transfer_envelope;
    let prefix = detail.prefix.clone().unwrap_or_default();
    let missing_data = detail.prefix.is_none();
    let remote = detail.remote;
    let missing_label = if remote {
        "Preview was not loaded."
    } else {
        "No data file yet."
    };
    let preview_text = if missing_data {
        None
    } else if let Some(text) = text_preview(&prefix) {
        Some(text)
    } else {
        Some(hex_preview(&prefix))
    };
    let caption = preview_caption(prefix.len(), detail.entry.data_length).unwrap_or_else(|| {
        if missing_data {
            String::new()
        } else {
            "Bytes".to_string()
        }
    });

    rsx! {
        h2 { "{id}" }
        div { class: "facts",
            span { class: "pill presence {presence}", "{label}" }
            span { class: "pill", "{length}" }
            if piece_count > 0 {
                span { class: "pill", {format!("{piece_count} pieces")} }
            }
            if has_loa {
                span { class: "pill", "LOA envelope" }
            }
            if has_transfer_manifest {
                span { class: "pill", "Transfer manifest" }
            }
            if has_transfer_envelope {
                span { class: "pill", "Transfer envelope" }
            }
        }
        if !claims.is_empty() {
            section { class: "section",
                h3 { "Claims" }
                dl { class: "claims",
                    for claim in claims {
                        ClaimRow { name: claim.0.clone(), value: claim.1.clone() }
                    }
                }
            }
        }
        Document { title: "Manifest", text: manifest }
        Document { title: "LOA envelope", text: loa_envelope }
        Document { title: "Transfer manifest", text: transfer_manifest }
        Document { title: "Transfer envelope", text: transfer_envelope }
        section { class: "section",
            h3 { "Data" }
            if missing_data {
                p { class: "empty-copy", "{missing_label}" }
            } else if let Some(body) = preview_text {
                p { class: "meta", "{caption}" }
                pre { "{body}" }
            }
        }
    }
}

#[component]
fn ClaimRow(name: String, value: String) -> Element {
    rsx! {
        div { class: "claim-row", key: "{name}",
            dt { "{name}" }
            dd { "{value}" }
        }
    }
}

#[component]
fn Document(title: &'static str, text: Option<String>) -> Element {
    let Some(text) = text else {
        return rsx! { "" };
    };
    rsx! {
        section { class: "section",
            h3 { "{title}" }
            pre { "{text}" }
        }
    }
}

struct Opened {
    session: Session,
    selected: Option<String>,
    notice: Option<String>,
}

#[derive(Clone, Copy)]
struct OpenFields {
    path_text: Signal<String>,
    destination_text: Signal<String>,
    notice: Signal<Option<String>>,
    session: Signal<Option<Session>>,
    selected: Signal<Option<String>>,
    busy: Signal<bool>,
}

fn begin_open(raw: String, destination: String, keep_selection: bool, mut fields: OpenFields) {
    if (fields.busy)() {
        return;
    }
    let trimmed = raw.trim().to_string();
    let destination = destination.trim().to_string();
    if trimmed.is_empty() {
        fields.notice.set(Some(
            "Choose a stack config directory, an object store root, or a data directory."
                .to_string(),
        ));
        return;
    }
    let keep = if keep_selection {
        (fields.selected)()
    } else {
        None
    };
    let remote = !destination.is_empty();
    fields.busy.set(true);
    fields.notice.set(Some(if remote {
        "Opening the remote store.".to_string()
    } else {
        "Opening the store.".to_string()
    }));
    let path_for_load = trimmed.clone();
    let destination_for_load = destination.clone();
    spawn(async move {
        let loaded = tokio::task::spawn_blocking(move || {
            open_blocking(path_for_load, destination_for_load, keep)
        })
        .await;
        fields.busy.set(false);
        match loaded {
            Ok(Ok(opened)) => {
                fields.path_text.set(trimmed);
                fields.destination_text.set(destination);
                fields.selected.set(opened.selected);
                fields.notice.set(opened.notice);
                fields.session.set(Some(opened.session));
            }
            Ok(Err(error)) => fields.notice.set(Some(error)),
            Err(_) => fields
                .notice
                .set(Some("the store browser stopped while opening".to_string())),
        }
    });
}

fn open_blocking(
    path: String,
    destination: String,
    keep: Option<String>,
) -> Result<Opened, String> {
    let mut loaded = if destination.is_empty() {
        load_session(Path::new(&path))?
    } else {
        load_remote(Path::new(&path), &destination)?
    };
    let selected = keep.filter(|id| loaded.entries.iter().any(|entry| entry.id.as_str() == id));
    let notice = if let Some(id) = &selected {
        ensure_preview(&mut loaded, id).err()
    } else {
        None
    };
    Ok(Opened {
        session: loaded,
        selected,
        notice,
    })
}

fn choose_object(
    id: String,
    mut session: Signal<Option<Session>>,
    mut selected: Signal<Option<String>>,
    mut notice: Signal<Option<String>>,
) {
    let Some(loaded) = session() else {
        selected.set(Some(id));
        return;
    };
    if !matches!(loaded.source, SessionSource::Remote { .. }) {
        selected.set(Some(id));
        return;
    }
    selected.set(Some(id.clone()));
    spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            let mut loaded = loaded;
            ensure_preview(&mut loaded, &id).map(|()| loaded)
        })
        .await;
        match result {
            Ok(Ok(loaded)) => {
                session.set(Some(loaded));
                notice.set(None);
            }
            Ok(Err(error)) => notice.set(Some(error)),
            Err(_) => notice.set(Some(
                "the store browser stopped while fetching the preview".to_string(),
            )),
        }
    });
}

fn ensure_preview(session: &mut Session, id: &str) -> Result<(), String> {
    let (socket, destination, already) = match &session.source {
        SessionSource::Remote {
            socket,
            destination,
            previews,
            ..
        } => (
            socket.clone(),
            destination.clone(),
            previews.contains_key(id),
        ),
        SessionSource::Local(_) => return Ok(()),
    };
    if already {
        return Ok(());
    }
    let (_length, bytes) =
        request_preview(&socket, &destination, id).map_err(|error| error.to_string())?;
    if let SessionSource::Remote { previews, .. } = &mut session.source {
        previews.insert(id.to_string(), bytes);
    }
    Ok(())
}

fn pick_folder(current: &str) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title("Open object store");
    if Path::new(current).is_dir() {
        dialog = dialog.set_directory(current);
    }
    dialog.pick_folder()
}

fn load_session(path: &Path) -> Result<Session, String> {
    let location = locate_store(path).map_err(|error| error.to_string())?;
    let data_dir = location.data_dir();
    let entries = read_catalog(&data_dir).map_err(|error| error.to_string())?;
    Ok(Session {
        summary: location.summary(),
        source: SessionSource::Local(data_dir),
        entries,
    })
}

fn load_remote(config_dir: &Path, destination: &str) -> Result<Session, String> {
    let socket = socket_path(config_dir);
    if !socket.exists() {
        return Err(format!(
            "object-services is not listening on {}",
            socket.display()
        ));
    }
    let objects = request_catalog(&socket, destination).map_err(|error| error.to_string())?;
    let mut envelopes = HashMap::new();
    let mut entries = Vec::with_capacity(objects.len());
    for object in objects {
        let entry = entry_from_envelope(&object.envelope, object.length)
            .map_err(|error| error.to_string())?;
        envelopes.insert(entry.id.to_string(), object.envelope);
        entries.push(entry);
    }
    let shown = if destination.len() > 8 {
        &destination[..8]
    } else {
        destination
    };
    Ok(Session {
        summary: format!("remote {shown} via {}", config_dir.display()),
        source: SessionSource::Remote {
            socket,
            destination: destination.to_string(),
            envelopes,
            previews: HashMap::new(),
        },
        entries,
    })
}

fn detail_for(session: &Session, id: &str) -> Result<Detail, String> {
    let entry = session
        .entries
        .iter()
        .find(|entry| entry.id.as_str() == id)
        .cloned()
        .ok_or_else(|| format!("object {id} is not in the store"))?;
    match &session.source {
        SessionSource::Local(data_dir) => {
            let record =
                read_object_record(data_dir, &entry.id).map_err(|error| error.to_string())?;
            let prefix = read_data_prefix(data_dir, &entry.id, PREVIEW_LIMIT)
                .map_err(|error| error.to_string())?;
            Ok(Detail {
                entry,
                record,
                prefix,
                remote: false,
            })
        }
        SessionSource::Remote {
            envelopes,
            previews,
            ..
        } => Ok(Detail {
            record: ObjectRecord {
                manifest: None,
                loa_envelope: envelopes.get(id).cloned(),
                transfer_manifest: None,
                transfer_envelope: None,
            },
            prefix: previews.get(id).cloned(),
            entry,
            remote: true,
        }),
    }
}

fn entry_matches(entry: &CatalogEntry, query: &str) -> bool {
    entry.id.as_str().contains(query)
        || presence_label(entry.presence)
            .to_ascii_lowercase()
            .contains(query)
        || entry.claims.iter().any(|(name, value)| {
            name.to_ascii_lowercase().contains(query) || value.to_ascii_lowercase().contains(query)
        })
}

fn list_count(shown: usize, total: usize) -> String {
    if shown == total {
        match total {
            1 => "1 object".to_string(),
            count => format!("{count} objects"),
        }
    } else {
        format!("{shown} of {total}")
    }
}

fn summary_line(entry: &CatalogEntry) -> String {
    let find = |name: &str| {
        entry
            .claims
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let mut parts = Vec::new();
    if let Some(kind) = find("object-type") {
        parts.push(kind.to_string());
    }
    if let Some(name) = find("name").or_else(|| find("title")) {
        parts.push(name.to_string());
    }
    if let Some(board) = find("board") {
        parts.push(board.to_string());
    }
    if let Some(version) = find("version") {
        parts.push(version.to_string());
    }
    if parts.is_empty() {
        if let Some((name, value)) = entry.claims.first() {
            parts.push(format!("{name} {value}"));
        }
    }
    if entry.presence == ObjectPresence::Partial && entry.piece_count > 0 {
        parts.push(format!("{} pieces", entry.piece_count));
    }
    parts.join(" · ")
}

fn display_length(entry: &CatalogEntry) -> String {
    match entry.manifest_length.or(entry.data_length) {
        Some(length) => format_bytes(length),
        None => "length unknown".to_string(),
    }
}

fn format_bytes(length: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = length as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{length} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn presence_name(presence: ObjectPresence) -> &'static str {
    match presence {
        ObjectPresence::Complete => "complete",
        ObjectPresence::Partial => "partial",
        ObjectPresence::Mismatch => "mismatch",
        ObjectPresence::Empty => "empty",
    }
}

fn presence_label(presence: ObjectPresence) -> &'static str {
    match presence {
        ObjectPresence::Complete => "Complete",
        ObjectPresence::Partial => "Partial",
        ObjectPresence::Mismatch => "Mismatch",
        ObjectPresence::Empty => "Empty",
    }
}

fn short_id(id: &str) -> String {
    if id.len() < 16 {
        id.to_string()
    } else {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    }
}

fn preview_caption(shown: usize, total: Option<u64>) -> Option<String> {
    let total = total?;
    let shown = u64::try_from(shown).ok()?;
    if total > shown {
        Some(format!("First {shown} bytes of {total}"))
    } else {
        None
    }
}

fn text_preview(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() || bytes.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let characters = text.chars().count();
    if characters == 0 {
        return None;
    }
    let printable = text
        .chars()
        .filter(|character| character.is_ascii_graphic() || character.is_ascii_whitespace())
        .count();
    if printable * 10 < characters * 9 {
        return None;
    }
    Some(text.to_string())
}

fn hex_preview(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (row, chunk) in bytes.chunks(16).enumerate() {
        if row > 0 {
            out.push('\n');
        }
        out.push_str(&format!("{:04x}  ", row * 16));
        for (index, byte) in chunk.iter().enumerate() {
            if index == 8 {
                out.push(' ');
            }
            out.push_str(&format!("{byte:02x} "));
        }
        for index in chunk.len()..16 {
            if index == 8 {
                out.push(' ');
            }
            out.push_str("   ");
        }
        out.push(' ');
        for byte in chunk {
            let character = if byte.is_ascii_graphic() {
                char::from(*byte)
            } else {
                '.'
            };
            out.push(character);
        }
    }
    out
}
