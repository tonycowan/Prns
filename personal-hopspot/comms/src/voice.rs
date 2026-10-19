//! A live voice call on a link.
//!
//! The words of a note travel as a message. A voice call does not: the microphone
//! is encoded one short moment at a time and those packets ride the link directly.
//! Each moment is full band, at the highest rate that still fits in one link packet,
//! so a second moment can travel back the other way.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub const NEEDS_MICROPHONE: &str = "Awill needs the microphone for a voice call.";
pub const DIDNT_CONNECT: &str = "The call didn't connect.";
pub const WASNT_ANSWERED: &str = "The call wasn't answered.";

const RATE: u32 = 48_000;
const FRAME: usize = 960;
const HEADER: usize = 6;
const HELLO: u8 = 1;
const AUDIO: u8 = 2;
const BYE: u8 = 3;
/// Two moments. Anything older is dropped so one side cannot fill the link.
const OUTBOX_LIMIT: usize = 2;
const PLAY_LIMIT: usize = RATE as usize / 2;

static STOP: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Bumped on every start and stop so a previous audio thread cannot keep the next call.
static LIVE: AtomicU64 = AtomicU64::new(0);
static GATE: Mutex<()> = Mutex::new(());
static CAPTURE: OnceLock<Mutex<VecDeque<i16>>> = OnceLock::new();
static PLAY: OnceLock<Mutex<VecDeque<i16>>> = OnceLock::new();
static OUTBOX: OnceLock<Mutex<VecDeque<Vec<u8>>>> = OnceLock::new();
static INBOX: OnceLock<Mutex<VecDeque<(u16, Vec<u8>)>>> = OnceLock::new();
static ASSEMBLY: OnceLock<Mutex<Assembly>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceKind {
    Hello,
    Audio,
    Bye,
    Other,
}

#[derive(Default)]
struct Assembly {
    seq: Option<u16>,
    parts: Vec<Option<Vec<u8>>>,
}

pub fn kind(bytes: &[u8]) -> VoiceKind {
    match bytes {
        [b'V', HELLO, ..] => VoiceKind::Hello,
        [b'V', AUDIO, ..] => VoiceKind::Audio,
        [b'V', BYE, ..] => VoiceKind::Bye,
        _ => VoiceKind::Other,
    }
}

pub fn hello_packet() -> Vec<u8> {
    vec![b'V', HELLO]
}

pub fn bye_packet() -> Vec<u8> {
    vec![b'V', BYE]
}

/// Puts the hello ahead of any audio so the other person hears that we answered
/// before the microphone takes the link.
pub fn stage_hello() {
    outbox().push_front(hello_packet());
}

pub fn running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

pub fn stopped() -> bool {
    STOP.load(Ordering::Acquire) || !running()
}

/// Opens the microphone and the speaker. Blocks until both are open, or until
/// the devices refuse.
pub fn start() -> Result<(), &'static str> {
    let _gate = GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if running() {
        return Ok(());
    }
    let epoch = LIVE.fetch_add(1, Ordering::AcqRel) + 1;
    STOP.store(false, Ordering::Release);
    clear_queues();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("journal-voice".into())
        .stack_size(2 * 1024 * 1024)
        .spawn(move || audio_thread(ready_tx, epoch))
        .map_err(|_| DIDNT_CONNECT)?;
    match ready_rx.recv() {
        Ok(result) => result,
        Err(_) => Err(DIDNT_CONNECT),
    }
}

pub fn stop() {
    LIVE.fetch_add(1, Ordering::AcqRel);
    STOP.store(true, Ordering::Release);
    RUNNING.store(false, Ordering::Release);
}

pub fn take_one() -> Option<Vec<u8>> {
    outbox().pop_front()
}

/// Joins the pieces of one moment and queues it for the speaker.
pub fn hear(bytes: &[u8]) {
    if kind(bytes) != VoiceKind::Audio {
        return;
    }
    let finished = {
        let mut assembly = assembly();
        push_part(&mut assembly, bytes)
    };
    let Some((seq, frame)) = finished else {
        return;
    };
    let mut inbox = inbox();
    inbox.push_back((seq, frame));
    while inbox.len() > 8 {
        inbox.pop_front();
    }
}

