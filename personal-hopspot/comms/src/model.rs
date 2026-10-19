//! The notebook: contacts the user has added, and the notes they have shared.
//!
//! A contact code is the person's name and their identity public key, one
//! field per line. Nothing here names the network underneath.

use qrcode::types::Color;
use qrcode::QrCode;

/// Browser stand-in for the identity public key. The desktop session replaces it with a real key.
pub const YOUR_PUBLIC_KEY: &str = "7f3a9c1e5b28d046a1c7e94f0b6d3a82c5e17890ab34df12657890abcdef01239c4e7a1b0d5f3826e4a9c0b7d1f6a3852e0c9b4d7f1a6e3085c2b9d4f7a1e063";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteKind {
    Text,
    Voice,
    Video,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Note {
    pub id: u64,
    pub kind: NoteKind,
    /// Weekday shown on the divider, not beside the words.
    pub day: String,
    /// Clock time, kept off the page until the note is slid aside.
    pub time: String,
    pub words: String,
    /// True when this note or call is the user's, so it sits on the right.
    pub mine: bool,
    /// A note we just sent stays grey until the other side acknowledges it.
    pub settled: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Person {
    pub id: u64,
    pub name: String,
    pub identity: String,
    pub notes: Vec<Note>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    People,
    Code,
    Add,
    Journal(u64),
}

pub fn contact_code(name: &str, identity: &str) -> String {
    format!("{name}\n{identity}")
}

#[cfg(any(test, target_arch = "wasm32"))]
pub fn sample_people() -> Vec<Person> {
    vec![
        Person {
            id: 1,
            name: "Mara".into(),
            identity: "19ab44c0e8d15f2076c3aa91b0d4e6f2".into(),
            notes: vec![
                kept(
                    NoteKind::Text,
                    "Monday",
                    "9:14",
                    "The pears are ready if you want some.",
                    false,
                ),
                kept(
                    NoteKind::Voice,
                    "Monday",
                    "9:18",
                    "Outbound voice call, 14 seconds",
                    true,
                ),
                kept(
                    NoteKind::Text,
                    "Wednesday",
                    "16:02",
                    "I'll leave a basket on the step.",
                    false,
                ),
                kept(NoteKind::Text, "Wednesday", "16:11", "I'll take two.", true),
            ],
        },
        Person {
            id: 2,
            name: "Sam".into(),
            identity: "77c0de12a9b4f6530188e2d4c6ab5190".into(),
            notes: vec![
                kept(
                    NoteKind::Video,
                    "Sunday",
                    "11:05",
                    "A video call, nine seconds.",
                    true,
                ),
                kept(
                    NoteKind::Text,
                    "Sunday",
                    "11:07",
                    "That light was only there for a minute.",
                    false,
                ),
            ],
        },
        Person {
            id: 3,
            name: "Jo".into(),
            identity: "05f8a1d6c3b94720e4d1aa78c2b6e093".into(),
            notes: vec![
                kept(
                    NoteKind::Text,
                    "Friday",
                    "18:40",
                    "See you at the gate.",
                    false,
                ),
                kept(NoteKind::Text, "Friday", "18:44", "I'll be there.", true),
            ],
        },
    ]
}

pub fn write_note(person: &mut Person, words: &str) -> Option<u64> {
    push_note(person, words, true, false)
}

/// The other side has acknowledged this note, so it can turn from grey to ink.
pub fn settle_note(people: &mut [Person], id: u64) -> bool {
    for person in people {
        if let Some(note) = person.notes.iter_mut().find(|note| note.id == id) {
            note.settled = true;
            return true;
        }
    }
    false
}

/// The voice button is shown for a real contact, and for the sample conversations
/// that already offer a call.
pub fn can_call(person: &Person) -> bool {
    offered_ways(person.id).voice || person.identity.trim().len() == 128
}

/// A voice call that just ended, written into the thread as a voice note.
///
/// `outgoing` sits on the right and reads "Outbound". A call that arrived sits
/// on the left and reads "Inbound".
pub fn remember_call(person: &mut Person, seconds: u64, outgoing: bool) {
    let (day, time) = local_stamp();
    let direction = if outgoing { "Outbound" } else { "Inbound" };
    let words = match seconds {
        0 => format!("{direction} voice call"),
        1 => format!("{direction} voice call, 1 second"),
        secs if secs < 60 => format!("{direction} voice call, {secs} seconds"),
        secs => {
            let minutes = secs / 60;
            let remain = secs % 60;
            format!("{direction} voice call, {minutes}:{remain:02} minutes")
        }
    };
    person.notes.push(Note {
        id: fresh_note_id(),
        kind: NoteKind::Voice,
        day,
        time,
        words,
        mine: outgoing,
        settled: true,
    });
}

/// A note that arrived from this contact, stamped with the time it was written.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn note_from_them(person: &mut Person, words: &str, written_at_unix: f64) {
    let words = words.trim();
    if words.is_empty() {
        return;
    }
    let (day, time) = stamp_at(written_at_unix);
    person.notes.push(Note {
        id: fresh_note_id(),
        kind: NoteKind::Text,
        day,
        time,
        words: words.to_string(),
        mine: false,
        settled: true,
    });
}

fn push_note(person: &mut Person, words: &str, mine: bool, settled: bool) -> Option<u64> {
    let words = words.trim();
    if words.is_empty() {
        return None;
    }
    let (day, time) = local_stamp();
    let id = fresh_note_id();
    person.notes.push(Note {
        id,
        kind: NoteKind::Text,
        day,
        time,
        words: words.to_string(),
        mine,
        settled: settled || cfg!(target_arch = "wasm32"),
    });
    Some(id)
}

fn fresh_note_id() -> u64 {
    use std::sync::atomic::Ordering;
    NEXT_NOTE.fetch_add(1, Ordering::Relaxed)
}

/// The next note id stays ahead of anything already stored, so a restarted
/// journal does not hand a new note an id that an old one still uses.
pub fn note_ids_from(people: &[Person]) {
    use std::sync::atomic::Ordering;
    let stored = people
        .iter()
        .flat_map(|person| person.notes.iter())
        .map(|note| note.id)
        .max()
        .unwrap_or(0);
    let _ = NEXT_NOTE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        (stored >= current).then_some(stored.saturating_add(1))
    });
}

