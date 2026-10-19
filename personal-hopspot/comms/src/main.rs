#[cfg(target_os = "android")]
mod camera;
#[cfg(target_os = "android")]
mod lan;
#[cfg(target_os = "android")]
mod links;
mod lxmf;
mod model;
mod scan;
#[cfg(not(target_arch = "wasm32"))]
mod session;
#[cfg(target_os = "macos")]
mod viewfinder;
#[cfg(not(target_arch = "wasm32"))]
mod voice;

use dioxus::prelude::*;
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
use scan::contact_from_rgba;
use scan::{contact_from_code, contact_from_image};

#[cfg(not(target_arch = "wasm32"))]
use model::note_from_them;
#[cfg(not(target_arch = "wasm32"))]
use model::remember_call;
#[cfg(target_arch = "wasm32")]
use model::sample_people;
#[cfg(not(target_arch = "wasm32"))]
use model::settle_note;
use model::{
    add_person, can_call, code_mark, contact_code, group_by_day, offered_ways, write_note,
    NoteKind, OfferedWays, Page, Person, Reach, YOUR_PUBLIC_KEY,
};

const PAPER: &str = r#"
@import url("https://fonts.googleapis.com/css2?family=Kalam:wght@400&display=swap");
:root {
  color-scheme: light;
  --paper: #f3efe4;
  --ink: #2b2722;
  --pencil: #6e685f;
  --rule: rgba(92, 78, 58, 0.28);
  --hand: "Kalam", Noteworthy, "Segoe Script", "Segoe Print", cursive;
  --book: "Iowan Old Style", Palatino, "Palatino Linotype", "Book Antiqua", Georgia, serif;
}
* { box-sizing: border-box; }
html, body, #main {
  margin: 0;
  height: 100%;
  background: var(--paper);
  color: var(--ink);
}
button, input, textarea {
  font: inherit;
  color: inherit;
  background: transparent;
  border: 0;
  padding: 0;
}
button { cursor: pointer; }
button:focus-visible, input:focus-visible, textarea:focus-visible {
  outline: 1px solid var(--pencil);
  outline-offset: 3px;
}
.notebook {
  height: 100%;
  overflow: hidden;
  font-family: var(--book);
  background: var(--paper);
}
.page, .sheet {
  --edge-left: max(20px, env(safe-area-inset-left));
  --edge-right: max(20px, env(safe-area-inset-right));
  height: 100%;
  overflow: auto;
  padding:
    calc(max(18px, env(safe-area-inset-top)) + 22px)
    var(--edge-right)
    max(32px, env(safe-area-inset-bottom))
    var(--edge-left);
  background: var(--paper);
}
.wires {
  position: fixed;
  z-index: 3;
  top: max(14px, env(safe-area-inset-top));
  left: 0;
  right: 0;
  display: flex;
  justify-content: center;
  gap: 0.7em;
  margin: 0;
  pointer-events: none;
  font-family: var(--book);
  font-size: 15px;
  line-height: 1;
  letter-spacing: 0.08em;
}
.wires .live { color: var(--ink); }
.wires .quiet { color: var(--pencil); opacity: 0.35; }
.incoming {
  position: fixed;
  z-index: 4;
  top: calc(max(14px, env(safe-area-inset-top)) + 22px);
  left: 0;
  right: 0;
  display: flex;
  justify-content: flex-end;
  align-items: baseline;
  gap: 18px;
  margin: 0;
  padding: 2px max(20px, env(safe-area-inset-right)) 6px max(20px, env(safe-area-inset-left));
  background: var(--paper);
}
.incoming p {
  margin: 0;
  color: var(--ink);
  font-size: 22px;
  line-height: 1.2;
}
.incoming button {
  color: var(--pencil);
  font-family: var(--book);
  font-size: 16px;
  line-height: 1;
  text-decoration: underline;
  text-underline-offset: 4px;
  min-height: 44px;
}
.hand { font-family: var(--hand); }
.actions {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 6px;
}
.action {
  color: var(--pencil);
  font-size: 16px;
  line-height: 1;
  text-decoration: underline;
  text-underline-offset: 4px;
  text-decoration-thickness: 1px;
  min-height: 44px;
  padding: 12px 0;
}
.names { list-style: none; margin: 12px 0 0; padding: 0; }
.names li { border-bottom: 1px solid var(--rule); }
.names button, .save {
  display: block;
  width: 100%;
  text-align: left;
  font-size: 28px;
  line-height: 1.45;
  padding: 10px 0 8px;
}
.save {
  color: var(--pencil);
  font-size: 18px;
  text-decoration: underline;
  text-underline-offset: 4px;
  width: auto;
}
.sheet {
  position: fixed;
  inset: 0;
  background: var(--paper);
  transform: translateX(100%);
  transition: transform 240ms ease;
  pointer-events: none;
}
.sheet.open {
  transform: translateX(0);
  pointer-events: auto;
}
.title-row {
  display: flex;
  align-items: flex-start;
  gap: 14px;
  margin: 0 0 8px;
}
.who { min-width: 0; }
.title-row h1 {
  margin: 0;
  font-size: 30px;
  font-weight: 500;
}
.back { font-size: 26px; line-height: 1; margin-top: 6px; }
.via {
  margin: 1px 0 0;
  color: var(--pencil);
  font-family: var(--book);
  font-size: 13px;
  line-height: 1.2;
}
.thread { list-style: none; margin: 10px 0 0; padding: 0; }
.day {
  display: flex;
  align-items: center;
  gap: 12px;
  margin: 16px 0 12px;
  color: var(--pencil);
  font-size: 14px;
}
.day::before, .day::after {
  content: "";
  flex: 1;
  height: 1px;
  background: var(--rule);
}
.swipe {
  display: flex;
  align-items: flex-start;
  overflow: hidden;
  touch-action: pan-y;
}
.words {
  flex: 1 1 auto;
  min-width: 0;
  margin: 0 0 16px;
  font-size: 22px;
  line-height: 1.35;
}
.words.hand { font-size: 26px; }
.mine .words {
  text-align: right;
  padding-left: var(--edge-left);
}
.theirs .words {
  padding-right: var(--edge-right);
}
.voice .words, .video .words { color: var(--pencil); font-size: 18px; }
.waiting .words { color: var(--pencil); }
.when {
  flex: 0 0 var(--reveal, 0px);
  overflow: hidden;
  margin: 0 0 16px;
  transition: flex-basis 180ms ease;
}
.when span {
  display: block;
  width: 72px;
  padding: 6px 0 0 12px;
  box-sizing: border-box;
  text-align: right;
  color: var(--pencil);
  font-size: 15px;
  line-height: 1.35;
  font-variant-numeric: tabular-nums;
  transform: translateX(calc(72px - var(--reveal, 0px)));
  transition: transform 180ms ease;
}
.swipe.live .when,
.swipe.live .when span { transition: none; }
.call-line { margin-top: 8px; }
.call-line .hint {
  margin: 0 0 4px;
  text-align: right;
}
.mesh { margin: 0 0 10px; }
.mesh .hint { margin: 0 0 6px; text-align: left; }
.mesh-report {
  width: 100%;
  margin: 4px 0 0;
  border-collapse: collapse;
  color: var(--pencil);
  font-family: var(--book);
  font-size: 13px;
  line-height: 1.2;
}
.mesh-report td {
  padding: 1px 0;
  vertical-align: baseline;
}
.mesh-status {
  text-align: right;
  color: var(--pencil);
  font-variant-numeric: tabular-nums;
}
.offers {
  display: flex;
  justify-content: flex-end;
  gap: 22px;
}
.offers button {
  color: var(--pencil);
  font-size: 16px;
  line-height: 1;
  text-decoration: underline;
  text-underline-offset: 4px;
  min-height: 44px;
  padding: 12px 0;
}
.photo {
  position: relative;
  display: inline-block;
  color: var(--pencil);
  font-size: 16px;
  line-height: 1;
  text-decoration: underline;
  text-underline-offset: 4px;
  min-height: 44px;
  padding: 12px 0;
  cursor: pointer;
}
.photo input {
  position: absolute;
  inset: 0;
  opacity: 0;
  cursor: pointer;
}
.finder { margin: 4px 0 8px; }
.finder .well {
  position: relative;
  display: flex;
  align-items: center;
  justify-content: center;
  background: #1c1a17;
  aspect-ratio: 16 / 9;
  overflow: hidden;
}
.finder .opening {
  margin: 0;
  color: var(--paper);
  font-size: 15px;
}
.finder img {
  display: block;
  width: 100%;
  height: 100%;
  object-fit: cover;
  transform: scaleX(-1);
}
.finder .well::after {
  content: "";
  position: absolute;
  left: 14%;
  right: 14%;
  top: 12%;
  bottom: 12%;
  border: 1px solid var(--paper);
  border-radius: 8px;
  pointer-events: none;
}
.finder .aim {
  margin: 12px 0 0;
  text-align: center;
  color: var(--pencil);
  font-size: 15px;
}
.hint, .notice {
  margin: 0 0 18px;
  color: var(--pencil);
  font-size: 15px;
}
.composer {
  display: grid;
  margin-top: 8px;
  border-bottom: 1px solid var(--rule);
}
.composer::after {
  content: attr(data-value) " ";
  visibility: hidden;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  word-break: break-word;
  grid-area: 1 / 1;
  min-width: 0;
  max-height: calc(1.35em * 5);
  overflow: hidden;
  font-family: var(--hand);
  font-size: 26px;
  line-height: 1.35;
}
.composer textarea {
  grid-area: 1 / 1;
  min-width: 0;
  width: 100%;
  height: 100%;
  min-height: 1.35em;
  max-height: calc(1.35em * 5);
  resize: none;
  overflow-x: hidden;
  overflow-y: auto;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  word-break: break-word;
  font-family: var(--hand);
  font-size: 26px;
  line-height: 1.35;
  text-align: left;
}
.composer textarea::placeholder { color: var(--pencil); font-family: var(--hand); }
.field { margin: 18px 0; }
.field span {
  display: block;
  color: var(--pencil);
  font-size: 15px;
}
.field input {
  width: 100%;
  font-size: 24px;
  line-height: 1.4;
  border-bottom: 1px solid var(--rule);
}
.field input.hand { font-size: 28px; }
.mark {
  display: block;
  width: min(240px, 70vw);
  margin: 18px 0 8px;
  background: var(--paper);
  cursor: pointer;
  text-align: left;
}
.mark svg { display: block; width: 100%; height: auto; }
.mark rect { fill: var(--ink); }
.name-line {
  width: 100%;
  font-size: 34px;
  border-bottom: 1px solid transparent;
}
.public-key {
  display: block;
  width: 100%;
  font-size: 15px;
  line-height: 1.45;
  letter-spacing: 0.02em;
  text-align: left;
  white-space: normal;
  word-break: break-all;
  cursor: pointer;
  user-select: all;
}
"#;

