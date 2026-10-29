//! The notebook: contacts the user has added, and the notes they have shared.
//!
//! A contact code is the person's name and their `lxmf.delivery` address, one
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

/// A place carried with a note. Degrees are kept at seven decimal places.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub latitude_e7: i32,
    pub longitude_e7: i32,
}

/// Shown in the note while it is being written. If these words are still there
/// when the note is sent, the place is sent with them.
pub const PLACEHOLDER: &str = "📍 Location";

impl Place {
    pub fn from_degrees(latitude: f64, longitude: f64) -> Option<Self> {
        if !latitude.is_finite()
            || !longitude.is_finite()
            || !(-90.0..=90.0).contains(&latitude)
            || !(-180.0..=180.0).contains(&longitude)
        {
            return None;
        }
        Some(Self {
            latitude_e7: (latitude * 10_000_000.0).round() as i32,
            longitude_e7: (longitude * 10_000_000.0).round() as i32,
        })
    }

    pub fn parse(text: &str) -> Option<Self> {
        let (latitude, longitude) = text.trim().split_once(',')?;
        Self::from_degrees(
            latitude.trim().parse().ok()?,
            longitude.trim().parse().ok()?,
        )
    }

    pub fn latitude(self) -> f64 {
        f64::from(self.latitude_e7) / 10_000_000.0
    }

    pub fn longitude(self) -> f64 {
        f64::from(self.longitude_e7) / 10_000_000.0
    }

    pub fn text(self) -> String {
        format!("{:.7},{:.7}", self.latitude(), self.longitude())
    }
}

/// The words of a note, with the place marker taken out once the map is shown.
pub fn shown_words(words: &str, place: Option<Place>) -> String {
    if place.is_none() {
        return words.to_string();
    }
    let mut out = words.replace(PLACEHOLDER, "");
    while out.contains("  ") {
        out = out.replace("  ", " ");
    }
    out.lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// One OpenStreetMap tile positioned inside the place window.
#[derive(Clone, Debug, PartialEq)]
pub struct MapTile {
    pub x: u32,
    pub y: u32,
    pub left: f64,
    pub top: f64,
    pub edge: f64,
}

/// A square of about 500 m², with the place in the middle.
#[derive(Clone, Debug, PartialEq)]
pub struct MapWindow {
    pub zoom: u32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub tiles: Vec<MapTile>,
}

const VIEW_SIDE_METERS: f64 = 22.360679774997898;
const MERCATOR_LIMIT: f64 = 1.4844222297453322;

pub fn map_window(place: Place) -> MapWindow {
    let latitude = place.latitude();
    let longitude = place.longitude();
    let zoom = (1..=19)
        .rev()
        .find(|&zoom| pixels_for_view(latitude, zoom) <= 512.0)
        .unwrap_or(19);
    let geo = pixels_for_view(latitude, zoom);
    let width = geo.max(168.0).round().max(1.0);
    let scale = width / geo;
    let (world_x, world_y) = world_pixel(latitude, longitude, zoom);
    let left = world_x - geo / 2.0;
    let top = world_y - geo / 2.0;
    let n = 1_i64 << zoom;
    let x0 = (left / 256.0).floor() as i64;
    let x1 = ((left + geo) / 256.0).floor() as i64;
    let y0 = (top / 256.0).floor() as i64;
    let y1 = ((top + geo) / 256.0).floor() as i64;
    let mut tiles = Vec::new();
    for ty in y0..=y1 {
        if ty < 0 || ty >= n {
            continue;
        }
        for tx in x0..=x1 {
            let wrapped = tx.rem_euclid(n);
            tiles.push(MapTile {
                x: wrapped as u32,
                y: ty as u32,
                left: (tx as f64 * 256.0 - left) * scale,
                top: (ty as f64 * 256.0 - top) * scale,
                edge: 256.0 * scale,
            });
        }
    }
    MapWindow {
        zoom,
        width: width as u32,
        height: width as u32,
        scale,
        tiles,
    }
}

fn pixels_for_view(latitude: f64, zoom: u32) -> f64 {
    VIEW_SIDE_METERS / meters_per_pixel(latitude, zoom)
}

fn meters_per_pixel(latitude: f64, zoom: u32) -> f64 {
    let cosine = latitude.to_radians().cos().abs().max(0.01);
    156_543.03392804097 * cosine / f64::from(1_u32 << zoom)
}

fn world_pixel(latitude: f64, longitude: f64, zoom: u32) -> (f64, f64) {
    let span = f64::from(1_u32 << zoom) * 256.0;
    let x = (longitude + 180.0) / 360.0 * span;
    let latitude = latitude.to_radians().clamp(-MERCATOR_LIMIT, MERCATOR_LIMIT);
    let y =
        (1.0 - (latitude.tan() + 1.0 / latitude.cos()).ln() / std::f64::consts::PI) / 2.0 * span;
    (x, y)
}

/// A mark on a note. The same set is offered on every note from a contact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reaction {
    Love,
    Yes,
    Down,
    Ha,
    Oh,
    Ask,
}