fn audio_thread(ready: std::sync::mpsc::SyncSender<Result<(), &'static str>>, epoch: u64) {
    #[cfg(target_os = "android")]
    attach_audio_thread();

    let opened = open_devices();
    let (input, output, input_rate, output_rate) = match opened {
        Ok(devices) => devices,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let encoder = match highest_encoder() {
        Ok(encoder) => encoder,
        Err(_) => {
            let _ = ready.send(Err(DIDNT_CONNECT));
            return;
        }
    };
    let decoder = match opus::Decoder::new(RATE, opus::Channels::Mono) {
        Ok(decoder) => decoder,
        Err(_) => {
            let _ = ready.send(Err(DIDNT_CONNECT));
            return;
        }
    };
    if LIVE.load(Ordering::Acquire) != epoch {
        let _ = ready.send(Err(DIDNT_CONNECT));
        return;
    }
    RUNNING.store(true, Ordering::Release);
    let _running = RunningGuard { epoch };
    let _ = ready.send(Ok(()));
    pump(
        epoch,
        input,
        output,
        input_rate,
        output_rate,
        encoder,
        decoder,
    );
}

struct RunningGuard {
    epoch: u64,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if LIVE.load(Ordering::Acquire) == self.epoch {
            RUNNING.store(false, Ordering::Release);
            STOP.store(true, Ordering::Release);
        }
    }
}

#[cfg(target_os = "android")]
fn attach_audio_thread() {
    let ctx = ndk_context::android_context();
    // SAFETY: ndk-context publishes the process JavaVM for the life of the activity.
    // `JavaVM::from_raw` only wraps that pointer and does not destroy the VM on drop.
    let Ok(vm) = (unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }) else {
        return;
    };
    // A permanent attach stays for this thread, which is what AAudio needs.
    let _ = vm.attach_current_thread_permanently();
}

fn open_devices() -> Result<(cpal::Stream, cpal::Stream, u32, u32), &'static str> {
    let host = cpal::default_host();
    let input_device = host.default_input_device().ok_or(NEEDS_MICROPHONE)?;
    let output_device = host.default_output_device().ok_or(DIDNT_CONNECT)?;
    let (input, input_rate) = open_input(&input_device)?;
    let (output, output_rate) = open_output(&output_device)?;
    Ok((input, output, input_rate, output_rate))
}

fn open_input(device: &cpal::Device) -> Result<(cpal::Stream, u32), &'static str> {
    let supported = preferred_config(device, true)?;
    let rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let config = supported.config();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _| push_i16(data, channels),
            ignore_stream_error,
            None,
        ),
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _| push_f32(data, channels),
            ignore_stream_error,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &config,
            move |data: &[u16], _| push_u16(data, channels),
            ignore_stream_error,
            None,
        ),
        _ => return Err(NEEDS_MICROPHONE),
    }
    .map_err(|_| NEEDS_MICROPHONE)?;
    stream.play().map_err(|_| NEEDS_MICROPHONE)?;
    Ok((stream, rate))
}

fn open_output(device: &cpal::Device) -> Result<(cpal::Stream, u32), &'static str> {
    let supported = preferred_config(device, false)?;
    let rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let config = supported.config();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::I16 => device.build_output_stream(
            &config,
            move |data: &mut [i16], _| fill_i16(data, channels),
            ignore_stream_error,
            None,
        ),
        cpal::SampleFormat::F32 => device.build_output_stream(
            &config,
            move |data: &mut [f32], _| fill_f32(data, channels),
            ignore_stream_error,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_output_stream(
            &config,
            move |data: &mut [u16], _| fill_u16(data, channels),
            ignore_stream_error,
            None,
        ),
        _ => return Err(DIDNT_CONNECT),
    }
    .map_err(|_| DIDNT_CONNECT)?;
    stream.play().map_err(|_| DIDNT_CONNECT)?;
    Ok((stream, rate))
}