use std::sync::atomic::AtomicU64;
static NEXT_NOTE: AtomicU64 = AtomicU64::new(1);

/// A note already in the thread. It is not waiting on an acknowledgement.
#[cfg(any(test, target_arch = "wasm32"))]
fn kept(kind: NoteKind, day: &str, time: &str, words: &str, mine: bool) -> Note {
    Note {
        id: fresh_note_id(),
        kind,
        day: day.to_string(),
        time: time.to_string(),
        words: words.to_string(),
        mine,
        settled: true,
    }
}

/// Consecutive notes that share a day, in the order they were written.
pub fn group_by_day(notes: &[Note]) -> Vec<(String, Vec<Note>)> {
    let mut groups: Vec<(String, Vec<Note>)> = Vec::new();
    for note in notes {
        match groups.last_mut() {
            Some((day, grouped)) if day == &note.day => grouped.push(note.clone()),
            _ => groups.push((note.day.clone(), vec![note.clone()])),
        }
    }
    groups
}

/// How a contact can be reached, after each live interface has been tried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reach {
    pub pending: bool,
    pub ways: OfferedWays,
    /// One row per interface while the mesh is being searched, and after.
    pub lines: Vec<ReachLine>,
    /// The interface the search kept, once it has finished.
    pub winner: Option<String>,
}

/// One interface on the search table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReachLine {
    pub name: String,
    pub status: String,
}

impl Reach {
    pub fn unknown() -> Self {
        Self {
            pending: true,
            ways: OfferedWays::none(),
            lines: Vec::new(),
            winner: None,
        }
    }
}

/// Where one interface is in the search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeshStep {
    Finding,
    NoPath,
    Testing,
    TimedOut,
    /// Round trip of the 400-byte sample, in milliseconds.
    Rate(u64),
}

/// The words shown for one interface row.
pub fn mesh_status(step: MeshStep) -> String {
    match step {
        MeshStep::Finding => "Finding path".to_string(),
        MeshStep::NoPath => "none found".to_string(),
        MeshStep::Testing => "Testing speed".to_string(),
        MeshStep::TimedOut => "timed out".to_string(),
        MeshStep::Rate(round_trip_ms) => format!("{}bps", bits_per_second(round_trip_ms)),
    }
}

/// Bits per second implied by carrying the 400-byte sample in `round_trip_ms`.
pub fn bits_per_second(round_trip_ms: u64) -> u64 {
    const SAMPLE_BYTES: u64 = 400;
    SAMPLE_BYTES.saturating_mul(8).saturating_mul(1_000) / round_trip_ms.max(1)
}

/// Ways the both-ways packet says this conversation can use.
/// A note needs the least, then a voice call, then a video call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OfferedWays {
    pub note: bool,
    pub voice: bool,
    pub video: bool,
}