impl Reaction {
    pub fn name(self) -> &'static str {
        match self {
            Self::Love => "love",
            Self::Yes => "yes",
            Self::Down => "down",
            Self::Ha => "ha",
            Self::Oh => "oh",
            Self::Ask => "ask",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "love" => Self::Love,
            "yes" => Self::Yes,
            "down" => Self::Down,
            "ha" => Self::Ha,
            "oh" => Self::Oh,
            "ask" => Self::Ask,
            _ => return None,
        })
    }
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
    /// Set after this note has been tried three times and is no longer sending itself.
    pub failed: bool,
    /// The mailbox accepted this note. It stays grey until the other person acknowledges it.
    pub posted: bool,
    /// sha256 of the packed note, so an acknowledgement can find it after a restart.
    pub digest: Option<[u8; 32]>,
    /// Set when the note still carried [`PLACEHOLDER`] at send time.
    pub place: Option<Place>,
    /// When the note was written, as the bits of a Unix-seconds float.
    /// Zero means the note was saved before reactions, so a mark stays on this device.
    pub written_at: u64,
    pub reaction: Option<Reaction>,
    /// The file sent with this note. The bytes live beside the book, under `files/`.
    pub file_name: Option<String>,
    /// A file they sent has been copied into Downloads.
    pub downloaded: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Person {
    pub id: u64,
    pub name: String,
    pub identity: String,
    pub notes: Vec<Note>,
    /// The newest note id this journal has been opened through.
    /// A later note from them is still unread.
    pub seen: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    People,
    Code,
    Add,
    Journal(u64),
    Share(u64),
}

pub fn contact_code(name: &str, identity: &str) -> String {
    format!("{name}\n{identity}")
}

#[cfg(any(test, target_arch = "wasm32"))]
pub fn sample_people() -> Vec<Person> {
    let mut people = vec![
        Person {
            id: 1,
            name: "Mara".into(),
            identity: "19ab44c0e8d15f2076c3aa91b0d4e6f2".into(),
            seen: 0,
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
            seen: 0,
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
            seen: 0,
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
    ];
    for person in &mut people {
        mark_read(person);
    }
    people
}

pub fn write_note(person: &mut Person, words: &str) -> Option<u64> {
    push_note(person, words, None, true, false, None)
}

/// A note that also carries a place, when the placeholder is still in the words.
pub fn write_placed_note(person: &mut Person, words: &str, place: Option<Place>) -> Option<u64> {
    push_note(person, words, place, true, false, None)
}

/// A note that also carries a file. The words may be empty. The place is kept when the placeholder is still in the words.
pub fn write_file_note(
    person: &mut Person,
    words: &str,
    place: Option<Place>,
    file_name: &str,
) -> Option<u64> {
    push_note(person, words, place, true, false, Some(file_name))
}

/// The other side has acknowledged this note, so it can turn from grey to ink.
pub fn settle_note(people: &mut [Person], id: u64) -> bool {
    for person in people {
        if let Some(note) = person.notes.iter_mut().find(|note| note.id == id) {
            note.settled = true;
            note.failed = false;
            note.posted = false;
            return true;
        }
    }
    false
}

/// This note was tried three times and will not send itself again.
pub fn fail_note(people: &mut [Person], id: u64) -> bool {
    for person in people {
        if let Some(note) = person.notes.iter_mut().find(|note| note.id == id) {
            if note.settled || note.failed || note.posted || !note.mine {
                return false;
            }
            note.failed = true;
            return true;
        }
    }
    false
}

/// A file they named arrived, and the bytes did not. The note stays, marked failed.
pub fn fail_their_file(person: &mut Person, written_at: f64, file_name: &str) -> bool {
    let file_name = file_name_of(file_name);
    let written_at = written_at.to_bits();
    let Some(note) = person.notes.iter_mut().rev().find(|note| {
        !note.mine
            && note.kind == NoteKind::Text
            && note.written_at == written_at
            && note.file_name.as_deref() == Some(file_name.as_str())
    }) else {
        return false;
    };
    if note.failed || note.downloaded {
        return false;
    }
    note.failed = true;
    true
}

/// Their file was copied into Downloads. It is no longer a failed transfer.
pub fn land_their_file(person: &mut Person, id: u64) -> bool {
    let Some(note) = person.notes.iter_mut().find(|note| note.id == id) else {
        return false;
    };
    if note.mine || note.file_name.as_ref().is_none_or(|name| name.is_empty()) {
        return false;
    }
    let changed = !note.downloaded || note.failed;
    note.failed = false;
    note.downloaded = true;
    changed
}

/// The mailbox accepted this note. It is not delivered yet.
pub fn post_note(people: &mut [Person], id: u64, digest: [u8; 32]) -> bool {
    for person in people {
        if let Some(note) = person.notes.iter_mut().find(|note| note.id == id) {
            if note.settled || !note.mine {
                return false;
            }
            note.posted = true;
            note.failed = false;
            note.digest = Some(digest);
            return true;
        }
    }
    false
}

/// The user asked to send a failed note again. The words go back out for three more tries.
pub fn retry_note(
    person: &mut Person,
    id: u64,
) -> Option<(String, f64, Option<Place>, Option<String>)> {
    let note = person.notes.iter_mut().find(|note| note.id == id)?;
    if !note.mine || !note.failed || note.kind != NoteKind::Text {
        return None;
    }
    note.failed = false;
    Some((
        note.words.clone(),
        f64::from_bits(note.written_at),
        note.place,
        note.file_name.clone(),
    ))
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
        failed: false,
        posted: false,
        digest: None,
        place: None,
        written_at: 0,
        reaction: None,
        file_name: None,
        downloaded: false,
    });
}

/// A note that arrived from this contact, stamped with the time it was written.
/// A second copy of a note already in this journal stays the one entry.
/// Returns whether this copy was written into the journal.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
#[must_use]
pub fn note_from_them(
    person: &mut Person,
    words: &str,
    written_at_unix: f64,
    place: Option<Place>,
    file_name: Option<&str>,
) -> bool {
    let words = words.trim();
    let file_name = file_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(file_name_of);
    if words.is_empty() && place.is_none() && file_name.is_none() {
        return false;
    }
    let written_at = written_at_unix.to_bits();
    let already = person.notes.iter().any(|note| {
        note.kind == NoteKind::Text
            && !note.mine
            && note.written_at == written_at
            && note.words.trim() == words
            && note.place == place
            && note.file_name == file_name
    });
    if already {
        return false;
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
        failed: false,
        posted: false,
        digest: None,
        place,
        written_at,
        reaction: None,
        file_name,
        downloaded: false,
    });
    true
}