fn preferred_config(
    device: &cpal::Device,
    input: bool,
) -> Result<cpal::SupportedStreamConfig, &'static str> {
    if input {
        let ranges = device
            .supported_input_configs()
            .map_err(|_| NEEDS_MICROPHONE)?;
        pick_rate(ranges, NEEDS_MICROPHONE)
    } else {
        let ranges = device
            .supported_output_configs()
            .map_err(|_| DIDNT_CONNECT)?;
        pick_rate(ranges, DIDNT_CONNECT)
    }
}

fn pick_rate(
    ranges: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    failure: &'static str,
) -> Result<cpal::SupportedStreamConfig, &'static str> {
    let mut fallback = None;
    for range in ranges {
        if range.channels() < 1 {
            continue;
        }
        let covers = range.min_sample_rate() <= cpal::SampleRate(RATE)
            && range.max_sample_rate() >= cpal::SampleRate(RATE);
        if covers {
            return Ok(range.with_sample_rate(cpal::SampleRate(RATE)));
        }
        if fallback.is_none() {
            fallback = Some(range.with_max_sample_rate());
        }
    }
    fallback.ok_or(failure)
}

fn ignore_stream_error(_: cpal::StreamError) {}

fn pump(
    epoch: u64,
    _input: cpal::Stream,
    _output: cpal::Stream,
    input_rate: u32,
    output_rate: u32,
    mut encoder: opus::Encoder,
    mut decoder: opus::Decoder,
) {
    let needed = samples_for_20ms(input_rate);
    let mut pending = Vec::<i16>::new();
    let mut seq = 0u16;
    let mut last_heard: Option<u16> = None;
    let mut encoded = [0u8; 4000];
    let mut decoded = [0i16; 2000];
    while LIVE.load(Ordering::Acquire) == epoch {
        {
            let mut capture = capture();
            pending.extend(capture.drain(..));
        }
        if pending.len() > needed.saturating_mul(8) {
            let extra = pending.len() - needed.saturating_mul(4);
            pending.drain(..extra);
        }
        let mut produced = false;
        while pending.len() >= needed && LIVE.load(Ordering::Acquire) == epoch {
            let frame: Vec<i16> = pending.drain(..needed).collect();
            let pcm = to_960(&frame, input_rate);
            if let Ok(size) = encoder.encode(&pcm, &mut encoded) {
                if size > 0 {
                    enqueue(seq, &encoded[..size]);
                    seq = seq.wrapping_add(1);
                    produced = true;
                }
            }
        }
        play_inbox(&mut decoder, output_rate, &mut last_heard, &mut decoded);
        if !produced {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn play_inbox(
    decoder: &mut opus::Decoder,
    output_rate: u32,
    last_heard: &mut Option<u16>,
    decoded: &mut [i16],
) {
    let frames: Vec<(u16, Vec<u8>)> = {
        let mut inbox = inbox();
        inbox.drain(..).collect()
    };
    for (seq, frame) in frames {
        if let Some(previous) = *last_heard {
            if seq.wrapping_sub(previous) == 2 {
                push_play(&to_output(&[0i16; FRAME], output_rate));
            }
        }
        *last_heard = Some(seq);
        if let Ok(samples) = decoder.decode(&frame, decoded, false) {
            push_play(&to_output(&decoded[..samples], output_rate));
        }
    }
}

fn highest_encoder() -> opus::Result<opus::Encoder> {
    let mut encoder = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Audio)?;
    encoder.set_bitrate(opus::Bitrate::Bits(fitting_bitrate()))?;
    encoder.set_complexity(10)?;
    encoder.set_vbr(true)?;
    encoder.set_bandwidth(opus::Bandwidth::Fullband)?;
    encoder.set_signal(opus::Signal::Music)?;
    Ok(encoder)
}

/// The highest rate whose 20 ms moment still fits in one link packet.
fn fitting_bitrate() -> i32 {
    let room = chunk_len() as i32;
    room.saturating_mul(8).saturating_mul(25)
}

fn enqueue(seq: u16, payload: &[u8]) {
    if payload.len() > chunk_len() {
        return;
    }
    let mut outbox = outbox();
    for packet in packets_for(seq, payload) {
        outbox.push_back(packet);
    }
    while outbox.len() > OUTBOX_LIMIT {
        outbox.pop_front();
    }
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
            packet.push(b'V');
            packet.push(AUDIO);
            packet.extend(seq.to_be_bytes());
            packet.push(part as u8);
            packet.push(count as u8);
            packet.extend_from_slice(chunk);
            packet
        })
        .collect()
}