impl OfferedWays {
    pub const fn none() -> Self {
        Self {
            note: false,
            voice: false,
            video: false,
        }
    }
}

/// A 400-byte sample back in 20 ms is 160,000 bps. A video session needs that much.
pub const VIDEO_ROUND_TRIP_MS: u64 = 20;

/// What a 400-byte acknowledgement is quick enough to carry.
/// A note is one message. Voice needs the answer back inside a fifth of a second.
/// Video needs 160 kbps.
pub fn ways_for_ack(round_trip_ms: Option<u64>) -> OfferedWays {
    let Some(round_trip_ms) = round_trip_ms else {
        return OfferedWays::none();
    };
    OfferedWays {
        note: true,
        voice: round_trip_ms <= 200,
        video: round_trip_ms <= VIDEO_ROUND_TRIP_MS,
    }
}

/// The sample came back quickly enough that this path can carry video.
pub fn enough_for_video(round_trip_ms: u64) -> bool {
    ways_for_ack(Some(round_trip_ms)).video
}

/// Stand-in for the packet exchanged when a person is opened.
/// A measured path will replace the id match.
pub fn offered_ways(person_id: u64) -> OfferedWays {
    match person_id {
        1 => OfferedWays {
            note: true,
            voice: true,
            video: true,
        },
        2 => OfferedWays {
            note: true,
            voice: true,
            video: false,
        },
        _ => OfferedWays {
            note: true,
            voice: false,
            video: false,
        },
    }
}

fn local_stamp() -> (String, String) {
    stamp_of(chrono::Local::now())
}

fn stamp_at(unix_secs: f64) -> (String, String) {
    let secs = unix_secs.floor() as i64;
    let Some(utc) = chrono::DateTime::from_timestamp(secs, 0) else {
        return local_stamp();
    };
    stamp_of(utc.with_timezone(&chrono::Local))
}

fn stamp_of(when: chrono::DateTime<chrono::Local>) -> (String, String) {
    use chrono::Timelike;
    let day = when.format("%A").to_string();
    let time = format!("{}:{:02}", when.hour(), when.minute());
    (day, time)
}

pub fn add_person(people: &mut Vec<Person>, next_id: &mut u64, name: &str, identity: &str) -> bool {
    let name = name.trim();
    let identity = identity.trim();
    if name.is_empty() || identity.is_empty() {
        return false;
    }
    let id = *next_id;
    *next_id += 1;
    people.push(Person {
        id,
        name: name.to_string(),
        identity: identity.to_string(),
        notes: Vec::new(),
    });
    true
}

/// The contact book, written as one line per field so a note can contain more than one line.
pub fn encode_contacts(people: &[Person]) -> String {
    let mut out = String::from("journal-contacts 1\n");
    out.push_str(&people.len().to_string());
    out.push('\n');
    for person in people {
        line(&mut out, &person.id.to_string());
        line(&mut out, &escape_field(&person.name));
        line(&mut out, &escape_field(&person.identity));
        line(&mut out, &person.notes.len().to_string());
        for note in &person.notes {
            line(&mut out, &note.id.to_string());
            line(&mut out, kind_name(note.kind));
            line(&mut out, &escape_field(&note.day));
            line(&mut out, &escape_field(&note.time));
            line(&mut out, if note.mine { "1" } else { "0" });
            line(&mut out, if note.settled { "1" } else { "0" });
            line(&mut out, &escape_field(&note.words));
        }
    }
    out
}

/// Reads a book written by `encode_contacts`. Anything else is left unread.
pub fn decode_contacts(text: &str) -> Option<Vec<Person>> {
    let mut lines = text.lines();
    if lines.next()? != "journal-contacts 1" {
        return None;
    }
    let count: usize = lines.next()?.parse().ok()?;
    let mut people = Vec::with_capacity(count);
    for _ in 0..count {
        let id = lines.next()?.parse().ok()?;
        let name = unescape_field(lines.next()?)?;
        let identity = unescape_field(lines.next()?)?;
        let notes_len: usize = lines.next()?.parse().ok()?;
        let mut notes = Vec::with_capacity(notes_len);
        for _ in 0..notes_len {
            notes.push(Note {
                id: lines.next()?.parse().ok()?,
                kind: parse_kind(lines.next()?)?,
                day: unescape_field(lines.next()?)?,
                time: unescape_field(lines.next()?)?,
                mine: parse_flag(lines.next()?)?,
                settled: parse_flag(lines.next()?)?,
                words: unescape_field(lines.next()?)?,
            });
        }
        people.push(Person {
            id,
            name,
            identity,
            notes,
        });
    }
    if lines.next().is_some() {
        return None;
    }
    Some(people)
}

