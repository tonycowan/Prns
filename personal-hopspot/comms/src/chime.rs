//! Two sounds from one G7#11 chord, G B D F C#.
//! A note arrives as the chord. An incoming call arpeggiates the same notes.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

static CHIME: AtomicBool = AtomicBool::new(false);
static RINGING: AtomicBool = AtomicBool::new(false);
static GEN: AtomicU64 = AtomicU64::new(0);

/// G3, D4, F4, B4, C#5, with a gain and a delay in seconds.
const TONES: [(u8, f32, f32); 5] = [
    (55, 0.20, 0.000),
    (62, 0.12, 0.020),
    (65, 0.13, 0.040),
    (71, 0.14, 0.060),
    (73, 0.11, 0.080),
];

const ARP: [(u8, f32, f32); 5] = [
    (55, 0.34, 0.00),
    (62, 0.30, 0.28),
    (65, 0.30, 0.56),
    (71, 0.32, 0.84),
    (73, 0.30, 1.12),
];

pub fn arrived() {
    if RINGING.load(Ordering::Acquire) || CHIME.swap(true, Ordering::AcqRel) {
        return;
    }
    let generation = GEN.fetch_add(1, Ordering::AcqRel) + 1;
    let started = std::thread::Builder::new()
        .name("touchbase-chime".into())
        .spawn(move || {
            let _release = ReleaseChime;
            let _ = play_once(render, generation, Duration::from_secs(3));
        });
    if started.is_err() {
        CHIME.store(false, Ordering::Release);
    }
}

/// Repeats the arpeggio while a call is ringing, and stops when `on` is false.
pub fn ring(on: bool) {
    if on {
        if RINGING.swap(true, Ordering::AcqRel) {
            return;
        }
        let generation = GEN.fetch_add(1, Ordering::AcqRel) + 1;
        let started = std::thread::Builder::new()
            .name("touchbase-ring".into())
            .spawn(move || ring_until_stopped(generation));
        if started.is_err() {
            RINGING.store(false, Ordering::Release);
        }
        return;
    }
    if RINGING.swap(false, Ordering::AcqRel) {
        GEN.fetch_add(1, Ordering::AcqRel);
    }
}

struct ReleaseChime;

impl Drop for ReleaseChime {
    fn drop(&mut self) {
        CHIME.store(false, Ordering::Release);
    }
}

fn ring_until_stopped(generation: u64) {
    let _ = sustain(generation);
}

fn hz(note: u8) -> f64 {
    440.0 * 2.0_f64.powf((f64::from(note) - 69.0) / 12.0)
}

fn render(rate: u32) -> Vec<f32> {
    mix(rate, &TONES, 0.012, 0.34, 1.2)
}

fn arpeggio(rate: u32) -> Vec<f32> {
    mix(rate, &ARP, 0.008, 0.18, 2.35)
}

pub fn ringback_samples(rate: u32) -> Vec<i16> {
    arpeggio(rate)
        .into_iter()
        .map(|sample| (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)
        .collect()
}

fn mix(rate: u32, tones: &[(u8, f32, f32)], attack: f64, decay: f64, seconds: f64) -> Vec<f32> {
    let rate = f64::from(rate.max(1));
    let count = (rate * seconds) as usize;
    let mut samples = vec![0.0; count];
    for &(note, gain, delay) in tones {
        let frequency = hz(note);
        let skip = (rate * f64::from(delay)) as usize;
        for (index, sample) in samples.iter_mut().enumerate().skip(skip) {
            let t = (index - skip) as f64 / rate;
            let rise = (t / attack).min(1.0);
            let envelope = rise * (-t / decay).exp();
            let wave = (std::f64::consts::TAU * frequency * t).sin();
            *sample += (f64::from(gain) * envelope * wave) as f32;
        }
    }
    samples
}

fn still(generation: u64) -> bool {
    GEN.load(Ordering::Acquire) == generation
}

fn play_once(rendered: fn(u32) -> Vec<f32>, generation: u64, limit: Duration) -> Result<(), ()> {
    hear(rendered, generation, false, limit)
}

fn sustain(generation: u64) -> Result<(), ()> {
    hear(arpeggio, generation, true, Duration::from_secs(2))
}

fn hear(
    rendered: fn(u32) -> Vec<f32>,
    generation: u64,
    repeating: bool,
    limit: Duration,
) -> Result<(), ()> {
    #[cfg(target_os = "android")]
    attach_android_audio();
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(())?;
    let supported = device.default_output_config().map_err(|_| ())?;
    let channels = supported.channels() as usize;
    if channels == 0 {
        return Err(());
    }
    let samples = rendered(supported.sample_rate().0);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let mut cursor = Cursor {
        samples,
        frame: 0,
        channels,
        repeating,
        generation,
        done: Some(done_tx),
    };
    let config = supported.config();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _| cursor.write(data, |sample| sample),
                ignore,
                None,
            )
            .map_err(|_| ())?,
        cpal::SampleFormat::I16 => device
            .build_output_stream(
                &config,
                move |data: &mut [i16], _| {
                    cursor.write(data, |sample| {
                        (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
                    });
                },
                ignore,
                None,
            )
            .map_err(|_| ())?,
        cpal::SampleFormat::U16 => device
            .build_output_stream(
                &config,
                move |data: &mut [u16], _| {
                    cursor.write(data, |sample| {
                        ((sample.clamp(-1.0, 1.0) * 0.5 + 0.5) * f32::from(u16::MAX)) as u16
                    });
                },
                ignore,
                None,
            )
            .map_err(|_| ())?,
        _ => return Err(()),
    };
    stream.play().map_err(|_| ())?;
    loop {
        match done_rx.recv_timeout(limit) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) if !repeating || !still(generation) => break,
            Err(RecvTimeoutError::Timeout) if !RINGING.load(Ordering::Acquire) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
    Ok(())
}

