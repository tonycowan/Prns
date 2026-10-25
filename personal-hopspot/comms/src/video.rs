//! A live picture on a link.
//!
//! Voice already rides the link one short moment at a time. A picture does the
//! same: a small frame, a few times a second, split into the same size of packet
//! so it can travel back the other way. The page shows the other person's picture.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

pub const NEEDS_CAMERA: &str = "Optio needs the camera for a video call.";

/// The long side of a picture. The short side follows the camera, so a phone
/// stays tall and a wide camera stays wide.
const PICTURE_LONG: u32 = 160;
const BITRATE_KBPS: i32 = 160;
const FPS: i32 = 8;
const HEADER: usize = 6;
const HELLO: u8 = 1;
const FRAME: u8 = 2;
const BYE: u8 = 3;
/// A frame that cannot fit on the link is dropped. The next key frame recovers it.
const MAX_FRAME: usize = 12_000;
const OUTBOX_LIMIT: usize = 40;

static STOP: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicU64 = AtomicU64::new(0);
static SEQ: AtomicU16 = AtomicU16::new(0);
static GATE: Mutex<()> = Mutex::new(());
static OUTBOX: OnceLock<Mutex<VecDeque<Vec<u8>>>> = OnceLock::new();
static FRAME_KIND: OnceLock<Mutex<VecDeque<(u16, bool)>>> = OnceLock::new();
static ASSEMBLY: OnceLock<Mutex<Assembly>> = OnceLock::new();
static PICTURE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilmKind {
    Hello,
    Frame,
    Bye,
    Other,
}

#[derive(Default)]
struct Assembly {
    seq: Option<u16>,
    parts: Vec<Option<Vec<u8>>>,
}

pub fn kind(bytes: &[u8]) -> FilmKind {
    match bytes {
        [b'F', HELLO, ..] => FilmKind::Hello,
        [b'F', FRAME, ..] => FilmKind::Frame,
        [b'F', BYE, ..] => FilmKind::Bye,
        _ => FilmKind::Other,
    }
}

pub fn hello_packet() -> Vec<u8> {
    vec![b'F', HELLO]
}

pub fn bye_packet() -> Vec<u8> {
    vec![b'F', BYE]
}

pub fn stage_hello() {
    outbox().push_front(hello_packet());
}

pub fn running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

pub fn stopped() -> bool {
    STOP.load(Ordering::Acquire) || !running()
}

/// Opens the camera and the encoder. Blocks until both are open, or until they refuse.
pub fn start() -> Result<(), &'static str> {
    let _gate = lock(&GATE);
    if running() {
        return Ok(());
    }
    let epoch = LIVE.fetch_add(1, Ordering::AcqRel) + 1;
    STOP.store(false, Ordering::Release);
    clear_queues();
    #[cfg(target_os = "macos")]
    {
        open_decoder()?;
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("journal-video".into())
            .spawn(move || picture_thread(ready_tx, epoch))
            .map_err(|_| crate::voice::DIDNT_CONNECT)?;
        return match ready_rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(words)) => {
                close_decoder();
                Err(words)
            }
            Err(_) => {
                STOP.store(true, Ordering::Release);
                LIVE.fetch_add(1, Ordering::AcqRel);
                crate::film::close();
                close_decoder();
                Err(NEEDS_CAMERA)
            }
        };
    }
    #[cfg(target_os = "android")]
    {
        let _ = epoch;
        match begin_film() {
            Ok(()) => {
                RUNNING.store(true, Ordering::Release);
                Ok(())
            }
            Err(words) => Err(words),
        }
    }
}

pub fn stop() {
    note("video stop");
    LIVE.fetch_add(1, Ordering::AcqRel);
    STOP.store(true, Ordering::Release);
    RUNNING.store(false, Ordering::Release);
    #[cfg(target_os = "macos")]
    {
        crate::film::close();
        close_decoder();
    }
    #[cfg(target_os = "android")]
    end_film();
    *picture_slot() = None;
}

pub fn take_one() -> Option<Vec<u8>> {
    outbox().pop_front()
}