fn push_part(assembly: &mut Assembly, packet: &[u8]) -> Option<(u16, Vec<u8>)> {
    if packet.len() < HEADER || packet[0] != b'V' || packet[1] != AUDIO {
        return None;
    }
    let seq = u16::from_be_bytes([packet[2], packet[3]]);
    let part = packet[4];
    let count = packet[5];
    if count == 0 || part >= count {
        return None;
    }
    if assembly.seq != Some(seq) || assembly.parts.len() != count as usize {
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

fn samples_for_20ms(rate: u32) -> usize {
    (u64::from(rate) * 20 / 1_000).max(1) as usize
}

fn to_960(frame: &[i16], rate: u32) -> [i16; FRAME] {
    let mut pcm = [0i16; FRAME];
    if frame.is_empty() {
        return pcm;
    }
    if rate == RATE && frame.len() >= FRAME {
        pcm.copy_from_slice(&frame[..FRAME]);
        return pcm;
    }
    resample(frame, &mut pcm);
    pcm
}

fn to_output(pcm: &[i16], output_rate: u32) -> Vec<i16> {
    if output_rate == RATE || pcm.is_empty() {
        return pcm.to_vec();
    }
    let out_len = ((pcm.len() as u64 * u64::from(output_rate)) / u64::from(RATE)).max(1) as usize;
    let mut out = vec![0i16; out_len];
    resample(pcm, &mut out);
    out
}

fn resample(input: &[i16], output: &mut [i16]) {
    let last = input.len() - 1;
    let steps = output.len().saturating_sub(1).max(1);
    for (index, sample) in output.iter_mut().enumerate() {
        let position = index as f64 * last as f64 / steps as f64;
        let left = (position.floor() as usize).min(last);
        let right = (left + 1).min(last);
        let frac = position - left as f64;
        let mixed =
            f64::from(input[left]) + (f64::from(input[right]) - f64::from(input[left])) * frac;
        *sample = mixed
            .round()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
    }
}

fn push_i16(samples: &[i16], channels: usize) {
    let channels = channels.max(1);
    let mut capture = capture();
    for frame in samples.chunks(channels) {
        capture.push_back(downmix_i16(frame));
    }
    trim_capture(&mut capture);
}

fn push_f32(samples: &[f32], channels: usize) {
    let channels = channels.max(1);
    let mut capture = capture();
    for frame in samples.chunks(channels) {
        let mixed = if frame.is_empty() {
            0.0
        } else {
            frame.iter().sum::<f32>() / frame.len() as f32
        };
        let scaled = mixed.clamp(-1.0, 1.0) * f32::from(i16::MAX);
        capture.push_back(scaled as i16);
    }
    trim_capture(&mut capture);
}

fn push_u16(samples: &[u16], channels: usize) {
    let channels = channels.max(1);
    let mut capture = capture();
    for frame in samples.chunks(channels) {
        let mixed = if frame.is_empty() {
            0u16
        } else {
            let sum: u32 = frame.iter().map(|sample| u32::from(*sample)).sum();
            (sum / frame.len() as u32) as u16
        };
        capture.push_back((i32::from(mixed) - 32_768) as i16);
    }
    trim_capture(&mut capture);
}

fn downmix_i16(frame: &[i16]) -> i16 {
    if frame.is_empty() {
        return 0;
    }
    let sum: i32 = frame.iter().map(|sample| i32::from(*sample)).sum();
    (sum / frame.len() as i32) as i16
}

fn trim_capture(capture: &mut VecDeque<i16>) {
    while capture.len() > RATE as usize {
        capture.pop_front();
    }
}

fn fill_i16(data: &mut [i16], channels: usize) {
    let channels = channels.max(1);
    let mut play = play_buf();
    for frame in data.chunks_mut(channels) {
        let sample = play.pop_front().unwrap_or(0);
        for slot in frame {
            *slot = sample;
        }
    }
}

fn fill_f32(data: &mut [f32], channels: usize) {
    let channels = channels.max(1);
    let mut play = play_buf();
    for frame in data.chunks_mut(channels) {
        let sample = play.pop_front().unwrap_or(0);
        let unit = f32::from(sample) / f32::from(i16::MAX);
        for slot in frame {
            *slot = unit;
        }
    }
}

fn fill_u16(data: &mut [u16], channels: usize) {
    let channels = channels.max(1);
    let mut play = play_buf();
    for frame in data.chunks_mut(channels) {
        let sample = play.pop_front().unwrap_or(0);
        let shifted = (i32::from(sample) + 32_768) as u16;
        for slot in frame {
            *slot = shifted;
        }
    }
}

fn push_play(samples: &[i16]) {
    let mut play = play_buf();
    play.extend(samples);
    while play.len() > PLAY_LIMIT {
        play.pop_front();
    }
}

fn clear_queues() {
    capture().clear();
    play_buf().clear();
    outbox().clear();
    inbox().clear();
    *assembly() = Assembly::default();
}

fn capture() -> MutexGuard<'static, VecDeque<i16>> {
    lock(&CAPTURE)
}

fn play_buf() -> MutexGuard<'static, VecDeque<i16>> {
    lock(&PLAY)
}