fn initial_people() -> Vec<Person> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        session::opening_contacts()
    }
    #[cfg(target_arch = "wasm32")]
    {
        sample_people()
    }
}

fn initial_next_id() -> u64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        session::opening_next_id()
    }
    #[cfg(target_arch = "wasm32")]
    {
        4
    }
}

fn commit_people(mut people: Signal<Vec<Person>>, listed: Vec<Person>) {
    #[cfg(not(target_arch = "wasm32"))]
    session::save_contacts(&listed);
    people.set(listed);
}

fn main() {
    #[cfg(feature = "desktop")]
    {
        use dioxus::desktop::{Config, WindowBuilder};
        session::start_default().expect("the journal session could not start");
        dioxus::LaunchBuilder::desktop()
            .with_cfg(
                Config::new().with_window(
                    WindowBuilder::new()
                        .with_title("Awill")
                        .with_inner_size(dioxus::desktop::LogicalSize::new(440.0, 760.0)),
                ),
            )
            .launch(App);
    }
    #[cfg(all(feature = "mobile", not(feature = "desktop")))]
    {
        session::start_default().expect("the journal session could not start");
        dioxus::LaunchBuilder::mobile().launch(App);
    }
    #[cfg(all(feature = "web", not(feature = "desktop"), not(feature = "mobile")))]
    dioxus::launch(App);
}