/// Joins the pieces of one frame and keeps the latest picture for the page.
pub fn hear(bytes: &[u8]) {
    if kind(bytes) != FilmKind::Frame {
        return;
    }
    let finished = {
        let mut assembly = assembly();
        push_part(&mut assembly, bytes)
    };
    let Some((_seq, frame)) = finished else {
        return;
    };
    show_frame(&frame);
}

pub fn picture() -> Option<String> {
    picture_slot().clone()
}

fn clear_queues() {
    outbox().clear();
    *assembly() = Assembly::default();
    *picture_slot() = None;
    SEQ.store(0, Ordering::Release);
}

fn enqueue(payload: &[u8]) {
    if payload.is_empty() || payload.len() > MAX_FRAME {
        let kind = if is_key(payload) { "key" } else { "delta" };
        note(format!("dropped a {} byte {kind} frame", payload.len()));
        return;
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    remember_frame(seq, is_key(payload));
    let mut queued = outbox();
    for packet in packets_for(seq, payload) {
        queued.push_back(packet);
    }
    let dropped = trim_outbox(&mut queued);
    log_dropped(&dropped);
}

fn packets_for(seq: u16, payload: &[u8]) -> Vec<Vec<u8>> {
    let room = chunk_len();
    let count = payload.len().div_ceil(room).clamp(1, 255);
    payload
        .chunks(room)
        .take(count)
        .enumerate()
        .map(|(part, chunk)| {
            let mut packet = Vec::with_capacity(HEADER + chunk.len());
            packet.push(b'F');
            packet.push(FRAME);
            packet.extend(seq.to_be_bytes());
            packet.push(part as u8);
            packet.push(count as u8);
            packet.extend_from_slice(chunk);
            packet
        })
        .collect()
}

fn push_part(assembly: &mut Assembly, packet: &[u8]) -> Option<(u16, Vec<u8>)> {
    if packet.len() < HEADER || packet[0] != b'F' || packet[1] != FRAME {
        return None;
    }
    let seq = u16::from_be_bytes([packet[2], packet[3]]);
    let part = packet[4];
    let count = packet[5];
    if count == 0 || part >= count {
        return None;
    }
    if assembly.seq != Some(seq) || assembly.parts.len() != count as usize {
        note_abandoned(assembly);
        assembly.seq = Some(seq);
        assembly.parts = vec![None; count as usize];
    }
    assembly.parts[part as usize] = Some(packet[HEADER..].to_vec());
    if assembly.parts.iter().any(Option::is_none) {
        return None;
    }
    let mut frame = Vec::new();
    for piece in assembly.parts.drain(..) {
        frame.extend(piece.unwrap_or_default());
    }
    assembly.seq = None;
    Some((seq, frame))
}

fn chunk_len() -> usize {
    personal_rns::engine::MAX_SEND_TO_LINK_PLAINTEXT_LEN
        .saturating_sub(HEADER)
        .max(1)
}

fn is_key(payload: &[u8]) -> bool {
    payload.first().is_some_and(|byte| byte & 1 == 0)
}

fn remember_frame(seq: u16, key: bool) {
    let mut kinds = lock(FRAME_KIND.get_or_init(|| Mutex::new(VecDeque::new())));
    kinds.push_back((seq, key));
    while kinds.len() > 64 {
        kinds.pop_front();
    }
}

fn frame_was_key(seq: u16) -> bool {
    lock(FRAME_KIND.get_or_init(|| Mutex::new(VecDeque::new())))
        .iter()
        .rev()
        .find(|(id, _)| *id == seq)
        .is_some_and(|(_, key)| *key)
}

fn packet_part(packet: &[u8]) -> u8 {
    packet.get(4).copied().unwrap_or(0)
}

fn packet_is_key(packet: &[u8]) -> bool {
    packet_part(packet) == 0 && packet.get(HEADER).is_some_and(|byte| byte & 1 == 0)
}

struct QueuedFrame {
    start: usize,
    end: usize,
    key: bool,
}

fn queued_frames(outbox: &VecDeque<Vec<u8>>, from: usize) -> Vec<QueuedFrame> {
    let mut frames = Vec::new();
    let mut index = from;
    while index < outbox.len() {
        if packet_part(&outbox[index]) != 0 {
            index += 1;
            continue;
        }
        let start = index;
        let key = packet_is_key(&outbox[index]);
        index += 1;
        while index < outbox.len() && packet_part(&outbox[index]) != 0 {
            index += 1;
        }
        frames.push(QueuedFrame {
            start,
            end: index,
            key,
        });
    }
    frames
}

/// Drops whole frames until the queue fits. A frame whose first part has already
/// gone out stays. A newer key frame makes older unsent pictures useless, so
/// those go first, and then the oldest remaining pictures, until the queue fits.
fn trim_outbox(outbox: &mut VecDeque<Vec<u8>>) -> Vec<Vec<u8>> {
    let protected = if outbox
        .front()
        .is_some_and(|packet| packet_part(packet) != 0)
    {
        outbox
            .iter()
            .position(|packet| packet_part(packet) == 0)
            .unwrap_or(outbox.len())
    } else {
        0
    };
    let mut dropped = Vec::new();
    let frames = queued_frames(outbox, protected);
    if let Some(key) = frames.iter().rev().find(|frame| frame.key) {
        let stale = key.start.saturating_sub(protected);
        for _ in 0..stale {
            if let Some(packet) = outbox.remove(protected) {
                dropped.push(packet);
            }
        }
    }
    while outbox.len() > OUTBOX_LIMIT {
        let frames = queued_frames(outbox, protected);
        let newest_key = frames
            .iter()
            .rev()
            .find(|frame| frame.key)
            .map(|frame| frame.start);
        let victim = frames
            .iter()
            .find(|frame| !frame.key)
            .or_else(|| frames.iter().find(|frame| Some(frame.start) != newest_key));
        let Some(victim) = victim else {
            break;
        };
        let start = victim.start;
        let end = victim.end;
        for _ in start..end {
            if let Some(packet) = outbox.remove(start) {
                dropped.push(packet);
            }
        }
    }
    dropped
}

fn packet_seq(packet: &[u8]) -> u16 {
    if packet.len() < 4 {
        return 0;
    }
    u16::from_be_bytes([packet[2], packet[3]])
}

fn log_dropped(packets: &[Vec<u8>]) {
    let mut index = 0;
    while index < packets.len() {
        let seq = packet_seq(&packets[index]);
        let mut end = index + 1;
        while end < packets.len() && packet_seq(&packets[end]) == seq {
            end += 1;
        }
        let count = packets[index].get(5).copied().unwrap_or(0);
        let start = packets[index].get(4).copied().unwrap_or(0);
        let last = packets[end - 1].get(4).copied().unwrap_or(0);
        let kind = if frame_was_key(seq) { "key" } else { "delta" };
        note(format!("queue dropped {kind} {start}-{last}/{count}"));
        index = end;
    }
}

fn note_abandoned(assembly: &Assembly) {
    let Some(_) = assembly.seq else {
        return;
    };
    let missing = assembly.parts.iter().filter(|part| part.is_none()).count();
    let count = assembly.parts.len();
    if missing == 0 || missing == count {
        return;
    }
    let kind = match assembly.parts.first().and_then(|part| part.as_ref()) {
        Some(bytes) if bytes.first().is_some_and(|byte| byte & 1 == 0) => "key",
        Some(_) => "delta",
        None => "frame",
    };
    let missing_parts: Vec<usize> = assembly
        .parts
        .iter()
        .enumerate()
        .filter(|(_, part)| part.is_none())
        .map(|(index, _)| index)
        .collect();
    note(format!(
        "abandoned {kind} frame, missing {missing} of {count} {missing_parts:?}"
    ));
}

fn outbox() -> MutexGuard<'static, VecDeque<Vec<u8>>> {
    lock(OUTBOX.get_or_init(|| Mutex::new(VecDeque::new())))
}

