//! The laptop camera, open while a contact's code is being read.
//!
//! Windows Media Foundation supplies the frames. The picture on the page is
//! mirrored; the search uses the camera's own orientation. No camera, or a
//! camera Windows will not open, is reported so the page can ask for a photo instead.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{E_ACCESSDENIED, RPC_E_CHANGED_MODE};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaSource, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video, MFShutdown,
    MFStartup, MFVideoFormat_RGB32, MFSTARTUP_NOSOCKET, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READERF_ERROR,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

use crate::scan::contact_from_bgra;

const PREVIEW_EDGE: u32 = 640;

static RUN: AtomicU64 = AtomicU64::new(0);
static CURRENT: AtomicU64 = AtomicU64::new(0);
static STOP: AtomicBool = AtomicBool::new(false);
static HOLD: Mutex<()> = Mutex::new(());
static PREVIEW: Mutex<Option<String>> = Mutex::new(None);
static DONE: Mutex<Option<Done>> = Mutex::new(None);

enum Done {
    Code(String),
    Denied,
    Unavailable,
}

/// What the page should show while the camera is open.
pub enum View {
    Starting,
    Picture(String),
    Code(String),
    Denied,
    Unavailable,
}

/// Opens the camera. The returned token stops matching once the camera closes.
pub fn start() -> u64 {
    STOP.store(false, Ordering::Release);
    let run = RUN.fetch_add(1, Ordering::AcqRel) + 1;
    CURRENT.store(run, Ordering::Release);
    *lock(&PREVIEW) = None;
    *lock(&DONE) = None;
    std::thread::Builder::new()
        .name("journal-camera".into())
        .spawn(move || run_camera(run))
        .ok();
    run
}

/// Closes the camera, including when the page goes away.
pub fn stop() {
    STOP.store(true, Ordering::Release);
    CURRENT.store(0, Ordering::Release);
    RUN.fetch_add(1, Ordering::AcqRel);
}

pub fn token() -> u64 {
    RUN.load(Ordering::Acquire)
}

pub fn look() -> View {
    if let Some(done) = lock(&DONE).take() {
        return match done {
            Done::Code(text) => View::Code(text),
            Done::Denied => View::Denied,
            Done::Unavailable => View::Unavailable,
        };
    }
    if let Some(picture) = lock(&PREVIEW).clone() {
        View::Picture(picture)
    } else {
        View::Starting
    }
}

struct Opened {
    source: Option<IMFMediaSource>,
    media: bool,
    com: bool,
}

impl Drop for Opened {
    fn drop(&mut self) {
        // SAFETY: shutdown is the documented end of a Media Foundation capture.
        // Each flag is set only after the matching start succeeded on this thread.
        unsafe {
            if let Some(source) = self.source.take() {
                let _ = source.Shutdown();
            }
            if self.media {
                let _ = MFShutdown();
            }
            if self.com {
                CoUninitialize();
            }
        }
    }
}

fn run_camera(run: u64) {
    let _hold = HOLD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if CURRENT.load(Ordering::Acquire) != run {
        return;
    }
    let mut opened = Opened {
        source: None,
        media: false,
        com: false,
    };
    // SAFETY: COM and Media Foundation are initialized on this camera thread,
    // one video device is activated, and `Opened` shuts them down in reverse.
    let open = open_camera(&mut opened);
    let (reader, width, height, stride) = match open {
        Ok(ready) => ready,
        Err(error) => {
            finish(run, failed(error));
            return;
        }
    };
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    while CURRENT.load(Ordering::Acquire) == run && !STOP.load(Ordering::Acquire) {
        let mut flags = 0u32;
        let mut sample = None;
        // SAFETY: the reader is the one `open_camera` created and is still alive.
        let read = unsafe {
            reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
        };
        if let Err(error) = read {
            finish(run, failed(error));
            return;
        }
        if flags & (MF_SOURCE_READERF_ERROR.0 as u32) != 0 {
            finish(run, Done::Unavailable);
            return;
        }
        let Some(sample) = sample else {
            continue;
        };
        // SAFETY: the sample is the buffer Media Foundation just returned.
        if let Some(bgra) = copy_frame(&sample, width, height, stride) {
            if preview_due() {
                if let Some(picture) = preview_url(&bgra, width, height) {
                    *lock(&PREVIEW) = Some(picture);
                }
            }
            if let Some((name, identity)) = contact_from_bgra(&bgra, width, height, width * 4) {
                *lock(&DONE) = Some(Done::Code(format!("{name}\n{identity}")));
                STOP.store(true, Ordering::Release);
            }
        }
    }
}