struct Cursor {
    samples: Vec<f32>,
    frame: usize,
    channels: usize,
    repeating: bool,
    generation: u64,
    done: Option<mpsc::SyncSender<()>>,
}

impl Cursor {
    fn write<T>(&mut self, data: &mut [T], convert: impl Fn(f32) -> T) {
        let stop = !still(self.generation) || (self.repeating && !RINGING.load(Ordering::Acquire));
        for slot in data.chunks_mut(self.channels) {
            let sample = if stop {
                0.0
            } else if self.repeating {
                self.samples[self.frame % self.samples.len()]
            } else {
                self.samples.get(self.frame).copied().unwrap_or(0.0)
            };
            for out in slot.iter_mut() {
                *out = convert(sample);
            }
            self.frame += 1;
        }
        let finished = stop || (!self.repeating && self.frame >= self.samples.len());
        if finished {
            if let Some(done) = self.done.take() {
                let _ = done.send(());
            }
        }
    }
}

fn ignore(_: cpal::StreamError) {}

#[cfg(target_os = "android")]
fn attach_android_audio() {
    let ctx = ndk_context::android_context();
    // SAFETY: ndk-context publishes the process JavaVM for the life of the activity.
    // `JavaVM::from_raw` only wraps that pointer and does not destroy the VM on drop.
    let Ok(vm) = (unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }) else {
        return;
    };
    let _ = vm.attach_current_thread_permanently();
}

#[cfg(test)]
mod tests {
    use super::{arpeggio, hz, render, ARP, TONES};

    #[test]
    fn the_arrival_chord_is_g7_sharp_11() {
        let classes: Vec<u8> = TONES.iter().map(|(note, _, _)| note % 12).collect();
        // G, D, F, B, C#.
        assert_eq!(classes, vec![7, 2, 5, 11, 1]);
        assert!((hz(55) - 195.9977).abs() < 0.01);
        assert!((hz(73) - 554.3653).abs() < 0.01);
        assert!(hz(73) - hz(72) > 20.0);
    }

    #[test]
    fn the_chord_is_audible_and_does_not_clip() {
        let samples = render(48_000);
        let peak = peak_of(&samples);
        assert!(peak > 0.15, "{peak}");
        assert!(peak < 1.0, "{peak}");
        assert!(samples.len() > 48_000);
    }

    #[test]
    fn the_ring_arpeggiates_the_same_chord() {
        let classes: Vec<u8> = ARP.iter().map(|(note, _, _)| note % 12).collect();
        assert_eq!(
            classes,
            TONES
                .iter()
                .map(|(note, _, _)| note % 12)
                .collect::<Vec<_>>()
        );
        let gaps: Vec<f32> = ARP.windows(2).map(|pair| pair[1].2 - pair[0].2).collect();
        assert!(gaps.iter().all(|gap| *gap > 0.2));
        let samples = arpeggio(48_000);
        let early = (0.030 * 48_000.0) as usize;
        let t = early as f64 / 48_000.0;
        let expected = 0.34
            * (t / 0.008).min(1.0)
            * (-t / 0.18).exp()
            * (std::f64::consts::TAU * hz(55) * t).sin();
        assert!((f64::from(samples[early]) - expected).abs() < 0.02);
        assert!(samples.len() as f64 / 48_000.0 > 2.0);
        let peak = peak_of(&samples);
        assert!(peak > 0.15, "{peak}");
        assert!(peak < 1.0, "{peak}");
    }

    fn peak_of(samples: &[f32]) -> f32 {
        samples
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()))
    }
}