fn push_note(
    person: &mut Person,
    words: &str,
    place: Option<Place>,
    mine: bool,
    settled: bool,
    file_name: Option<&str>,
) -> Option<u64> {
    let words = words.trim();
    let place = place.filter(|_| words.contains(PLACEHOLDER));
    let file_name = file_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(file_name_of);
    if words.is_empty() && file_name.is_none() {
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
        failed: false,
        posted: false,
        digest: None,
        place,
        written_at: unix_now().to_bits(),
        reaction: None,
        file_name,
        downloaded: false,
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
        failed: false,
        posted: false,
        digest: None,
        place: None,
        written_at: 0,
        reaction: None,
        file_name: None,
        downloaded: false,
    }
}

/// Marks a note from this contact. The same mark again clears it.
/// The returned bits are the time the note was written, or zero when that time was not kept.
pub fn react_to_note(person: &mut Person, note_id: u64, choice: Reaction) -> Option<u64> {
    let note = person.notes.iter_mut().find(|note| note.id == note_id)?;
    if note.mine || note.kind != NoteKind::Text {
        return None;
    }
    if note.reaction == Some(choice) {
        note.reaction = None;
    } else {
        note.reaction = Some(choice);
    }
    Some(note.written_at)
}

/// A reaction that arrived for a note. The time it was written wins. Notes saved
/// before that time was kept are matched by their words, on the note this person wrote.
pub fn apply_reaction(
    person: &mut Person,
    written_at: f64,
    words: &str,
    reaction: Option<Reaction>,
) -> bool {
    let bits = written_at.to_bits();
    let words = words.trim();
    let index = if bits != 0 {
        person.notes.iter().position(|note| note.written_at == bits)
    } else {
        None
    };
    let index = index.or_else(|| note_with_words(person, words));
    let Some(index) = index else {
        return false;
    };
    if person.notes[index].reaction == reaction {
        return false;
    }
    person.notes[index].reaction = reaction;
    true
}

fn note_with_words(person: &Person, words: &str) -> Option<usize> {
    if words.is_empty() {
        return None;
    }
    let same = |note: &Note| note.kind == NoteKind::Text && note.words.trim() == words;
    person
        .notes
        .iter()
        .rposition(|note| same(note) && note.mine)
        .or_else(|| person.notes.iter().rposition(same))
}

/// Seconds since 1970-01-01 00:00:00 UTC. This is the instant that is sent.
fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

/// Consecutive notes that share a day, in the order they were written.
///
/// A note that carries a UTC instant is shown in the device's local time.
/// The stored day and clock are only the fallback for a note that has no instant.
pub fn group_by_day(notes: &[Note]) -> Vec<(String, Vec<Note>)> {
    let mut groups: Vec<(String, Vec<Note>)> = Vec::new();
    for note in notes {
        let mut note = note.clone();
        let (day, time) = shown_stamp(&note);
        note.day = day.clone();
        note.time = time;
        match groups.last_mut() {
            Some((heading, grouped)) if heading == &day => grouped.push(note),
            _ => groups.push((day, vec![note])),
        }
    }
    groups
}

fn shown_stamp(note: &Note) -> (String, String) {
    if note.written_at == 0 {
        return (note.day.clone(), note.time.clone());
    }
    stamp_at(f64::from_bits(note.written_at))
}

/// The interface the stack chose for a contact, and what a sample on that path can carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reach {
    pub pending: bool,
    pub ways: OfferedWays,
    /// The interface the stack's path uses, once one is known.
    pub winner: Option<String>,
    /// How many hops that path takes, once one is known.
    pub hops: Option<u8>,
    /// The most bytes a file on this path may be. None means the path has no ceiling.
    pub file_limit: Option<u64>,
}

impl Reach {
    pub fn unknown() -> Self {
        Self {
            pending: true,
            ways: OfferedWays::none(),
            winner: None,
            hops: None,
            file_limit: Some(SMALLEST_ATTACHMENT),
        }
    }
}

/// The label in the note menu. An open path says "file". A capped path names the ceiling.
pub fn file_choice(limit: Option<u64>) -> String {
    match limit {
        None => "file".to_string(),
        Some(bytes) => format!("file <{}", size_word(bytes)),
    }
}

/// A short size for the menu and for the notice when a file is too big.
pub fn size_word(bytes: u64) -> String {
    if bytes >= 1_000_000 && bytes % 1_000_000 == 0 {
        return format!("{}M", bytes / 1_000_000);
    }
    if bytes >= 1024 && bytes % 1024 == 0 {
        return format!("{}k", bytes / 1024);
    }
    bytes.to_string()
}

/// What the composer says when the chosen file is past the path's ceiling.
pub fn file_too_big(limit: Option<u64>) -> String {
    match limit {
        Some(bytes) => format!("That file is over {}.", size_word(bytes)),
        None => "That file could not be read.".to_string(),
    }
}

/// `window.xml` with `2` becomes `window (2).xml`. The first copy keeps the original name.
/// Android names the public copy in the activity, so this helper is the desktop path.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub fn numbered_file_name(name: &str, n: u32) -> String {
    if n <= 1 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            format!("{stem} ({n}).{ext}")
        }
        _ => format!("{name} ({n})"),
    }
}