fn outbox() -> MutexGuard<'static, VecDeque<Vec<u8>>> {
    lock(&OUTBOX)
}

fn inbox() -> MutexGuard<'static, VecDeque<(u16, Vec<u8>)>> {
    lock(&INBOX)
}

fn assembly() -> MutexGuard<'static, Assembly> {
    lock(&ASSEMBLY)
}

fn lock<T: Default>(slot: &'static OnceLock<Mutex<T>>) -> MutexGuard<'static, T> {
    slot.get_or_init(|| Mutex::new(T::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loud_moment_uses_the_highest_opus_rate() {
        let mut encoder = highest_encoder().expect("encoder");
        assert_eq!(encoder.get_complexity().unwrap(), 10);
        assert_eq!(encoder.get_bandwidth().unwrap(), opus::Bandwidth::Fullband);
        assert_eq!(encoder.get_signal().unwrap(), opus::Signal::Music);
        let pcm = loud_noise();
        let mut out = [0u8; 4000];
        let size = encoder.encode(&pcm, &mut out).expect("encode");
        assert!(size <= chunk_len(), "encoded {size} bytes");
        assert_eq!(packets_for(1, &out[..size]).len(), 1);
        let mut decoder = opus::Decoder::new(RATE, opus::Channels::Mono).unwrap();
        let mut decoded = [0i16; 2000];
        let samples = decoder.decode(&out[..size], &mut decoded, false).unwrap();
        assert_eq!(samples, FRAME);
        let energy: i64 = decoded[..samples]
            .iter()
            .map(|sample| i64::from(i32::from(*sample).unsigned_abs()))
            .sum();
        assert!(energy > 1_000_000, "energy {energy}");
    }

    #[test]
    fn a_long_moment_splits_into_link_packets() {
        let payload = vec![7u8; 1000];
        let packets = packets_for(4, &payload);
        assert!(packets.len() > 1);
        let room = personal_rns::engine::MAX_SEND_TO_LINK_PLAINTEXT_LEN;
        let mut assembly = Assembly::default();
        let mut done = None;
        for packet in &packets {
            assert!(packet.len() <= room, "{}", packet.len());
            assert_eq!(kind(packet), VoiceKind::Audio);
            if let Some(frame) = push_part(&mut assembly, packet) {
                done = Some(frame);
            }
        }
        let (seq, frame) = done.expect("the pieces join");
        assert_eq!(seq, 4);
        assert_eq!(frame, payload);
        assert!(hello_packet().len() <= room);
        assert_eq!(kind(&hello_packet()), VoiceKind::Hello);
        assert_eq!(kind(&bye_packet()), VoiceKind::Bye);
    }

    fn loud_noise() -> [i16; FRAME] {
        let mut state = 0x1234_5678u32;
        let mut pcm = [0i16; FRAME];
        for sample in &mut pcm {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *sample = (state >> 16) as i16;
        }
        pcm
    }
}