fn assembly() -> MutexGuard<'static, Assembly> {
    lock(ASSEMBLY.get_or_init(|| Mutex::new(Assembly::default())))
}

fn picture_slot() -> MutexGuard<'static, Option<String>> {
    lock(PICTURE.get_or_init(|| Mutex::new(None)))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn note(message: impl std::fmt::Display) {
    eprintln!("touchbase: {message}");
    let _ = std::io::Write::flush(&mut std::io::stderr());
}

fn show_frame(frame: &[u8]) {
    #[cfg(target_os = "macos")]
    {
        let decoder = lock(&DECODER);
        let Some(codec) = decoder.as_ref() else {
            note(format!("picture {} bytes, decoder closed", frame.len()));
            return;
        };
        let Some((width, height, picture)) = codec.decode(frame) else {
            note(format!(
                "picture {} bytes {}, {}",
                frame.len(),
                frame_head(frame),
                codec.decode_error()
            ));
            return;
        };
        let Some(url) = jpeg_url(&picture, width, height) else {
            note(format!("picture {width}x{height}, jpeg failed"));
            return;
        };
        drop(decoder);
        *picture_slot() = Some(url);
        note(format!("picture set {width}x{height}"));
    }
    #[cfg(target_os = "android")]
    {
        let Some(jpeg) = android_jpeg(frame) else {
            note(format!("picture {} bytes, no jpeg", frame.len()));
            return;
        };
        *picture_slot() = Some(format!("data:image/jpeg;base64,{}", base64(&jpeg)));
        note(format!("picture set, jpeg {} bytes", jpeg.len()));
    }
}