/// The last path piece, with control characters taken out, short enough to sit on a note.
pub fn file_name_of(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();
    let name: String = base
        .chars()
        .filter(|ch| !ch.is_control())
        .take(120)
        .collect();
    let name = name.trim().trim_matches('.').trim();
    if name.is_empty() {
        "file".to_string()
    } else {
        name.to_string()
    }
}

/// The words under a contact's name. A known path names its interface. One hop, with nobody in between, is direct. A longer path says how many hops it takes. A search still running says so. A finished search with no path is isolated.
pub fn path_label(real: bool, pending: bool, winner: Option<&str>, hops: Option<u8>) -> String {
    if let Some(winner) = winner.map(str::trim).filter(|winner| !winner.is_empty()) {
        let Some(hops) = hops else {
            return winner.to_string();
        };
        if hops <= 1 {
            return format!("{winner} : direct");
        }
        return format!("{winner} : {hops} hops");
    }
    if !real {
        return String::new();
    }
    // Indexed, not a plain else. A release build was dropping the else literal.
    let labels = [
        std::hint::black_box("searching the mesh"),
        std::hint::black_box("isolated"),
    ];
    labels[usize::from(!pending)].to_string()
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

/// The sample a path measurement times, in bytes.
pub const PROBE_BYTES: u64 = 400;

/// At this rate, and above it, a resource has no size limit.
pub const OPEN_PATH_BPS: u64 = 150_000;

/// The most a LoRa path, or any path this slow, will carry beyond a single packet.
pub const SMALLEST_ATTACHMENT: u64 = 2_048;

/// How much of a note that does not fit in one packet a path will carry.
/// `Open` is no limit. A LoRa hop stays at [`SMALLEST_ATTACHMENT`] however fast the sample looked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceCarry {
    Open,
    Limited(u64),
}

impl ResourceCarry {
    pub const fn allows(self, bytes: u64) -> bool {
        match self {
            Self::Open => true,
            Self::Limited(limit) => bytes <= limit,
        }
    }
}

/// Bits per second implied by a 400-byte sample. A zero time is not a measurement.
pub fn bits_per_second(round_trip_ms: u64) -> Option<u64> {
    if round_trip_ms == 0 {
        return None;
    }
    Some(PROBE_BYTES.saturating_mul(8).saturating_mul(1_000) / round_trip_ms)
}

/// The resource budget for a measured rate. No measurement, and anything under 8 kb/s, is the smallest attachment.
pub fn resource_carry(bits_per_second: Option<u64>, lora: bool) -> ResourceCarry {
    if lora {
        return ResourceCarry::Limited(SMALLEST_ATTACHMENT);
    }
    let Some(bits_per_second) = bits_per_second else {
        return ResourceCarry::Limited(SMALLEST_ATTACHMENT);
    };
    if bits_per_second >= OPEN_PATH_BPS {
        return ResourceCarry::Open;
    }
    if bits_per_second >= 64_000 {
        return ResourceCarry::Limited(1_000_000);
    }
    if bits_per_second >= 32_000 {
        return ResourceCarry::Limited(256 * 1024);
    }
    if bits_per_second >= 16_000 {
        return ResourceCarry::Limited(64 * 1024);
    }
    if bits_per_second >= 8_000 {
        return ResourceCarry::Limited(16 * 1024);
    }
    ResourceCarry::Limited(SMALLEST_ATTACHMENT)
}

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

/// The weekday and clock for a UTC instant, shifted by the device's current offset.
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
        seen: 0,
    });
    true
}

/// Drops the contact and every note shared with them.
pub fn remove_person(people: &mut Vec<Person>, id: u64) -> bool {
    let before = people.len();
    people.retain(|person| person.id != id);
    people.len() != before
}

/// The name shown in the list. An empty name is refused, and the notes stay.
pub fn rename_person(people: &mut [Person], id: u64, name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    let Some(person) = people.iter_mut().find(|person| person.id == id) else {
        return false;
    };
    if person.name == name {
        return false;
    }
    person.name = name.to_string();
    true
}

/// A note from them that arrived after this journal was last opened.
pub fn has_unread(person: &Person) -> bool {
    person
        .notes
        .iter()
        .any(|note| !note.mine && note.id > person.seen)
}

/// The journal is open, so every note already in it has been seen.
pub fn mark_read(person: &mut Person) -> bool {
    let Some(latest) = person.notes.iter().map(|note| note.id).max() else {
        return false;
    };
    if latest <= person.seen {
        return false;
    }
    person.seen = latest;
    true
}

/// The contact book, written as one line per field so a note can contain more than one line.
/// Version 2 adds a place line after each note. Version 3 adds the time it was written
/// and a reaction. Version 4 adds how far the journal has been read. Version 5 adds
/// whether a note we sent has given up. Version 6 adds whether the mailbox has it,
/// and the hash an acknowledgement will name. Version 7 adds the file's name.
/// Version 8 adds whether a file they sent landed in Downloads.
/// The bytes stay in a file beside the book. An older book still opens.
pub fn encode_contacts(people: &[Person]) -> String {
    let mut out = String::from("journal-contacts 8\n");
    out.push_str(&people.len().to_string());
    out.push('\n');
    for person in people {
        line(&mut out, &person.id.to_string());
        line(&mut out, &escape_field(&person.name));
        line(&mut out, &escape_field(&person.identity));
        line(&mut out, &person.seen.to_string());
        line(&mut out, &person.notes.len().to_string());
        for note in &person.notes {
            line(&mut out, &note.id.to_string());
            line(&mut out, kind_name(note.kind));
            line(&mut out, &escape_field(&note.day));
            line(&mut out, &escape_field(&note.time));
            line(&mut out, if note.mine { "1" } else { "0" });
            line(&mut out, if note.settled { "1" } else { "0" });
            line(&mut out, &escape_field(&note.words));
            line(
                &mut out,
                &match note.place {
                    Some(place) => escape_field(&place.text()),
                    None => "-".to_string(),
                },
            );
            line(
                &mut out,
                &if note.written_at == 0 {
                    "-".to_string()
                } else {
                    format!("{:016x}", note.written_at)
                },
            );
            line(&mut out, note.reaction.map(Reaction::name).unwrap_or("-"));
            line(&mut out, if note.failed { "1" } else { "0" });
            line(&mut out, if note.posted { "1" } else { "0" });
            line(&mut out, &digest_field(note.digest));
            line(
                &mut out,
                &match &note.file_name {
                    Some(name) if !name.is_empty() => escape_field(name),
                    _ => "-".to_string(),
                },
            );
            line(&mut out, if note.downloaded { "1" } else { "0" });
        }
    }
    out
}