fn open_camera(
    opened: &mut Opened,
) -> windows::core::Result<(
    windows::Win32::Media::MediaFoundation::IMFSourceReader,
    u32,
    u32,
    i32,
)> {
    // SAFETY: this thread owns the COM apartment and the Media Foundation session.
    // `Opened` releases both, and the device array is freed before we return.
    unsafe {
        let com = CoInitializeEx(None, COINIT_MULTITHREADED);
        if com.is_ok() {
            opened.com = true;
        } else if com != RPC_E_CHANGED_MODE {
            return Err(com.into());
        }
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;
        opened.media = true;

        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes = attributes.ok_or_else(windows::core::Error::from_win32)?;
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
        let mut listed: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attributes, &mut listed, &mut count)?;
        if count == 0 || listed.is_null() {
            if !listed.is_null() {
                CoTaskMemFree(Some(listed.cast()));
            }
            return Err(windows::core::Error::empty());
        }
        let activate = {
            let devices = std::slice::from_raw_parts_mut(listed, count as usize);
            let first = devices[0].take();
            for device in devices.iter_mut().skip(1) {
                let _ = device.take();
            }
            first
        };
        CoTaskMemFree(Some(listed.cast()));
        let activate: IMFActivate = activate.ok_or_else(windows::core::Error::from_win32)?;
        let source: IMFMediaSource = activate.ActivateObject()?;
        opened.source = Some(source.clone());

        let mut reader_attributes = None;
        MFCreateAttributes(&mut reader_attributes, 1)?;
        let reader_attributes = reader_attributes.ok_or_else(windows::core::Error::from_win32)?;
        reader_attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        let reader = MFCreateSourceReaderFromMediaSource(&source, &reader_attributes)?;
        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        reader.SetCurrentMediaType(stream, None, &output)?;
        let kind = reader.GetCurrentMediaType(stream)?;
        let packed = kind.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let width = (packed >> 32) as u32;
        let height = (packed & 0xFFFF_FFFF) as u32;
        if width == 0 || height == 0 {
            return Err(windows::core::Error::empty());
        }
        let stride = kind
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .unwrap_or(width.saturating_mul(4)) as i32;
        let stride = if stride == 0 {
            width.saturating_mul(4) as i32
        } else {
            stride
        };
        Ok((reader, width, height, stride))
    }
}

fn copy_frame(
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
    width: u32,
    height: u32,
    stride: i32,
) -> Option<Vec<u8>> {
    // SAFETY: `sample` is the buffer from the current ReadSample, locked only until Unlock.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().ok()?;
        let mut data = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut data, None, Some(&mut len)).ok()?;
        let frame = copy_top_down(data, len, width, height, stride);
        let _ = buffer.Unlock();
        frame
    }
}

fn copy_top_down(data: *mut u8, len: u32, width: u32, height: u32, stride: i32) -> Option<Vec<u8>> {
    let row = stride.unsigned_abs() as usize;
    let tight = (width as usize).checked_mul(4)?;
    if row < tight || data.is_null() {
        return None;
    }
    let bytes = usize::try_from(len).ok()?;
    let need = row.checked_mul(height as usize)?;
    if bytes < need || u64::from(width) * u64::from(height) > 16_000_000 {
        return None;
    }
    // SAFETY: Media Foundation's lock covers `len` bytes at `data` until Unlock.
    let src = unsafe { std::slice::from_raw_parts(data, bytes) };
    let mut out = vec![0u8; tight * height as usize];
    for y in 0..height as usize {
        let from_row = if stride >= 0 {
            y
        } else {
            height as usize - 1 - y
        };
        let from = from_row * row;
        let to = y * tight;
        out[to..to + tight].copy_from_slice(src.get(from..from + tight)?);
    }
    Some(out)
}

fn failed(error: windows::core::Error) -> Done {
    let code = error.code();
    // 0x800704EC is ERROR_ACCESS_DISABLED_BY_POLICY, the camera privacy switch.
    if code == E_ACCESSDENIED || code == windows::core::HRESULT(0x800704EC_u32 as i32) {
        Done::Denied
    } else {
        Done::Unavailable
    }
}

fn preview_due() -> bool {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = lock(&LAST);
    let now = Instant::now();
    if last.is_some_and(|then| now.duration_since(then) < Duration::from_millis(120)) {
        return false;
    }
    *last = Some(now);
    true
}

fn preview_url(bgra: &[u8], width: u32, height: u32) -> Option<String> {
    let (out_w, out_h, shrunk) = shrink_bgra(bgra, width, height, PREVIEW_EDGE)?;
    let mut rgb = Vec::with_capacity((out_w as usize) * (out_h as usize) * 3);
    for pixel in shrunk.chunks_exact(4) {
        rgb.push(pixel[2]);
        rgb.push(pixel[1]);
        rgb.push(pixel[0]);
    }
    let mut jpeg = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 55);
    image::ImageEncoder::write_image(encoder, &rgb, out_w, out_h, image::ExtendedColorType::Rgb8)
        .ok()?;
    Some(format!("data:image/jpeg;base64,{}", base64(&jpeg)))
}

fn shrink_bgra(src: &[u8], width: u32, height: u32, edge: u32) -> Option<(u32, u32, Vec<u8>)> {
    let longest = width.max(height);
    let (out_w, out_h) = if longest <= edge {
        (width, height)
    } else {
        (
            ((u64::from(width) * u64::from(edge) / u64::from(longest)).max(1) as u32),
            ((u64::from(height) * u64::from(edge) / u64::from(longest)).max(1) as u32),
        )
    };
    let stride = width as usize * 4;
    let mut out = vec![0u8; (out_w as usize) * (out_h as usize) * 4];
    for y in 0..out_h {
        let sy = (u64::from(y) * u64::from(height) / u64::from(out_h)) as usize;
        for x in 0..out_w {
            let sx = (u64::from(x) * u64::from(width) / u64::from(out_w)) as usize;
            let from = sy * stride + sx * 4;
            let to = (y as usize * out_w as usize + x as usize) * 4;
            out[to..to + 4].copy_from_slice(src.get(from..from + 4)?);
        }
    }
    Some((out_w, out_h, out))
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

fn finish(run: u64, done: Done) {
    if CURRENT.load(Ordering::Acquire) == run {
        *lock(&DONE) = Some(done);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