pub fn next_person_id(people: &[Person]) -> u64 {
    people
        .iter()
        .map(|person| person.id)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1)
}

fn line(out: &mut String, field: &str) {
    out.push_str(field);
    out.push('\n');
}

fn kind_name(kind: NoteKind) -> &'static str {
    match kind {
        NoteKind::Text => "text",
        NoteKind::Voice => "voice",
        NoteKind::Video => "video",
    }
}

fn parse_kind(text: &str) -> Option<NoteKind> {
    match text {
        "text" => Some(NoteKind::Text),
        "voice" => Some(NoteKind::Voice),
        "video" => Some(NoteKind::Video),
        _ => None,
    }
}

fn parse_flag(text: &str) -> Option<bool> {
    match text {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

fn escape_field(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

fn unescape_field(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            _ => return None,
        }
    }
    Some(out)
}

/// Dark modules of a square mark for `contact_code`.
pub fn code_mark(payload: &str) -> Option<(usize, Vec<(usize, usize)>)> {
    let code = QrCode::new(payload.as_bytes()).ok()?;
    let width = code.width();
    let mut cells = Vec::new();
    for y in 0..width {
        for x in 0..width {
            if code[(x, y)] == Color::Dark {
                cells.push((x, y));
            }
        }
    }
    Some((width, cells))
}

#[cfg(test)]
mod tests {
    use super::{
        add_person, bits_per_second, can_call, code_mark, contact_code, decode_contacts,
        encode_contacts, enough_for_video, group_by_day, mesh_status, next_person_id,
        note_ids_from, offered_ways, remember_call, sample_people, settle_note, ways_for_ack,
        write_note, MeshStep, Note, NoteKind, OfferedWays, Person, VIDEO_ROUND_TRIP_MS,
    };

    #[test]
    fn a_contact_code_is_the_name_and_the_identity() {
        assert_eq!(
            contact_code("Mara", "19ab44c0e8d15f2076c3aa91b0d4e6f2"),
            "Mara\n19ab44c0e8d15f2076c3aa91b0d4e6f2"
        );
    }

    #[test]
    fn writing_a_note_keeps_the_words_on_that_person() {
        let mut people = sample_people();
        let before = people[0].notes.len();
        let id = write_note(&mut people[0], "  I saved you two.  ").expect("a note");
        assert_eq!(people[0].notes.len(), before + 1);
        let note = people[0].notes.last().expect("the new note");
        assert_eq!(note.kind, NoteKind::Text);
        assert_eq!(note.words, "I saved you two.");
        assert!(!note.day.is_empty());
        assert!(note.time.contains(':'));
        assert!(note.mine);
        assert!(!note.settled);
        assert!(people[0]
            .notes
            .iter()
            .rev()
            .skip(1)
            .all(|note| note.settled));
        assert!(settle_note(&mut people, id));
        assert!(people[0].notes.last().expect("acknowledged").settled);
        write_note(&mut people[0], "   ");
        assert_eq!(people[0].notes.len(), before + 1);
    }

    #[test]
    fn notes_from_the_same_day_share_one_divider() {
        let people = sample_people();
        let groups = group_by_day(&people[0].notes);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "Monday");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "Wednesday");
        assert_eq!(groups[1].1.len(), 2);
        assert!(!groups[0].1[0].mine);
        assert!(groups[0].1[1].mine);
    }

    #[test]
    fn a_real_contact_can_be_called() {
        let samples = sample_people();
        assert!(can_call(&samples[0]));
        assert!(can_call(&samples[1]));
        assert!(!can_call(&samples[2]));
        let real = Person {
            id: 9,
            name: "Ned".into(),
            identity: "ab".repeat(64),
            notes: Vec::new(),
        };
        assert!(can_call(&real));
    }

    #[test]
    fn a_call_is_remembered_as_a_voice_note() {
        let mut person = sample_people().pop().expect("a person");
        let before = person.notes.len();
        remember_call(&mut person, 14, true);
        let note = person.notes.last().expect("the call");
        assert_eq!(note.kind, NoteKind::Voice);
        assert_eq!(note.words, "Outbound voice call, 14 seconds");
        assert!(note.mine);
        remember_call(&mut person, 3, false);
        let inbound = person.notes.last().expect("an inbound call");
        assert_eq!(inbound.words, "Inbound voice call, 3 seconds");
        assert!(!inbound.mine);
        remember_call(&mut person, 1, false);
        let one = person.notes.last().expect("one second");
        assert_eq!(one.words, "Inbound voice call, 1 second");
        remember_call(&mut person, 151, true);
        assert_eq!(
            person.notes.last().expect("a longer call").words,
            "Outbound voice call, 2:31 minutes"
        );
        remember_call(&mut person, 0, true);
        assert_eq!(
            person.notes.last().expect("a short call").words,
            "Outbound voice call"
        );
        assert_eq!(person.notes.len(), before + 5);
    }

    #[test]
    fn adding_a_person_needs_both_a_name_and_a_code() {
        let mut people = Vec::new();
        let mut next_id = 4;
        assert!(!add_person(&mut people, &mut next_id, "  ", "abc"));
        assert!(!add_person(&mut people, &mut next_id, "Ned", " "));
        assert!(add_person(&mut people, &mut next_id, " Ned ", "abc"));
        assert_eq!(people[0].name, "Ned");
        assert_eq!(people[0].identity, "abc");
        assert!(people[0].notes.is_empty());
        assert_eq!(next_id, 5);
    }

    #[test]
    fn a_search_row_names_each_step() {
        assert_eq!(mesh_status(MeshStep::Finding), "Finding path");
        assert_eq!(mesh_status(MeshStep::NoPath), "none found");
        assert_eq!(mesh_status(MeshStep::Testing), "Testing speed");
        assert_eq!(mesh_status(MeshStep::TimedOut), "timed out");
        assert_eq!(mesh_status(MeshStep::Rate(200)), "16000bps");
        assert_eq!(bits_per_second(0), bits_per_second(1));
        assert_eq!(bits_per_second(VIDEO_ROUND_TRIP_MS), 160_000);
        assert!(enough_for_video(VIDEO_ROUND_TRIP_MS));
        assert!(!enough_for_video(VIDEO_ROUND_TRIP_MS + 1));
    }

    #[test]
    fn a_faster_answer_offers_more_ways_to_talk() {
        assert_eq!(ways_for_ack(None), OfferedWays::none());
        assert_eq!(
            ways_for_ack(Some(800)),
            OfferedWays {
                note: true,
                voice: false,
                video: false,
            }
        );
        assert_eq!(
            ways_for_ack(Some(120)),
            OfferedWays {
                note: true,
                voice: true,
                video: false,
            }
        );
        assert_eq!(
            ways_for_ack(Some(30)),
            OfferedWays {
                note: true,
                voice: true,
                video: false,
            }
        );
        assert_eq!(
            ways_for_ack(Some(VIDEO_ROUND_TRIP_MS)),
            OfferedWays {
                note: true,
                voice: true,
                video: true,
            }
        );
    }

    #[test]
    fn opening_a_person_offers_only_the_ways_the_packet_allows() {
        let mara = offered_ways(1);
        assert!(mara.note && mara.voice && mara.video);
        let sam = offered_ways(2);
        assert!(sam.note && sam.voice && !sam.video);
        let jo = offered_ways(3);
        assert!(jo.note && !jo.voice && !jo.video);
        let someone_new = offered_ways(4);
        assert!(someone_new.note && !someone_new.voice && !someone_new.video);
    }

    #[test]
    fn your_public_key_is_the_full_identity_key() {
        assert_eq!(super::YOUR_PUBLIC_KEY.len(), 128);
        assert!(super::YOUR_PUBLIC_KEY
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn your_qr_code_draws_a_square_mark() {
        let (width, cells) = code_mark(&contact_code("Alex", super::YOUR_PUBLIC_KEY))
            .expect("a contact code fits in a mark");
        assert!(width >= 21);
        assert!(!cells.is_empty());
        assert!(cells.iter().all(|(x, y)| *x < width && *y < width));
    }

    #[test]
    fn a_contact_book_round_trips_through_its_file() {
        let people = vec![Person {
            id: 7,
            name: "Alex \\ the gate".into(),
            identity: "ab".repeat(64),
            notes: vec![Note {
                id: 40,
                kind: NoteKind::Text,
                day: "Monday".into(),
                time: "9:14".into(),
                words: "Line one.\nLine \\ two.".into(),
                mine: true,
                settled: false,
            }],
        }];
        let stored = encode_contacts(&people);
        let read = decode_contacts(&stored).expect("the book");
        assert_eq!(encode_contacts(&read), stored);
        assert_eq!(read[0].notes[0].words, "Line one.\nLine \\ two.");
        assert!(!read[0].notes[0].settled);
        assert!(decode_contacts("not a book").is_none());
        assert_eq!(next_person_id(&read), 8);
        note_ids_from(&read);
        let mut again = read;
        let id = write_note(&mut again[0], "still here").expect("a note");
        assert!(id > 40);
    }
}