/// Reads a book written by `encode_contacts`. Anything else is left unread.
pub fn decode_contacts(text: &str) -> Option<Vec<Person>> {
    let mut lines = text.lines();
    let version = match lines.next()? {
        "journal-contacts 1" => 1,
        "journal-contacts 2" => 2,
        "journal-contacts 3" => 3,
        "journal-contacts 4" => 4,
        "journal-contacts 5" => 5,
        "journal-contacts 6" => 6,
        "journal-contacts 7" => 7,
        "journal-contacts 8" => 8,
        _ => return None,
    };
    let count: usize = lines.next()?.parse().ok()?;
    let mut people = Vec::with_capacity(count);
    for _ in 0..count {
        let id = lines.next()?.parse().ok()?;
        let name = unescape_field(lines.next()?)?;
        let identity = unescape_field(lines.next()?)?;
        let seen = if version >= 4 {
            Some(lines.next()?.parse().ok()?)
        } else {
            None
        };
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
                failed: false,
                posted: false,
                digest: None,
                downloaded: false,
                words: unescape_field(lines.next()?)?,
                place: if version >= 2 {
                    read_place(lines.next()?)?
                } else {
                    None
                },
                written_at: if version >= 3 {
                    read_stamp(lines.next()?)?
                } else {
                    0
                },
                reaction: if version >= 3 {
                    match lines.next()? {
                        "-" => None,
                        name => Reaction::parse(name),
                    }
                } else {
                    None
                },
                file_name: None,
            });
            if version >= 5 {
                notes.last_mut().expect("the note").failed = parse_flag(lines.next()?)?;
            }
            if version >= 6 {
                let note = notes.last_mut().expect("the note");
                note.posted = parse_flag(lines.next()?)?;
                note.digest = read_digest(lines.next()?)?;
            }
            if version >= 7 {
                let note = notes.last_mut().expect("the note");
                note.file_name = read_file_name(lines.next()?)?;
            }
            if version >= 8 {
                notes.last_mut().expect("the note").downloaded = parse_flag(lines.next()?)?;
            }
        }
        let seen = seen.unwrap_or_else(|| notes.iter().map(|note| note.id).max().unwrap_or(0));
        people.push(Person {
            id,
            name,
            identity,
            notes,
            seen,
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

fn read_file_name(text: &str) -> Option<Option<String>> {
    if text == "-" {
        Some(None)
    } else {
        Some(Some(unescape_field(text)?))
    }
}

fn read_place(text: &str) -> Option<Option<Place>> {
    let text = unescape_field(text)?;
    if text == "-" {
        Some(None)
    } else {
        Some(Place::parse(&text))
    }
}

fn digest_field(digest: Option<[u8; 32]>) -> String {
    let Some(digest) = digest else {
        return "-".to_string();
    };
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_digest(text: &str) -> Option<Option<[u8; 32]>> {
    if text == "-" {
        return Some(None);
    }
    if text.len() != 64 {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(Some(digest))
}

fn read_stamp(text: &str) -> Option<u64> {
    if text == "-" {
        return Some(0);
    }
    u64::from_str_radix(text, 16).ok()
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
        add_person, apply_reaction, bits_per_second, can_call, code_mark, contact_code,
        decode_contacts, encode_contacts, fail_note, fail_their_file, file_choice, file_name_of,
        group_by_day, has_unread, land_their_file, mark_read, next_person_id, note_from_them,
        note_ids_from, numbered_file_name, offered_ways, path_label, react_to_note, remember_call,
        remove_person, rename_person, resource_carry, retry_note, sample_people, settle_note,
        ways_for_ack, write_file_note, write_note, Note, NoteKind, OfferedWays, Person, Place,
        Reaction, ResourceCarry, OPEN_PATH_BPS, PLACEHOLDER, SMALLEST_ATTACHMENT,
        VIDEO_ROUND_TRIP_MS,
    };

    #[test]
    fn a_contact_with_no_path_is_isolated() {
        assert_eq!(path_label(true, true, None, None), "searching the mesh");
        assert_eq!(path_label(true, false, None, None), "isolated");
        assert_eq!(path_label(true, false, Some("  "), None), "isolated");
        assert_eq!(path_label(true, false, Some("USB"), None), "USB");
        assert_eq!(
            path_label(true, false, Some("Bluetooth"), Some(1)),
            "Bluetooth : direct"
        );
        assert_eq!(
            path_label(true, false, Some("Bluetooth"), Some(2)),
            "Bluetooth : 2 hops"
        );
        assert_eq!(path_label(false, false, None, None), "");
    }

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
    fn a_sent_instant_is_shown_in_local_time() {
        use chrono::Timelike;
        let unix = 1_700_000_000.0f64;
        let utc = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("instant");
        let local = utc.with_timezone(&chrono::Local);
        let mut note = super::kept(
            NoteKind::Text,
            "Monday",
            "22:13",
            "The pears are ready.",
            false,
        );
        note.written_at = unix.to_bits();
        let groups = group_by_day(&[note]);
        let shown = &groups[0].1[0];
        assert_eq!(groups[0].0, local.format("%A").to_string());
        assert_eq!(
            shown.time,
            format!("{}:{:02}", local.hour(), local.minute())
        );
        if local.offset().local_minus_utc() != 0 {
            assert_ne!(shown.time, format!("{}:{:02}", utc.hour(), utc.minute()));
        }
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
            seen: 0,
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
    fn removing_a_contact_drops_their_notes() {
        let mut people = sample_people();
        let id = people[0].id;
        assert!(!people[0].notes.is_empty());
        assert!(remove_person(&mut people, id));
        assert!(people.iter().all(|person| person.id != id));
        let read = decode_contacts(&encode_contacts(&people)).expect("the book");
        assert!(read.iter().all(|person| person.id != id));
        assert!(read.iter().all(|person| person.name != "Mara"));
        assert!(!remove_person(&mut people, id));
    }

    #[test]
    fn renaming_a_contact_keeps_their_notes() {
        let mut people = sample_people();
        let id = people[0].id;
        let notes = people[0].notes.clone();
        assert!(!rename_person(&mut people, id, "   "));
        assert_eq!(people[0].name, "Mara");
        assert!(!rename_person(&mut people, id, "Mara"));
        assert!(!rename_person(&mut people, 99, "Ned"));
        assert!(rename_person(&mut people, id, "  Ned  "));
        assert_eq!(people[0].name, "Ned");
        assert!(people[0].notes == notes);
        let read = decode_contacts(&encode_contacts(&people)).expect("the book");
        assert_eq!(read[0].name, "Ned");
        assert!(read[0].notes == notes);
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
    fn a_fast_path_carries_any_resource_and_lora_stays_small() {
        assert_eq!(bits_per_second(0), None);
        assert!(bits_per_second(21).expect("a sample") >= OPEN_PATH_BPS);
        assert!(bits_per_second(22).expect("a sample") < OPEN_PATH_BPS);
        assert_eq!(
            resource_carry(bits_per_second(21), false),
            ResourceCarry::Open
        );
        assert_eq!(
            resource_carry(Some(OPEN_PATH_BPS), true),
            ResourceCarry::Limited(SMALLEST_ATTACHMENT)
        );
        assert_eq!(
            resource_carry(Some(64_000), false),
            ResourceCarry::Limited(1_000_000)
        );
        assert_eq!(
            resource_carry(Some(32_000), false),
            ResourceCarry::Limited(256 * 1024)
        );
        assert_eq!(
            resource_carry(Some(16_000), false),
            ResourceCarry::Limited(64 * 1024)
        );
        assert_eq!(
            resource_carry(Some(8_000), false),
            ResourceCarry::Limited(16 * 1024)
        );
        assert_eq!(
            resource_carry(Some(7_999), false),
            ResourceCarry::Limited(SMALLEST_ATTACHMENT)
        );
        assert_eq!(
            resource_carry(None, false),
            ResourceCarry::Limited(SMALLEST_ATTACHMENT)
        );
        assert!(ResourceCarry::Open.allows(u64::MAX));
        assert!(!ResourceCarry::Limited(SMALLEST_ATTACHMENT).allows(SMALLEST_ATTACHMENT + 1));
    }

    #[test]
    fn the_file_choice_names_the_allowance() {
        assert_eq!(file_choice(Some(2_048)), "file <2k");
        assert_eq!(file_choice(Some(16 * 1024)), "file <16k");
        assert_eq!(file_choice(Some(64 * 1024)), "file <64k");
        assert_eq!(file_choice(Some(256 * 1024)), "file <256k");
        assert_eq!(file_choice(Some(1_000_000)), "file <1M");
        assert_eq!(file_choice(None), "file");
        assert_eq!(file_name_of("/tmp/../Pears.jpg"), "Pears.jpg");
        assert_eq!(numbered_file_name("window.xml", 1), "window.xml");
        assert_eq!(numbered_file_name("window.xml", 2), "window (2).xml");
        assert_eq!(numbered_file_name("notes", 2), "notes (2)");
        assert_eq!(file_name_of(r"C:\notes\Pears.jpg"), "Pears.jpg");
        assert_eq!(file_name_of(".."), "file");
    }

    #[test]
    fn a_file_can_be_a_note_on_its_own() {
        let mut person = Person {
            id: 1,
            name: "Alex".into(),
            identity: "ab".into(),
            seen: 0,
            notes: Vec::new(),
        };
        assert!(write_note(&mut person, "   ").is_none());
        let id = write_file_note(&mut person, "  ", None, "/tmp/Pears.jpg").expect("a file");
        assert_eq!(person.notes[0].words, "");
        assert_eq!(person.notes[0].file_name.as_deref(), Some("Pears.jpg"));
        let read = decode_contacts(&encode_contacts(&[person.clone()])).expect("the book");
        assert_eq!(read[0].notes[0].file_name.as_deref(), Some("Pears.jpg"));
        assert!(note_from_them(
            &mut person,
            "",
            1_700_000_000.0,
            None,
            Some("Pears.jpg")
        ));
        assert_eq!(person.notes.len(), 2);
        assert!(!note_from_them(
            &mut person,
            "",
            1_700_000_000.0,
            None,
            Some("/elsewhere/Pears.jpg")
        ));
        assert!(!note_from_them(
            &mut person,
            "",
            1_700_000_000.0,
            None,
            None
        ));
        let _ = id;
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
        let (width, cells) = code_mark(&contact_code("Alex", "19ab44c0e8d15f2076c3aa91b0d4e6f2"))
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
            seen: 40,
            notes: vec![Note {
                id: 40,
                kind: NoteKind::Text,
                day: "Monday".into(),
                time: "9:14".into(),
                words: "Line one.\nLine \\ two.".into(),
                mine: true,
                settled: false,
                failed: true,
                posted: true,
                digest: Some([0x11; 32]),
                place: Some(Place::from_degrees(40.7484, -73.9857).expect("place")),
                written_at: 1_700_000_000.0f64.to_bits(),
                reaction: Some(Reaction::Love),
                file_name: Some("Pears.jpg".into()),
                downloaded: false,
            }],
        }];
        let stored = encode_contacts(&people);
        let read = decode_contacts(&stored).expect("the book");
        assert_eq!(encode_contacts(&read), stored);
        assert_eq!(read[0].notes[0].words, "Line one.\nLine \\ two.");
        assert!(!read[0].notes[0].settled);
        assert!(read[0].notes[0].failed);
        assert!(read[0].notes[0].posted);
        assert_eq!(read[0].notes[0].digest, Some([0x11; 32]));
        assert_eq!(
            read[0].notes[0].place,
            Place::from_degrees(40.7484, -73.9857)
        );
        assert_eq!(read[0].notes[0].written_at, 1_700_000_000.0f64.to_bits());
        assert_eq!(read[0].notes[0].reaction, Some(Reaction::Love));
        assert_eq!(read[0].notes[0].file_name.as_deref(), Some("Pears.jpg"));
        assert!(!read[0].notes[0].downloaded);
        assert_eq!(read[0].seen, 40);
        assert!(decode_contacts("not a book").is_none());
        let older = "\
journal-contacts 1
1
7
Alex
ab
1
40
text
Monday
9:14
1
0
Hello
";
        let older = decode_contacts(older).expect("version 1");
        assert_eq!(older[0].notes[0].words, "Hello");
        assert!(older[0].notes[0].place.is_none());
        assert!(!older[0].notes[0].failed);
        assert_eq!(older[0].notes[0].written_at, 0);
        assert!(older[0].notes[0].reaction.is_none());
        assert_eq!(older[0].seen, 40);
        assert!(!has_unread(&older[0]));
        let version_two = "\
journal-contacts 2
1
7
Alex
ab
1
40
text
Monday
9:14
0
1
Hello
-
";
        let version_two = decode_contacts(version_two).expect("version 2");
        assert!(version_two[0].notes[0].place.is_none());
        assert_eq!(version_two[0].notes[0].written_at, 0);
        assert!(version_two[0].notes[0].reaction.is_none());
        assert_eq!(version_two[0].seen, 40);
        assert!(!has_unread(&version_two[0]));
        assert_eq!(next_person_id(&read), 8);
        note_ids_from(&read);
        let mut again = read;
        let id = write_note(&mut again[0], "still here").expect("a note");
        assert!(id > 40);
        assert!(again[0].notes[1].place.is_none());
        let placed = write_note(&mut again[0], &format!("Meet me {PLACEHOLDER}"));
        assert!(placed.is_some());
        assert!(again[0].notes.last().unwrap().place.is_none());
        let kept = super::write_placed_note(
            &mut again[0],
            &format!("Meet me {PLACEHOLDER}"),
            Place::from_degrees(41.0, -73.0),
        );
        assert!(kept.is_some());
        assert!(again[0].notes.last().unwrap().place.is_some());
        let dropped =
            super::write_placed_note(&mut again[0], "Meet me", Place::from_degrees(41.0, -73.0));
        assert!(dropped.is_some());
        assert!(again[0].notes.last().unwrap().place.is_none());
    }

    #[test]
    fn a_note_from_them_stays_unread_until_the_journal_is_opened() {
        let mut person = Person {
            id: 1,
            name: "Alex".into(),
            identity: "ab".into(),
            seen: 0,
            notes: Vec::new(),
        };
        write_note(&mut person, "I'm here.");
        assert!(!has_unread(&person));
        assert!(note_from_them(
            &mut person,
            "The pears are ready.",
            1_700_000_000.0,
            None,
            None
        ));
        assert!(has_unread(&person));
        let read = decode_contacts(&encode_contacts(&[person.clone()])).expect("the book");
        assert!(has_unread(&read[0]));
        assert!(mark_read(&mut person));
        assert!(!has_unread(&person));
        assert!(!mark_read(&mut person));
        let opened = decode_contacts(&encode_contacts(&[person])).expect("the opened book");
        assert!(!has_unread(&opened[0]));
        let older = "\
journal-contacts 3
1
1
Alex
ab
1
4
text
Monday
9:14
0
1
Hello
-
-
-
";
        let older = decode_contacts(older).expect("version 3");
        assert_eq!(older[0].seen, 4);
        assert!(!has_unread(&older[0]));
    }

    #[test]
    fn a_second_copy_of_their_note_stays_one_entry() {
        let mut person = Person {
            id: 1,
            name: "Alex".into(),
            identity: "ab".into(),
            seen: 0,
            notes: Vec::new(),
        };
        assert!(note_from_them(
            &mut person,
            "The pears are ready.",
            1_700_000_000.0,
            None,
            None
        ));
        person.notes[0].reaction = Some(Reaction::Yes);
        assert!(!note_from_them(
            &mut person,
            "  The pears are ready.  ",
            1_700_000_000.0,
            None,
            None
        ));
        assert_eq!(person.notes.len(), 1);
        assert_eq!(person.notes[0].reaction, Some(Reaction::Yes));
        assert!(note_from_them(
            &mut person,
            "The pears are ready.",
            1_700_000_001.0,
            None,
            None
        ));
        assert_eq!(person.notes.len(), 2);
        let place = Place::from_degrees(41.0, -73.0);
        assert!(note_from_them(
            &mut person,
            "Meet me here.",
            1_700_000_002.0,
            place,
            None
        ));
        assert!(!note_from_them(
            &mut person,
            "Meet me here.",
            1_700_000_002.0,
            place,
            None
        ));
        assert_eq!(person.notes.len(), 3);
    }

    #[test]
    fn a_note_that_gives_up_can_be_sent_again() {
        let mut people = vec![Person {
            id: 1,
            name: "Alex".into(),
            identity: "ab".into(),
            seen: 0,
            notes: Vec::new(),
        }];
        let id = write_note(&mut people[0], "The pears are ready.").expect("a note");
        assert!(fail_note(&mut people, id));
        assert!(people[0].notes[0].failed);
        assert!(!fail_note(&mut people, id));
        assert!(settle_note(&mut people, id));
        assert!(!people[0].notes[0].failed);
        people[0].notes[0].settled = false;
        people[0].notes[0].failed = true;
        let mut person = people.pop().expect("the contact");
        let (words, _, place, file_name) = retry_note(&mut person, id).expect("another try");
        assert_eq!(words, "The pears are ready.");
        assert!(place.is_none());
        assert!(file_name.is_none());
        assert!(!person.notes[0].failed);
        assert!(retry_note(&mut person, id).is_none());
    }

    #[test]
    fn their_file_is_marked_failed_when_the_bytes_never_arrive() {
        let mut person = Person {
            id: 1,
            name: "Alex".into(),
            identity: "ab".into(),
            seen: 0,
            notes: Vec::new(),
        };
        assert!(note_from_them(
            &mut person,
            "this photo",
            1_700_000_000.0,
            None,
            Some("Pears.jpg")
        ));
        assert!(fail_their_file(&mut person, 1_700_000_000.0, "Pears.jpg"));
        assert!(person.notes[0].failed);
        assert!(!fail_their_file(&mut person, 1_700_000_000.0, "Pears.jpg"));
        assert!(!fail_their_file(&mut person, 1_700_000_000.0, "other.jpg"));
        person.notes[0].mine = true;
        person.notes[0].failed = false;
        assert!(!fail_their_file(&mut person, 1_700_000_000.0, "Pears.jpg"));
        person.notes[0].mine = false;
        let id = person.notes[0].id;
        assert!(land_their_file(&mut person, id));
        assert!(person.notes[0].downloaded);
        assert!(!person.notes[0].failed);
        assert!(!fail_their_file(&mut person, 1_700_000_000.0, "Pears.jpg"));
        let stored = encode_contacts(&[person.clone()]);
        let read = decode_contacts(&stored).expect("the book");
        assert!(read[0].notes[0].downloaded);
    }

    #[test]
    fn a_reaction_marks_their_note_and_the_same_mark_clears_it() {
        let mut person = Person {
            id: 1,
            name: "Alex".into(),
            identity: "ab".into(),
            seen: 0,
            notes: vec![Note {
                id: 4,
                kind: NoteKind::Text,
                day: "Monday".into(),
                time: "9:14".into(),
                words: "The pears are ready.".into(),
                mine: false,
                settled: true,
                failed: false,
                posted: false,
                digest: None,
                place: None,
                written_at: 1_700_000_000.0f64.to_bits(),
                reaction: None,
                file_name: None,
                downloaded: false,
            }],
        };
        assert_eq!(
            react_to_note(&mut person, 4, Reaction::Ha),
            Some(1_700_000_000.0f64.to_bits())
        );
        assert_eq!(person.notes[0].reaction, Some(Reaction::Ha));
        assert_eq!(
            react_to_note(&mut person, 4, Reaction::Ha),
            Some(person.notes[0].written_at)
        );
        assert!(person.notes[0].reaction.is_none());
        person.notes[0].mine = true;
        assert!(react_to_note(&mut person, 4, Reaction::Yes).is_none());
        person.notes[0].mine = false;
        assert!(apply_reaction(
            &mut person,
            1_700_000_000.0,
            "",
            Some(Reaction::Oh)
        ));
        assert_eq!(person.notes[0].reaction, Some(Reaction::Oh));
        assert!(!apply_reaction(
            &mut person,
            1_700_000_000.0,
            "",
            Some(Reaction::Oh)
        ));
        assert!(apply_reaction(&mut person, 1_700_000_000.0, "", None));
        assert!(person.notes[0].reaction.is_none());
        person.notes[0].written_at = 0;
        person.notes[0].mine = true;
        assert!(apply_reaction(
            &mut person,
            0.0,
            "The pears are ready.",
            Some(Reaction::Down)
        ));
        assert_eq!(person.notes[0].reaction, Some(Reaction::Down));
    }

    #[test]
    fn a_place_map_covers_about_five_hundred_square_meters() {
        let place = Place::from_degrees(40.7484, -73.9857).expect("place");
        let window = super::map_window(place);
        let side = (f64::from(window.width) / window.scale)
            * super::meters_per_pixel(place.latitude(), window.zoom);
        let area = side * side;
        assert!((area - 500.0).abs() < 1.0, "{area}");
        assert!(!window.tiles.is_empty());
        let center_x = f64::from(window.width) / 2.0;
        let center_y = f64::from(window.height) / 2.0;
        assert!(window.tiles.iter().any(|tile| {
            center_x >= tile.left
                && center_x < tile.left + tile.edge
                && center_y >= tile.top
                && center_y < tile.top + tile.edge
        }));
        assert_eq!(
            super::shown_words(&format!("See you {PLACEHOLDER} there"), Some(place)),
            "See you there"
        );
    }
}