#[derive(Clone, PartialEq, Eq)]
struct VoiceLine {
    identity: String,
    kind: u8,
    words: String,
    ticket: u64,
}

impl VoiceLine {
    fn quiet() -> Self {
        Self {
            identity: String::new(),
            kind: 0,
            words: String::new(),
            ticket: 0,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn voice_line() -> VoiceLine {
    match session::call_face() {
        session::CallFace::Quiet => VoiceLine::quiet(),
        session::CallFace::Placing { identity, outgoing } => VoiceLine {
            identity,
            kind: 1,
            words: if outgoing {
                "Calling…".to_string()
            } else {
                "Answering…".to_string()
            },
            ticket: 0,
        },
        session::CallFace::Live { identity } => VoiceLine {
            identity,
            kind: 2,
            words: "On a call".to_string(),
            ticket: 0,
        },
        session::CallFace::Failed { identity, words } => VoiceLine {
            identity,
            kind: 3,
            words: words.to_string(),
            ticket: 0,
        },
        session::CallFace::Ringing { identity, ticket } => VoiceLine {
            identity,
            kind: 4,
            words: String::new(),
            ticket,
        },
    }
}

fn this_call(person: &str, voice: &VoiceLine) -> bool {
    if voice.kind == 0 || voice.identity.is_empty() {
        return false;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        session::same_contact(person, &voice.identity)
    }
    #[cfg(target_arch = "wasm32")]
    {
        person.eq_ignore_ascii_case(&voice.identity)
    }
}

#[component]
fn IncomingBar(voice: VoiceLine, people: Vec<Person>) -> Element {
    if voice.kind != 4 {
        return rsx! {};
    }
    let name = people
        .into_iter()
        .find(|person| this_call(&person.identity, &voice))
        .map(|person| person.name)
        .unwrap_or_else(|| "Someone".to_string());
    let ticket = voice.ticket;
    rsx! {
        div { class: "incoming",
            p { class: "hand", "{name} is calling" }
            button {
                onclick: move |_| {
                    #[cfg(not(target_arch = "wasm32"))]
                    session::answer(ticket);
                },
                "answer"
            }
            button {
                onclick: move |_| {
                    #[cfg(not(target_arch = "wasm32"))]
                    session::decline(ticket);
                },
                "decline"
            }
        }
    }
}

#[allow(non_snake_case)]
fn App() -> Element {
    let mut page = use_signal(|| Page::People);
    let mut cover = use_signal(|| Page::People);
    let mut your_name = use_signal(machine_name);
    let people = use_signal(initial_people);
    let mut next_id = use_signal(initial_next_id);
    let mut draft_name = use_signal(String::new);
    let mut draft_code = use_signal(String::new);
    let mut draft_note = use_signal(String::new);
    let mut visit = use_signal(|| 0_u64);
    let mut wires = use_signal(read_wires);
    let mut voice = use_signal(VoiceLine::quiet);
    let mut reach = use_signal(Reach::unknown);

    #[cfg(not(target_arch = "wasm32"))]
    {
        static LISTENING: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if LISTENING.set(()).is_ok() {
            spawn(async move {
                loop {
                    let next = read_wires();
                    if wires() != next {
                        wires.set(next);
                    }
                    for arrived in session::drain_inbox() {
                        let mut listed = people();
                        if let Some(person) = listed.iter_mut().find(|person| {
                            session::same_contact(&person.identity, &arrived.identity)
                        }) {
                            note_from_them(person, &arrived.words, arrived.written_at);
                            commit_people(people, listed);
                        }
                    }
                    for invite in session::drain_invites() {
                        let known = people().iter().any(|person| {
                            session::same_contact(&person.identity, &invite.identity)
                        });
                        if known {
                            session::offer_incoming(invite);
                        } else {
                            session::decline(invite.ticket);
                        }
                    }
                    let acks = session::drain_acks();
                    if !acks.is_empty() {
                        let mut listed = people();
                        let mut changed = false;
                        for id in acks {
                            changed |= settle_note(&mut listed, id);
                        }
                        if changed {
                            commit_people(people, listed);
                        }
                    }
                    if let Some(done) = session::take_finished() {
                        let mut listed = people();
                        if let Some(person) = listed
                            .iter_mut()
                            .find(|person| session::same_contact(&person.identity, &done.identity))
                        {
                            remember_call(person, done.seconds, done.outgoing);
                            commit_people(people, listed);
                        }
                    }
                    let next = voice_line();
                    if voice() != next {
                        voice.set(next);
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    if let Page::Journal(id) = page() {
                        if let Some(person) = people().iter().find(|person| person.id == id) {
                            session::measure(&person.identity);
                            let next = session::reach_for(&person.identity);
                            if reach() != next {
                                reach.set(next);
                            }
                        }
                    }
                    session::pause().await;
                }
            });
        }
    }

    let open = !matches!(page(), Page::People);

    rsx! {
        style { {PAPER} }
        div { class: "notebook",
            WireLetters { wires: wires() }
            IncomingBar { voice: voice(), people: people() }
            PeoplePage {
                people: people(),
                on_open: move |id| {
                    draft_note.set(String::new());
                    visit.set(visit() + 1);
                    reach.set(Reach::unknown());
                    #[cfg(not(target_arch = "wasm32"))]
                    if let Some(person) = people().iter().find(|person| person.id == id) {
                        session::forget(&person.identity);
                    }
                    let next = Page::Journal(id);
                    cover.set(next);
                    page.set(next);
                },
                on_code: move |_| {
                    cover.set(Page::Code);
                    page.set(Page::Code);
                },
                on_add: move |_| {
                    draft_name.set(String::new());
                    draft_code.set(String::new());
                    cover.set(Page::Add);
                    page.set(Page::Add);
                },
            }
            div { class: if open { "sheet open" } else { "sheet" },
                match cover() {
                    Page::People => rsx! {},
                    Page::Code => rsx! {
                        CodePage {
                            name: your_name(),
                            public_key: your_public_key(),
                            on_name: move |name| your_name.set(name),
                            on_back: move |_| page.set(Page::People),
                        }
                    },
                    Page::Add => rsx! {
                        AddPage {
                            name: draft_name(),
                            code: draft_code(),
                            on_name: move |value| draft_name.set(value),
                            on_code: move |value| draft_code.set(value),
                            on_back: move |_| page.set(Page::People),
                            on_scanned: move |(name, identity): (String, String)| {
                                let mut listed = people();
                                let mut id = next_id();
                                if add_person(&mut listed, &mut id, &name, &identity) {
                                    commit_people(people, listed);
                                    next_id.set(id);
                                    page.set(Page::People);
                                }
                            },
                            on_save: move |_| {
                                let mut listed = people();
                                let mut id = next_id();
                                if add_person(&mut listed, &mut id, &draft_name(), &draft_code()) {
                                    commit_people(people, listed);
                                    next_id.set(id);
                                    page.set(Page::People);
                                }
                            },
                        }
                    },
                    Page::Journal(id) => {
                        let person = people().into_iter().find(|person| person.id == id);
                        match person {
                            Some(person) => rsx! {
                                JournalPage {
                                    key: "{person.id}-{visit()}",
                                    person: person.clone(),
                                    draft: draft_note(),
                                    voice: voice(),
                                    reach: reach(),
                                    on_draft: move |value| draft_note.set(value),
                                    on_back: move |_| page.set(Page::People),
                                    on_write: move |_| {
                                        let words = draft_note();
                                        let mut listed = people();
                                        if let Some(person) = listed.iter_mut().find(|person| person.id == id) {
                                            let note_id = write_note(person, &words);
                                            #[cfg(not(target_arch = "wasm32"))]
                                            if let Some(note_id) = note_id {
                                                session::send_note(&person.identity, &words, note_id);
                                            }
                                            #[cfg(target_arch = "wasm32")]
                                            let _ = note_id;
                                        }
                                        commit_people(people, listed);
                                        draft_note.set(String::new());
                                    },
                                }
                            },
                            None => rsx! {},
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Wires {
    wifi: bool,
    internet: bool,
    ble: bool,
    usb: bool,
}

fn read_wires() -> Wires {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let marks = session::wire_marks();
        return Wires {
            wifi: marks.wifi,
            internet: marks.internet,
            ble: marks.ble,
            usb: marks.usb,
        };
    }
    #[cfg(target_arch = "wasm32")]
    Wires {
        wifi: false,
        internet: false,
        ble: false,
        usb: false,
    }
}

#[component]
fn WireLetters(wires: Wires) -> Element {
    rsx! {
        div { class: "wires",
            span { class: if wires.wifi { "live" } else { "quiet" }, title: "Wifi auto", "W" }
            span { class: if wires.internet { "live" } else { "quiet" }, title: "Internet", "I" }
            span { class: if wires.ble { "live" } else { "quiet" }, title: "BLE", "B" }
            span { class: if wires.usb { "live" } else { "quiet" }, title: "USB", "U" }
        }
    }
}

#[component]
fn PeoplePage(
    people: Vec<Person>,
    on_open: EventHandler<u64>,
    on_code: EventHandler<()>,
    on_add: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "page",
            div { class: "actions",
                button { class: "action", onclick: move |_| on_add.call(()), "Add a contact" }
                button { class: "action", onclick: move |_| on_code.call(()), "Your QR code" }
            }
            ul { class: "names",
                for person in people {
                    li {
                        button {
                            onclick: move |_| on_open.call(person.id),
                            "{person.name}"
                        }
                    }
                }
            }
        }
    }
}

fn machine_name() -> String {
    #[cfg(target_os = "macos")]
    {
        return "Mac".to_string();
    }
    #[cfg(target_os = "ios")]
    {
        return "iPhone".to_string();
    }
    #[cfg(target_os = "android")]
    {
        return session::machine_name();
    }
    #[cfg(target_os = "windows")]
    {
        return "PC".to_string();
    }
    #[cfg(target_arch = "wasm32")]
    {
        return "Browser".to_string();
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "android",
        target_os = "windows",
        target_arch = "wasm32",
    )))]
    {
        "Computer".to_string()
    }
}

#[component]
fn your_public_key() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(key) = session::PUBLIC_KEY.get() {
        return key.clone();
    }
    YOUR_PUBLIC_KEY.to_string()
}

#[component]
fn CodePage(
    name: String,
    public_key: String,
    on_name: EventHandler<String>,
    on_back: EventHandler<()>,
) -> Element {
    let payload = contact_code(&name, &public_key);
    let mark = code_mark(&payload);
    let copied = use_signal(|| false);
    let share = payload.clone();
    let key = public_key.clone();
    rsx! {
        div { class: "page",
            div { class: "title-row",
                button { class: "back", onclick: move |_| on_back.call(()), "←" }
                h1 { "Your QR code" }
            }
            input {
                class: "name-line hand",
                aria_label: "Your name",
                value: "{name}",
                oninput: move |event| on_name.call(event.value()),
            }
            Mark {
                mark,
                on_copy: move |_| copy_text(share.clone(), copied),
            }
            div { class: "field",
                span { "Public key" }
                button {
                    class: "public-key",
                    onclick: move |_| copy_text(key.clone(), copied),
                    "{public_key}"
                }
            }
            if copied() {
                p { class: "notice", "Copied" }
            }
        }
    }
}

#[component]
fn Mark(mark: Option<(usize, Vec<(usize, usize)>)>, on_copy: EventHandler<()>) -> Element {
    let Some((width, cells)) = mark else {
        return rsx! {};
    };
    rsx! {
        button {
            class: "mark",
            aria_label: "Copy your QR code",
            onclick: move |_| on_copy.call(()),
            svg {
                view_box: "0 0 {width} {width}",
                xmlns: "http://www.w3.org/2000/svg",
                for (x, y) in cells {
                    rect { x: "{x}", y: "{y}", width: "1", height: "1" }
                }
            }
        }
    }
}

fn copy_text(text: String, mut copied: Signal<bool>) {
    #[cfg(target_arch = "wasm32")]
    {
        spawn(async move {
            if write_clipboard(&text).await {
                copied.set(true);
            }
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if write_clipboard(&text) {
            copied.set(true);
        }
    }
}

#[cfg(target_arch = "wasm32")]
async fn write_clipboard(text: &str) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    wasm_bindgen_futures::JsFuture::from(window.navigator().clipboard().write_text(text))
        .await
        .is_ok()
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn write_clipboard(text: &str) -> bool {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .is_ok()
}

#[cfg(target_os = "android")]
fn write_clipboard(text: &str) -> bool {
    session::copy_to_clipboard(text)
}

#[component]
fn PhotoControl(on_scanned: EventHandler<(String, String)>, mut notice: Signal<String>) -> Element {
    #[cfg(target_os = "macos")]
    {
        let mut scanning = use_signal(|| false);
        let mut frame = use_signal(String::new);
        use_drop(|| viewfinder::stop());
        let picture = frame();
        return rsx! {
            if scanning() {
                div { class: "finder",
                    div { class: "well",
                        if picture.is_empty() {
                            p { class: "opening", "Opening the camera…" }
                        } else {
                            img { src: "{picture}", alt: "Camera" }
                        }
                    }
                    p { class: "aim", "Point at their code" }
                    button {
                        class: "photo",
                        onclick: move |_| {
                            viewfinder::stop();
                            scanning.set(false);
                            frame.set(String::new());
                        },
                        "Close the camera"
                    }
                }
            } else {
                button {
                    class: "photo",
                    aria_label: "Scan their QR code",
                    onclick: move |_| {
                        notice.set(String::new());
                        frame.set(String::new());
                        scanning.set(true);
                        let token = viewfinder::start();
                        let on_scanned = on_scanned;
                        spawn(async move {
                            loop {
                                if viewfinder::token() != token {
                                    break;
                                }
                                match viewfinder::look() {
                                    viewfinder::View::Picture(url) => {
                                        if frame() != url {
                                            frame.set(url);
                                        }
                                    }
                                    viewfinder::View::Code(text) => {
                                        viewfinder::stop();
                                        scanning.set(false);
                                        frame.set(String::new());
                                        if let Some(contact) = contact_from_code(&text) {
                                            on_scanned.call(contact);
                                        } else {
                                            notice.set(UNREADABLE.into());
                                        }
                                        break;
                                    }
                                    viewfinder::View::Denied => {
                                        viewfinder::stop();
                                        scanning.set(false);
                                        frame.set(String::new());
                                        notice.set("Awill needs the camera to photograph a code.".into());
                                        break;
                                    }
                                    viewfinder::View::Unavailable => {
                                        viewfinder::stop();
                                        scanning.set(false);
                                        frame.set(String::new());
                                        notice.set("The camera didn't open.".into());
                                        break;
                                    }
                                    viewfinder::View::Starting => {}
                                }
                                session::rest(80).await;
                            }
                        });
                    },
                    "Scan their code"
                }
            }
        };
    }
    #[cfg(target_os = "android")]
    {
        return rsx! {
            button {
                class: "photo",
                aria_label: "Scan their QR code",
                onclick: move |_| {
                    notice.set(String::new());
                    let on_scanned = on_scanned;
                    spawn(async move {
                        match session::capture_photo().await {
                            session::PhotoCapture::Code(text) => {
                                if let Some(contact) = contact_from_code(&text) {
                                    on_scanned.call(contact);
                                } else {
                                    notice.set(UNREADABLE.into());
                                }
                            }
                            session::PhotoCapture::Denied => {
                                notice.set("Awill needs the camera to photograph a code.".into());
                            }
                            session::PhotoCapture::Unavailable => {
                                notice.set("The camera didn't open.".into());
                            }
                            session::PhotoCapture::Cancelled => {}
                        }
                    });
                },
                "Scan their code"
            }
        };
    }
    #[cfg(not(any(target_os = "android", target_os = "macos")))]
    rsx! {
        label { class: "photo",
            "Take a photo"
            input {
                r#type: "file",
                accept: "image/*",
                capture: "environment",
                aria_label: "Take a photo of their QR code",
                onchange: move |event| {
                    let Some(file) = event.files().into_iter().next() else {
                        return;
                    };
                    let on_scanned = on_scanned;
                    let mut notice = notice;
                    spawn(async move {
                        let Ok(bytes) = file.read_bytes().await else {
                            notice.set(UNREADABLE.into());
                            return;
                        };
                        accept_image(&bytes, on_scanned, notice);
                    });
                },
            }
        }
    }
}

#[component]
fn AddPage(
    name: String,
    code: String,
    on_name: EventHandler<String>,
    on_code: EventHandler<String>,
    on_back: EventHandler<()>,
    on_scanned: EventHandler<(String, String)>,
    on_save: EventHandler<()>,
) -> Element {
    let notice = use_signal(String::new);
    let message = notice();
    rsx! {
        div {
            class: "page",
            tabindex: "0",
            onmounted: move |event| {
                spawn(async move {
                    let _ = event.set_focus(true).await;
                });
            },
            onpaste: move |event| accept_paste(event, on_scanned, notice),
            div { class: "title-row",
                button { class: "back", aria_label: "Back to contacts", onclick: move |_| on_back.call(()), "←" }
                h1 { "Add a contact" }
            }
            PhotoControl { on_scanned, notice }
            p { class: "hint", "Or paste a photo of their QR code." }
            if !message.is_empty() {
                p { class: "notice", "{message}" }
            }
            label { class: "field",
                span { "Name" }
                input {
                    class: "hand",
                    value: "{name}",
                    oninput: move |event| on_name.call(event.value()),
                }
            }
            label { class: "field",
                span { "Public key" }
                input {
                    value: "{code}",
                    oninput: move |event| on_code.call(event.value()),
                }
            }
            button { class: "save", onclick: move |_| on_save.call(()), "Add to your contacts" }
        }
    }
}

const UNREADABLE: &str = "That photo doesn't show a code.";

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn accept_image(
    bytes: &[u8],
    on_scanned: EventHandler<(String, String)>,
    mut notice: Signal<String>,
) {
    match contact_from_image(bytes) {
        Some((name, identity)) => on_scanned.call((name, identity)),
        None => notice.set(UNREADABLE.into()),
    }
}

fn accept_paste(
    event: Event<ClipboardData>,
    on_scanned: EventHandler<(String, String)>,
    mut notice: Signal<String>,
) {
    event.prevent_default();
    if let Some(text) = pasted_text(&event) {
        if let Some((name, identity)) = contact_from_code(&text) {
            on_scanned.call((name, identity));
            return;
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let Some(blob) = pasted_image_blob(&event) else {
            notice.set(UNREADABLE.into());
            return;
        };
        spawn(async move {
            let Some(bytes) = read_blob(blob).await else {
                notice.set(UNREADABLE.into());
                return;
            };
            accept_image(&bytes, on_scanned, notice);
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = event;
        match native_clipboard_contact() {
            Some((name, identity)) => on_scanned.call((name, identity)),
            None => notice.set(UNREADABLE.into()),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn pasted_text(_event: &Event<ClipboardData>) -> Option<String> {
    None
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn native_clipboard_contact() -> Option<(String, String)> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    if let Ok(text) = clipboard.get_text() {
        if let Some(contact) = contact_from_code(&text) {
            return Some(contact);
        }
    }
    let image = clipboard.get_image().ok()?;
    contact_from_rgba(image.width as u32, image.height as u32, &image.bytes)
}

#[cfg(target_os = "android")]
fn native_clipboard_contact() -> Option<(String, String)> {
    let contents = session::read_clipboard()?;
    if let Some(text) = contents.text.as_deref() {
        if let Some(contact) = contact_from_code(text) {
            return Some(contact);
        }
    }
    contact_from_image(contents.image.as_deref()?)
}

#[cfg(target_arch = "wasm32")]
fn pasted_text(event: &Event<ClipboardData>) -> Option<String> {
    let data = clipboard_data(event)?;
    let text = data.get_data("text/plain").ok()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(target_arch = "wasm32")]
fn pasted_image_blob(event: &Event<ClipboardData>) -> Option<web_sys::Blob> {
    let data = clipboard_data(event)?;
    let items = data.items();
    for index in 0..items.length() {
        let item = items.get(index)?;
        if item.kind() == "file" && item.type_().starts_with("image/") {
            let file = item.get_as_file().ok()??;
            return Some(file.into());
        }
    }
    None
}

#[cfg(target_arch = "wasm32")]
fn clipboard_data(event: &Event<ClipboardData>) -> Option<web_sys::DataTransfer> {
    use wasm_bindgen::JsCast;
    let raw = event.data().downcast::<web_sys::Event>()?.clone();
    let clipboard = raw.dyn_into::<web_sys::ClipboardEvent>().ok()?;
    clipboard.clipboard_data()
}

#[cfg(target_arch = "wasm32")]
async fn read_blob(blob: web_sys::Blob) -> Option<Vec<u8>> {
    let buffer = wasm_bindgen_futures::JsFuture::from(blob.array_buffer())
        .await
        .ok()?;
    Some(js_sys::Uint8Array::new(&buffer).to_vec())
}

#[component]
fn JournalPage(
    person: Person,
    draft: String,
    voice: VoiceLine,
    reach: Reach,
    on_draft: EventHandler<String>,
    on_back: EventHandler<()>,
    on_write: EventHandler<()>,
) -> Element {
    let name = person.name.clone();
    let groups = group_by_day(&person.notes);
    let real = person.identity.trim().len() == 128;
    let ways = if real {
        if reach.pending {
            OfferedWays::none()
        } else {
            reach.ways
        }
    } else {
        offered_ways(person.id)
    };
    let can_voice = ways.voice && can_call(&person);
    let identity = person.identity.clone();
    let on_this = this_call(&identity, &voice);
    let kind = voice.kind;
    let ending = on_this && (kind == 1 || kind == 2);
    let label = if ending { "end" } else { "voice" };
    let hint = if on_this && kind == 4 {
        "is calling".to_string()
    } else if on_this && kind != 0 {
        voice.words.clone()
    } else if real && !reach.pending && !ways.note && !ways.voice && !ways.video {
        "Can't reach them from here.".to_string()
    } else {
        String::new()
    };
    let searching = real && reach.pending;
    let lines = reach.lines.clone();
    let mut writing = use_signal(|| false);
    let mut picked = use_signal(|| false);
    let mut show_board = use_signal(|| true);
    let mut board_clock = use_signal(|| false);
    if reach.winner.is_some() && !board_clock() {
        board_clock.set(true);
        #[cfg(not(target_arch = "wasm32"))]
        spawn(async move {
            session::rest(1_000).await;
            show_board.set(false);
        });
        #[cfg(target_arch = "wasm32")]
        show_board.set(false);
    }
    let under_name = if let Some(winner) = reach.winner.clone() {
        winner
    } else if searching {
        "searching the mesh".to_string()
    } else {
        String::new()
    };
    rsx! {
        div { class: "page",
            div { class: "title-row",
                button { class: "back", aria_label: "Back to contacts", onclick: move |_| on_back.call(()), "←" }
                div { class: "who",
                    h1 { "{name}" }
                    if !under_name.is_empty() {
                        p { class: "via", "{under_name}" }
                    }
                    if show_board() && !picked() && !lines.is_empty() {
                        table { class: "mesh-report",
                            for line in lines {
                                tr {
                                    td { "{line.name}" }
                                    td { class: "mesh-status", "{line.status}" }
                                }
                            }
                        }
                    }
                }
            }
            ul { class: "thread",
                for (day, notes) in groups {
                    li { class: "day", "{day}" }
                    for note in notes {
                        NoteRow {
                            kind: note.kind,
                            words: note.words,
                            time: note.time,
                            mine: note.mine,
                            settled: note.settled,
                        }
                    }
                }
            }
            if writing() {
                div { class: "composer", "data-value": "{draft}",
                    textarea {
                        class: "hand",
                        aria_label: "Write a note",
                        placeholder: "Write a note",
                        rows: "1",
                        value: "{draft}",
                        onmounted: move |event| {
                            spawn(async move {
                                let _ = event.set_focus(true).await;
                            });
                        },
                        oninput: move |event| on_draft.call(event.value()),
                        onkeydown: move |event| {
                            if event.key() == Key::Enter && !event.modifiers().shift() {
                                event.prevent_default();
                                on_write.call(());
                                writing.set(false);
                            }
                        },
                    }
                }
            } else {
                div { class: "call-line",
                    if !hint.is_empty() {
                        p { class: "hint", "{hint}" }
                    }
                    div { class: "offers",
                        if ways.note {
                            button {
                                onclick: move |_| {
                                    picked.set(true);
                                    writing.set(true);
                                },
                                "note"
                            }
                        }
                        if can_voice && !(on_this && kind == 4) {
                            button {
                                onclick: move |_| {
                                    if !ending {
                                        picked.set(true);
                                    }
                                    #[cfg(not(target_arch = "wasm32"))]
                                    {
                                        if ending {
                                            session::hang_up();
                                        } else {
                                            session::place_call(&identity);
                                        }
                                    }
                                    #[cfg(target_arch = "wasm32")]
                                    {
                                        let _ = (&identity, ending);
                                    }
                                },
                                "{label}"
                            }
                        }
                        if ways.video {
                            button { onclick: move |_| picked.set(true), "video" }
                        }
                    }
                }
            }
        }
    }
}

const REVEAL: f64 = 72.0;

#[component]
fn NoteRow(kind: NoteKind, words: String, time: String, mine: bool, settled: bool) -> Element {
    let mut reveal = use_signal(|| 0.0);
    let mut drag = use_signal(|| None::<(f64, f64)>);
    let words_class = if kind == NoteKind::Text {
        "words hand"
    } else {
        "words"
    };
    let side = if mine { "mine" } else { "theirs" };
    let tone = if settled { "" } else { "waiting" };
    rsx! {
        li { class: "note {note_class(kind)} {side} {tone}",
            div {
                class: if drag().is_some() { "swipe live" } else { "swipe" },
                style: "--reveal: {reveal()}px",
                onpointerdown: move |event| {
                    drag.set(Some((event.client_coordinates().x, reveal())));
                },
                onpointermove: move |event| {
                    let Some((start_x, start_reveal)) = drag() else {
                        return;
                    };
                    let next = (start_reveal + start_x - event.client_coordinates().x)
                        .clamp(0.0, REVEAL);
                    reveal.set(next);
                },
                onpointerup: move |_| release_note(drag, reveal),
                onpointercancel: move |_| release_note(drag, reveal),
                onpointerleave: move |_| release_note(drag, reveal),
                p { class: "{words_class}", "{words}" }
                span { class: "when",
                    span { "{time}" }
                }
            }
        }
    }
}

fn release_note(mut drag: Signal<Option<(f64, f64)>>, mut reveal: Signal<f64>) {
    if drag().is_none() {
        return;
    }
    drag.set(None);
    let next = if reveal() > REVEAL / 2.0 { REVEAL } else { 0.0 };
    reveal.set(next);
}

fn note_class(kind: NoteKind) -> &'static str {
    match kind {
        NoteKind::Text => "",
        NoteKind::Voice => "voice",
        NoteKind::Video => "video",
    }
}