#[cfg(target_os = "macos")]
fn jpeg_url(i420: &[u8], width: u32, height: u32) -> Option<String> {
    let rgb = rgb_from_i420(i420, width, height)?;
    let mut jpeg = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 55);
    image::ImageEncoder::write_image(encoder, &rgb, width, height, image::ExtendedColorType::Rgb8)
        .ok()?;
    Some(format!("data:image/jpeg;base64,{}", base64(&jpeg)))
}

#[cfg(target_os = "macos")]
fn rgb_from_i420(i420: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let y_size = (width as usize).checked_mul(height as usize)?;
    let c_size = y_size / 4;
    if width % 2 != 0 || height % 2 != 0 || i420.len() < y_size + c_size + c_size {
        return None;
    }
    let y_plane = &i420[..y_size];
    let u_plane = &i420[y_size..y_size + c_size];
    let v_plane = &i420[y_size + c_size..y_size + c_size + c_size];
    let mut rgb = vec![0u8; y_size * 3];
    for row in 0..height {
        for col in 0..width {
            let index = (row * width + col) as usize;
            let chroma = ((row / 2) * (width / 2) + col / 2) as usize;
            let y = i32::from(y_plane[index]).saturating_sub(16);
            let u = i32::from(u_plane[chroma]) - 128;
            let v = i32::from(v_plane[chroma]) - 128;
            let red = ((298 * y + 409 * v) >> 8).clamp(0, 255);
            let green = ((298 * y - 100 * u - 208 * v) >> 8).clamp(0, 255);
            let blue = ((298 * y + 516 * u) >> 8).clamp(0, 255);
            rgb[index * 3] = red as u8;
            rgb[index * 3 + 1] = green as u8;
            rgb[index * 3 + 2] = blue as u8;
        }
    }
    Some(rgb)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let word = (u32::from(bytes[index]) << 16)
            | (u32::from(bytes[index + 1]) << 8)
            | u32::from(bytes[index + 2]);
        out.push(TABLE[((word >> 18) & 63) as usize] as char);
        out.push(TABLE[((word >> 12) & 63) as usize] as char);
        out.push(TABLE[((word >> 6) & 63) as usize] as char);
        out.push(TABLE[(word & 63) as usize] as char);
        index += 3;
    }
    let rest = bytes.len() - index;
    if rest == 1 {
        let word = u32::from(bytes[index]) << 16;
        out.push(TABLE[((word >> 18) & 63) as usize] as char);
        out.push(TABLE[((word >> 12) & 63) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rest == 2 {
        let word = (u32::from(bytes[index]) << 16) | (u32::from(bytes[index + 1]) << 8);
        out.push(TABLE[((word >> 18) & 63) as usize] as char);
        out.push(TABLE[((word >> 12) & 63) as usize] as char);
        out.push(TABLE[((word >> 6) & 63) as usize] as char);
        out.push('=');
    }
    out
}

#[cfg(target_os = "macos")]
extern "C" {
    fn optio_vpx_encoder(w: i32, h: i32, kbps: i32, fps: i32) -> *mut std::ffi::c_void;
    fn optio_vpx_encode(
        enc: *mut std::ffi::c_void,
        i420: *const u8,
        pts: i64,
        key: i32,
        out_len: *mut i32,
    ) -> *mut u8;
    fn optio_vpx_encoder_free(enc: *mut std::ffi::c_void);
    fn optio_vpx_decoder() -> *mut std::ffi::c_void;
    fn optio_vpx_decode(
        dec: *mut std::ffi::c_void,
        buf: *const u8,
        len: i32,
        w: *mut i32,
        h: *mut i32,
    ) -> *mut u8;
    fn optio_vpx_error(dec: *mut std::ffi::c_void) -> *const std::ffi::c_char;
    fn optio_vpx_decoder_free(dec: *mut std::ffi::c_void);
    fn optio_vpx_free(bytes: *mut u8);
}

#[cfg(target_os = "macos")]
struct Codec {
    raw: *mut std::ffi::c_void,
    encode: bool,
    width: u32,
    height: u32,
}

#[cfg(target_os = "macos")]
// SAFETY: `raw` is created and freed by the codec functions. The value is moved
// onto the thread that uses it, or kept behind the decoder mutex.
unsafe impl Send for Codec {}

#[cfg(target_os = "macos")]
impl Drop for Codec {
    fn drop(&mut self) {
        if self.raw.is_null() {
            return;
        }
        // SAFETY: `raw` came from the matching open function and has not been freed.
        unsafe {
            if self.encode {
                optio_vpx_encoder_free(self.raw);
            } else {
                optio_vpx_decoder_free(self.raw);
            }
        }
        self.raw = std::ptr::null_mut();
    }
}

#[cfg(target_os = "macos")]
impl Codec {
    fn encoder(width: u32, height: u32) -> Option<Self> {
        // SAFETY: the width, height, and rate are this call's picture size.
        let raw = unsafe { optio_vpx_encoder(width as i32, height as i32, BITRATE_KBPS, FPS) };
        if raw.is_null() {
            None
        } else {
            Some(Self {
                raw,
                encode: true,
                width,
                height,
            })
        }
    }

    fn encode(&self, i420: &[u8], pts: i64, key: bool) -> Option<Vec<u8>> {
        let expect = (self.width as usize) * (self.height as usize) * 3 / 2;
        if i420.len() != expect {
            return None;
        }
        let mut len = 0i32;
        // SAFETY: `raw` is a live encoder. `i420` is the packed picture of the size it was opened with.
        let ptr =
            unsafe { optio_vpx_encode(self.raw, i420.as_ptr(), pts, i32::from(key), &mut len) };
        owned_bytes(ptr, len)
    }

    fn decode(&self, frame: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        if frame.is_empty() || frame.len() > i32::MAX as usize {
            return None;
        }
        let mut width = 0i32;
        let mut height = 0i32;
        // SAFETY: `raw` is a live decoder. The frame is the joined link payload.
        let ptr = unsafe {
            optio_vpx_decode(
                self.raw,
                frame.as_ptr(),
                frame.len() as i32,
                &mut width,
                &mut height,
            )
        };
        let picture = owned_bytes(ptr, (width as usize * height as usize * 3 / 2) as i32)?;
        if width <= 0 || height <= 0 {
            return None;
        }
        Some((width as u32, height as u32, picture))
    }

    fn decode_error(&self) -> String {
        // SAFETY: `raw` is the live decoder. The text stays valid until the next decode.
        let ptr = unsafe { optio_vpx_error(self.raw) };
        if ptr.is_null() {
            return String::new();
        }
        // SAFETY: the shim returns a NUL-terminated string it owns.
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(target_os = "macos")]
fn frame_head(frame: &[u8]) -> String {
    frame
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(target_os = "macos")]
fn owned_bytes(ptr: *mut u8, len: i32) -> Option<Vec<u8>> {
    if ptr.is_null() || len <= 0 {
        return None;
    }
    // SAFETY: the shim allocated `len` bytes and gave this pointer to us. The copy finishes
    // before the shim frees it.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
    // SAFETY: this is the same pointer the shim allocated, freed once.
    unsafe { optio_vpx_free(ptr) };
    Some(bytes)
}

#[cfg(target_os = "macos")]
static DECODER: Mutex<Option<Codec>> = Mutex::new(None);

#[cfg(target_os = "macos")]
fn open_decoder() -> Result<(), &'static str> {
    // SAFETY: a fresh decoder. Nothing else holds this pointer.
    let raw = unsafe { optio_vpx_decoder() };
    if raw.is_null() {
        return Err(crate::voice::DIDNT_CONNECT);
    }
    *lock(&DECODER) = Some(Codec {
        raw,
        encode: false,
        width: 0,
        height: 0,
    });
    Ok(())
}

#[cfg(target_os = "macos")]
fn close_decoder() {
    lock(&DECODER).take();
}

#[cfg(target_os = "macos")]
struct RunningGuard {
    epoch: u64,
}

#[cfg(target_os = "macos")]
impl Drop for RunningGuard {
    fn drop(&mut self) {
        if LIVE.load(Ordering::Acquire) == self.epoch {
            RUNNING.store(false, Ordering::Release);
            STOP.store(true, Ordering::Release);
        }
    }
}

#[cfg(target_os = "macos")]
fn picture_thread(ready: std::sync::mpsc::SyncSender<Result<(), &'static str>>, epoch: u64) {
    if crate::film::open().is_err() {
        let _ = ready.send(Err(NEEDS_CAMERA));
        return;
    }
    if LIVE.load(Ordering::Acquire) != epoch {
        crate::film::close();
        let _ = ready.send(Err(NEEDS_CAMERA));
        return;
    }
    RUNNING.store(true, Ordering::Release);
    let _running = RunningGuard { epoch };
    let _ = ready.send(Ok(()));
    note("camera open");
    let mut encoder: Option<Codec> = None;
    let mut pts = 0i64;
    let mut key = true;
    while LIVE.load(Ordering::Acquire) == epoch && !STOP.load(Ordering::Acquire) {
        if let Some((width, height, frame)) = crate::film::take() {
            if encoder.is_none() {
                encoder = Codec::encoder(width, height);
                if encoder.is_some() {
                    note(format!("picture {width}x{height}"));
                } else {
                    note("the picture couldn't be opened");
                }
            }
            if let Some(encoder) = encoder.as_ref() {
                match encoder.encode(&frame, pts, key) {
                    Some(bytes) => {
                        note(format!("sent {} bytes", bytes.len()));
                        enqueue(&bytes);
                        key = false;
                        pts += 1;
                    }
                    None => note("a frame couldn't be sent"),
                }
            }
        }
        std::thread::sleep(Duration::from_millis(125));
    }
    crate::film::close();
}

/// The size to send for a camera frame. The long side stays [`PICTURE_LONG`] and
/// the short side keeps the camera's shape, rounded to an even count.
pub(crate) fn picture_size(width: u32, height: u32) -> Option<(u32, u32)> {
    if width < 2 || height < 2 {
        return None;
    }
    let (mut across, mut down) = if width >= height {
        let down = (u64::from(PICTURE_LONG) * u64::from(height) / u64::from(width)) as u32;
        (PICTURE_LONG, down)
    } else {
        let across = (u64::from(PICTURE_LONG) * u64::from(width) / u64::from(height)) as u32;
        (across, PICTURE_LONG)
    };
    across &= !1;
    down &= !1;
    if across < 2 || down < 2 {
        None
    } else {
        Some((across, down))
    }
}

/// Scales a camera frame down to the picture a call sends, keeping its shape.
#[cfg(target_os = "macos")]
pub(crate) fn i420_from_bgra(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    if width < 2 || height < 2 || stride < width.saturating_mul(4) {
        return None;
    }
    let (out_w, out_h) = picture_size(width, height)?;
    let mut y = vec![0u8; (out_w * out_h) as usize];
    let mut u = vec![128u8; (out_w * out_h / 4) as usize];
    let mut v = vec![128u8; (out_w * out_h / 4) as usize];
    for row in 0..out_h {
        let sy = (u64::from(row) * u64::from(height) / u64::from(out_h)) as u32;
        for col in 0..out_w {
            let sx = (u64::from(col) * u64::from(width) / u64::from(out_w)) as u32;
            let at = (sy as usize)
                .checked_mul(stride as usize)?
                .checked_add((sx as usize) * 4)?;
            let pixel = src.get(at..at + 4)?;
            let blue = i32::from(pixel[0]);
            let green = i32::from(pixel[1]);
            let red = i32::from(pixel[2]);
            let yy = (((66 * red + 129 * green + 25 * blue + 128) >> 8) + 16).clamp(0, 255);
            y[(row * out_w + col) as usize] = yy as u8;
            if row % 2 == 0 && col % 2 == 0 {
                let uu = (((-38 * red - 74 * green + 112 * blue + 128) >> 8) + 128).clamp(0, 255);
                let vv = (((112 * red - 94 * green - 18 * blue + 128) >> 8) + 128).clamp(0, 255);
                let chroma = ((row / 2) * (out_w / 2) + col / 2) as usize;
                u[chroma] = uu as u8;
                v[chroma] = vv as u8;
            }
        }
    }
    y.extend(u);
    y.extend(v);
    Some((out_w, out_h, y))
}

#[cfg(target_os = "android")]
fn begin_film() -> Result<(), &'static str> {
    let code = with_activity(|env, activity| {
        Ok(env.call_method(activity, "beginFilm", "()I", &[])?.i()?)
    });
    match code {
        Ok(0) => Ok(()),
        _ => Err(NEEDS_CAMERA),
    }
}

#[cfg(target_os = "android")]
fn end_film() {
    let _ = with_activity(|env, activity| {
        env.call_method(activity, "endFilm", "()V", &[])?;
        Ok(())
    });
}

#[cfg(target_os = "android")]
fn android_jpeg(frame: &[u8]) -> Option<Vec<u8>> {
    if frame.is_empty() || frame.len() > i32::MAX as usize {
        return None;
    }
    with_activity(|env, activity| {
        let array = env.byte_array_from_slice(frame)?;
        let value = env
            .call_method(
                activity,
                "decodeFilm",
                "([B)[B",
                &[jni::objects::JValue::Object(&array)],
            )?
            .l()?;
        if value.is_null() {
            return Ok(None);
        }
        let bytes = env.convert_byte_array(jni::objects::JByteArray::from(value))?;
        Ok(Some(bytes))
    })
    .ok()
    .flatten()
}

#[cfg(target_os = "android")]
fn with_activity<T>(
    body: impl FnOnce(&mut jni::JNIEnv, &jni::objects::JObject) -> jni::errors::Result<T>,
) -> jni::errors::Result<T> {
    let ctx = ndk_context::android_context();
    if ctx.vm().is_null() || ctx.context().is_null() {
        return Err(jni::errors::Error::NullPtr("android context"));
    }
    // SAFETY: the Android binding installs this process JavaVM before `main`.
    // `JavaVM::from_raw` only wraps that pointer and does not destroy the VM on drop.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }?;
    let mut env = vm.attach_current_thread()?;
    // SAFETY: the same binding publishes the activity as a live local reference.
    // `JObject` does not delete that reference when dropped.
    let activity = unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) };
    body(&mut env, &activity)
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_dev_dioxus_main_Film_nativeOffer(
    mut env: jni::JNIEnv,
    _this: jni::objects::JObject,
    frame: jni::objects::JByteArray,
) {
    let Ok(bytes) = env.convert_byte_array(frame) else {
        return;
    };
    enqueue(&bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_camera_stays_wide_and_a_tall_one_stays_tall() {
        assert_eq!(picture_size(1920, 1080), Some((160, 90)));
        assert_eq!(picture_size(1080, 1920), Some((90, 160)));
        assert_eq!(picture_size(640, 480), Some((160, 120)));
        assert_eq!(picture_size(480, 640), Some((120, 160)));
    }

    #[test]
    fn a_picture_splits_across_link_packets_and_comes_back() {
        let payload = vec![7u8; 1000];
        let packets = packets_for(4, &payload);
        assert!(packets.len() > 1);
        let room = chunk_len() + HEADER;
        assert!(packets.iter().all(|packet| packet.len() <= room));
        let mut assembly = Assembly::default();
        let mut whole = None;
        for packet in &packets {
            whole = push_part(&mut assembly, packet);
        }
        let (seq, frame) = whole.expect("one picture");
        assert_eq!(seq, 4);
        assert_eq!(frame, payload);
        assert!(push_part(&mut assembly, b"V\x02").is_none());
    }

    #[test]
    fn a_full_queue_keeps_the_newest_key_frame_whole() {
        let room = chunk_len();
        let key = |mark: u8| {
            let mut frame = vec![0u8; room * 22];
            frame[0] = mark;
            frame
        };
        let delta = {
            let mut frame = vec![1u8; room * 6];
            frame[0] = 0x21;
            frame
        };
        let mut outbox = VecDeque::new();
        let mut inflight = packets_for(1, &delta);
        inflight.remove(0);
        let protected = inflight.len();
        outbox.extend(inflight);
        outbox.extend(packets_for(2, &key(0x10)));
        for seq in 3..10 {
            outbox.extend(packets_for(seq, &delta));
        }
        outbox.extend(packets_for(10, &key(0x30)));
        assert!(outbox.len() > OUTBOX_LIMIT);
        let _dropped = trim_outbox(&mut outbox);
        assert!(outbox.len() <= OUTBOX_LIMIT);
        for packet in outbox.iter().take(protected) {
            assert_eq!(packet_seq(packet), 1);
            assert_ne!(packet_part(packet), 0);
        }
        let mut index = protected;
        let mut saw_newest = false;
        while index < outbox.len() {
            assert_eq!(packet_part(&outbox[index]), 0);
            let count = outbox[index][5] as usize;
            let seq = packet_seq(&outbox[index]);
            for part in 0..count {
                assert_eq!(packet_part(&outbox[index]), part as u8);
                assert_eq!(packet_seq(&outbox[index]), seq);
                index += 1;
            }
            if seq == 10 {
                saw_newest = true;
            }
            assert!(
                seq == 1 || seq == 10,
                "stale pictures ahead of the new key frame are dropped"
            );
        }
        assert!(saw_newest, "the newest key frame stays");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_flat_colour_survives_the_picture_codec() {
        let mut picture = vec![180u8; (160 * 120) as usize];
        picture.extend(vec![90u8; (160 * 120 / 4) as usize]);
        picture.extend(vec![200u8; (160 * 120 / 4) as usize]);
        let encoder = Codec::encoder(160, 120).expect("encoder");
        // SAFETY: a decoder used only by this test, freed when `Codec` drops.
        let raw = unsafe { optio_vpx_decoder() };
        let decoder = Codec {
            raw,
            encode: false,
            width: 0,
            height: 0,
        };
        assert!(!decoder.raw.is_null(), "decoder");
        let mut encoded = encoder.encode(&picture, 0, true);
        if encoded.as_ref().is_none_or(|bytes| bytes.is_empty()) {
            encoded = encoder.encode(&picture, 1, true);
        }
        let encoded = encoded.expect("frame");
        assert!(encoded.len() <= MAX_FRAME);
        let (width, height, back) = decoder.decode(&encoded).expect("decoded");
        assert_eq!((width, height), (160, 120));
        let center = (120 / 2 * 160 + 160 / 2) as usize;
        let y = back[center];
        let chroma = ((120 / 2 / 2) * (160 / 2) + 160 / 2 / 2) as usize;
        let u = back[(160 * 120) as usize + chroma];
        let v = back[(160 * 120) as usize + (160 * 120 / 4) as usize + chroma];
        assert!((i32::from(y) - 180).abs() < 40, "y {y}");
        assert!((i32::from(u) - 90).abs() < 40, "u {u}");
        assert!((i32::from(v) - 200).abs() < 40, "v {v}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_blue_camera_frame_lands_on_the_blue_planes() {
        let width = 320u32;
        let height = 240u32;
        let mut bgra = vec![0u8; (width * height * 4) as usize];
        for pixel in bgra.chunks_exact_mut(4) {
            pixel[0] = 255;
            pixel[3] = 255;
        }
        let (out_w, out_h, i420) = i420_from_bgra(&bgra, width, height, width * 4).expect("planes");
        assert_eq!((out_w, out_h), (160, 120));
        let center = (out_h / 2 * out_w + out_w / 2) as usize;
        let y = i420[center];
        assert!((i32::from(y) - 41).abs() <= 2, "y {y}");
    }
}
