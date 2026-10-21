//! The camera, open while a contact's code is being read.
//!
//! Frames stay small enough to search as they arrive. The picture shown on the
//! page is mirrored; the search uses the camera's own orientation, which is
//! what a code needs.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use image::GrayImage;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput,
    AVCaptureInput, AVCaptureOutput, AVCaptureSession, AVCaptureSessionPreset1280x720,
    AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
#[cfg(target_os = "ios")]
#[allow(deprecated)]
use objc2_av_foundation::{
    AVCaptureDevicePosition, AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureVideoOrientation,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, kCVReturnSuccess, CVPixelBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane,
    CVPixelBufferIsPlanar, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSDictionary, NSNumber, NSObject, NSString};

use crate::scan::contact_from_luma;

const SCAN_EDGE: u32 = 1280;
const PREVIEW_EDGE: u32 = 640;

static RUN: AtomicU64 = AtomicU64::new(0);
static CURRENT: AtomicU64 = AtomicU64::new(0);
static STOP: AtomicBool = AtomicBool::new(false);
static BUSY: AtomicBool = AtomicBool::new(false);
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

struct FinderIvars {
    run: u64,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AllocAnyThread]
    #[name = "JournalCodeFinder"]
    #[ivars = FinderIvars]
    struct CodeFinder;

    unsafe impl NSObjectProtocol for CodeFinder {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CodeFinder {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        #[allow(non_snake_case)]
        fn captureOutput_didOutputSampleBuffer_fromConnection(
            &self,
            _output: &AVCaptureOutput,
            sample_buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            if self.ivars().run != CURRENT.load(Ordering::Acquire) {
                return;
            }
            on_frame(sample_buffer);
        }
    }
);

impl CodeFinder {
    fn new(run: u64) -> Retained<Self> {
        let this = Self::alloc().set_ivars(FinderIvars { run });
        // SAFETY: `init` is NSObject's initializer, called on the object we just allocated.
        unsafe { msg_send![super(this), init] }
    }
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

fn run_camera(run: u64) {
    let _hold = HOLD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if CURRENT.load(Ordering::Acquire) != run {
        return;
    }
    // SAFETY: AVMediaTypeVideo is a process-wide media-type constant.
    let Some(media) = (unsafe { AVMediaTypeVideo }) else {
        finish(run, Done::Unavailable);
        return;
    };
    if !allowed(media, run) {
        return;
    }
    if CURRENT.load(Ordering::Acquire) != run {
        return;
    }
    // SAFETY: the session, input, and output are used as AVFoundation documents:
    // configure them, start on this background thread, and stop before they are dropped.
    // The frame receiver and its queue stay alive until the callback is cleared.
    let opened = unsafe { open_session(run, media) };
    let Some(opened) = opened else {
        finish(run, Done::Unavailable);
        return;
    };
    while CURRENT.load(Ordering::Acquire) == run && !STOP.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(40));
    }
    unsafe {
        opened.session.stopRunning();
        opened.output.setSampleBufferDelegate_queue(None, None);
    }
}

fn allowed(media: &objc2_av_foundation::AVMediaType, run: u64) -> bool {
    // SAFETY: reading the process-wide camera authorization for the video media type.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) };
    if status == AVAuthorizationStatus::Authorized {
        return true;
    }
    if status != AVAuthorizationStatus::NotDetermined {
        finish(run, Done::Denied);
        return false;
    }
    let (tx, rx) = mpsc::channel();
    let block = block2::RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });
    // SAFETY: requests the system camera prompt. The handler only sends a bool.
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &block) };
    let granted = rx.recv().unwrap_or(false);
    if CURRENT.load(Ordering::Acquire) != run {
        return false;
    }
    if !granted {
        finish(run, Done::Denied);
        return false;
    }
    true
}

struct Opened {
    session: Retained<AVCaptureSession>,
    output: Retained<AVCaptureVideoDataOutput>,
    /// AVFoundation does not keep this alive on its own. Frames stop if it is dropped.
    _delegate: Retained<CodeFinder>,
    _queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
}

/// SAFETY: `media` is the video type. The returned session is running and must be stopped
/// by the caller before it is dropped. The delegate must be kept until the callback is cleared.
unsafe fn open_session(run: u64, media: &objc2_av_foundation::AVMediaType) -> Option<Opened> {
    let device = video_device(media)?;
    // SAFETY: opens that device for capture. An error means it is unavailable.
    let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }.ok()?;
    let output = unsafe { AVCaptureVideoDataOutput::new() };
    let format = NSNumber::numberWithUnsignedInt(kCVPixelFormatType_32BGRA);
    let key = NSString::from_str("PixelFormatType");
    let settings = NSDictionary::<NSString, AnyObject>::from_slices(&[&*key], &[format.as_ref()]);
    let delegate = CodeFinder::new(run);
    let queue = dispatch2::DispatchQueue::new("journal.camera.frames", None);
    // SAFETY: the preset, settings, and delegate are the ones just built for this session.
    unsafe {
        output.setAlwaysDiscardsLateVideoFrames(true);
        output.setVideoSettings(Some(&settings));
        output.setSampleBufferDelegate_queue(
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&queue),
        );
    }
    let session = unsafe { AVCaptureSession::new() };
    unsafe {
        session.beginConfiguration();
        // SAFETY: the preset constant is process-wide, and this block is already unsafe.
        let preset = AVCaptureSessionPreset1280x720;
        if session.canSetSessionPreset(preset) {
            session.setSessionPreset(preset);
        }
        let input_ref: &AVCaptureInput = &input;
        if !session.canAddInput(input_ref) {
            session.commitConfiguration();
            return None;
        }
        session.addInput(input_ref);
        let output_ref: &AVCaptureOutput = &output;
        if !session.canAddOutput(output_ref) {
            session.commitConfiguration();
            return None;
        }
        session.addOutput(output_ref);
        #[cfg(target_os = "ios")]
        if let Some(connection) = output.connectionWithMediaType(media) {
            // The phone is held upright. Turn the frames to match, so the code is not on its side.
            #[allow(deprecated)]
            if connection.isVideoOrientationSupported() {
                connection.setVideoOrientation(AVCaptureVideoOrientation::Portrait);
            }
        }
        session.commitConfiguration();
        session.startRunning();
    }
    Some(Opened {
        session,
        output,
        _delegate: delegate,
        _queue: queue,
    })
}

fn video_device(media: &objc2_av_foundation::AVMediaType) -> Option<Retained<AVCaptureDevice>> {
    #[cfg(target_os = "ios")]
    {
        // SAFETY: the wide-angle type is a process-wide constant. The back camera
        // photographs a code held up to the phone.
        let device_type = unsafe { AVCaptureDeviceTypeBuiltInWideAngleCamera };
        if let Some(device) = unsafe {
            AVCaptureDevice::defaultDeviceWithDeviceType_mediaType_position(
                device_type,
                Some(media),
                AVCaptureDevicePosition::Back,
            )
        } {
            return Some(device);
        }
    }
    // SAFETY: default video device for this process. Nil means there is no camera.
    unsafe { AVCaptureDevice::defaultDeviceWithMediaType(media) }
}

fn on_frame(sample: &CMSampleBuffer) {
    if STOP.load(Ordering::Acquire) || BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Some(seen) = grab_frame(sample) {
        if preview_due() {
            if let Some(picture) = jpeg_url(&seen.preview, seen.preview_width, seen.preview_height)
            {
                *lock(&PREVIEW) = Some(picture);
            }
        }
        if let Some((name, identity)) = contact_from_luma(&seen.luma) {
            *lock(&DONE) = Some(Done::Code(format!("{name}\n{identity}")));
            STOP.store(true, Ordering::Release);
        }
    }
    BUSY.store(false, Ordering::Release);
}

struct Seen {
    luma: GrayImage,
    preview: Vec<u8>,
    preview_width: u32,
    preview_height: u32,
}

fn grab_frame(sample: &CMSampleBuffer) -> Option<Seen> {
    // SAFETY: this is the sample AVFoundation passed into the current callback.
    let image = unsafe { sample.image_buffer() }?;
    let pixel: &CVPixelBuffer = &image;
    // SAFETY: the buffer stays locked until `Unlock` drops, and the copy finishes first.
    if unsafe { CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly) }
        != kCVReturnSuccess
    {
        return None;
    }
    let _unlock = Unlock(pixel);
    let copied = if CVPixelBufferGetPixelFormatType(pixel) == kCVPixelFormatType_32BGRA {
        copy_bgra(pixel)
    } else if CVPixelBufferIsPlanar(pixel) {
        copy_plane(pixel)
    } else {
        None
    }?;
    drop(_unlock);
    match copied {
        Copied::Color {
            bytes,
            width,
            height,
            stride,
        } => {
            let (scan_w, scan_h, scan) = shrink_bgra(&bytes, width, height, stride, SCAN_EDGE)?;
            let (preview_width, preview_height, preview) =
                shrink_bgra(&bytes, width, height, stride, PREVIEW_EDGE)?;
            Some(Seen {
                luma: gray_from_bgra(&scan, scan_w, scan_h)?,
                preview,
                preview_width,
                preview_height,
            })
        }
        Copied::Luma {
            bytes,
            width,
            height,
            stride,
        } => {
            let (scan_w, scan_h, scan) = shrink_plane(&bytes, width, height, stride, SCAN_EDGE)?;
            let (preview_width, preview_height, plane) =
                shrink_plane(&bytes, width, height, stride, PREVIEW_EDGE)?;
            Some(Seen {
                luma: GrayImage::from_raw(scan_w, scan_h, scan)?,
                preview: gray_as_bgra(&plane),
                preview_width,
                preview_height,
            })
        }
    }
}

enum Copied {
    Color {
        bytes: Vec<u8>,
        width: u32,
        height: u32,
        stride: u32,
    },
    Luma {
        bytes: Vec<u8>,
        width: u32,
        height: u32,
        stride: u32,
    },
}

fn copy_bgra(pixel: &CVPixelBuffer) -> Option<Copied> {
    let width = u32::try_from(CVPixelBufferGetWidth(pixel)).ok()?;
    let height = u32::try_from(CVPixelBufferGetHeight(pixel)).ok()?;
    let stride = u32::try_from(CVPixelBufferGetBytesPerRow(pixel)).ok()?;
    let bytes = copy_locked(CVPixelBufferGetBaseAddress(pixel), width, height, stride, 4)?;
    Some(Copied::Color {
        bytes,
        width,
        height,
        stride,
    })
}

fn copy_plane(pixel: &CVPixelBuffer) -> Option<Copied> {
    let width = u32::try_from(CVPixelBufferGetWidthOfPlane(pixel, 0)).ok()?;
    let height = u32::try_from(CVPixelBufferGetHeightOfPlane(pixel, 0)).ok()?;
    let stride = u32::try_from(CVPixelBufferGetBytesPerRowOfPlane(pixel, 0)).ok()?;
    let bytes = copy_locked(
        CVPixelBufferGetBaseAddressOfPlane(pixel, 0),
        width,
        height,
        stride,
        1,
    )?;
    Some(Copied::Luma {
        bytes,
        width,
        height,
        stride,
    })
}

fn copy_locked(
    base: *mut std::ffi::c_void,
    width: u32,
    height: u32,
    stride: u32,
    bytes_per_pixel: u32,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || stride < width.saturating_mul(bytes_per_pixel) {
        return None;
    }
    if u64::from(width) * u64::from(height) > 8_000_000 {
        return None;
    }
    if base.is_null() {
        return None;
    }
    let len = usize::try_from(stride).ok()?.checked_mul(height as usize)?;
    // SAFETY: the base address is locked by the caller, and `len` is the row stride times the height.
    let src = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), len) };
    Some(src.to_vec())
}

struct Unlock<'a>(&'a CVPixelBuffer);

impl Drop for Unlock<'_> {
    fn drop(&mut self) {
        // SAFETY: this lock was taken with the same read-only flag in `copy_bgra`.
        unsafe {
            CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly);
        }
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

/// Searches a BGRA frame for a contact code. `stride` is bytes per row.
#[cfg(test)]
fn read_bgra(bytes: &[u8], width: u32, height: u32, stride: u32) -> Option<(String, String)> {
    let (width, height, tight) = shrink_bgra(bytes, width, height, stride, SCAN_EDGE)?;
    let image = gray_from_bgra(&tight, width, height)?;
    contact_from_luma(&image)
}

fn gray_from_bgra(bgra: &[u8], width: u32, height: u32) -> Option<GrayImage> {
    let mut luma = Vec::with_capacity((width as usize) * (height as usize));
    for pixel in bgra.chunks_exact(4) {
        let blue = u32::from(pixel[0]);
        let green = u32::from(pixel[1]);
        let red = u32::from(pixel[2]);
        luma.push(((red * 77 + green * 150 + blue * 29) >> 8) as u8);
    }
    GrayImage::from_raw(width, height, luma)
}

fn gray_as_bgra(luma: &[u8]) -> Vec<u8> {
    let mut bgra = Vec::with_capacity(luma.len() * 4);
    for value in luma {
        bgra.extend_from_slice(&[*value, *value, *value, 255]);
    }
    bgra
}

fn shrink_plane(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    edge: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    if width == 0 || height == 0 || stride < width {
        return None;
    }
    let (out_w, out_h) = fit(width, height, edge);
    let mut out = vec![0u8; (out_w as usize) * (out_h as usize)];
    for y in 0..out_h {
        let sy = (u64::from(y) * u64::from(height) / u64::from(out_h)) as usize;
        for x in 0..out_w {
            let sx = (u64::from(x) * u64::from(width) / u64::from(out_w)) as usize;
            let from = sy * stride as usize + sx;
            out[y as usize * out_w as usize + x as usize] = *src.get(from)?;
        }
    }
    Some((out_w, out_h, out))
}

fn fit(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= edge {
        (width, height)
    } else {
        (
            ((u64::from(width) * u64::from(edge) / u64::from(longest)).max(1) as u32),
            ((u64::from(height) * u64::from(edge) / u64::from(longest)).max(1) as u32),
        )
    }
}

fn shrink_bgra(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    edge: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    if width == 0 || height == 0 || stride < width.saturating_mul(4) {
        return None;
    }
    let (out_w, out_h) = fit(width, height, edge);
    let mut out = vec![0u8; (out_w as usize) * (out_h as usize) * 4];
    for y in 0..out_h {
        let sy = (u64::from(y) * u64::from(height) / u64::from(out_h)) as usize;
        for x in 0..out_w {
            let sx = (u64::from(x) * u64::from(width) / u64::from(out_w)) as usize;
            let from = sy * stride as usize + sx * 4;
            let pixel = src.get(from..from + 4)?;
            let to = (y as usize * out_w as usize + x as usize) * 4;
            out[to..to + 4].copy_from_slice(pixel);
        }
    }
    Some((out_w, out_h, out))
}

fn jpeg_url(bgra: &[u8], width: u32, height: u32) -> Option<String> {
    let mut rgb = Vec::with_capacity(bgra.len() / 4 * 3);
    for pixel in bgra.chunks_exact(4) {
        rgb.push(pixel[2]);
        rgb.push(pixel[1]);
        rgb.push(pixel[0]);
    }
    let mut jpeg = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 55);
    image::ImageEncoder::write_image(encoder, &rgb, width, height, image::ExtendedColorType::Rgb8)
        .ok()?;
    Some(format!("data:image/jpeg;base64,{}", base64(&jpeg)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_in_a_camera_frame_is_read() {
        let (width, height, bgra) = paint("Ada\n0123abcd");
        let (name, identity) = read_bgra(&bgra, width, height, width * 4).expect("code");
        assert_eq!(name, "Ada");
        assert_eq!(identity, "0123abcd");
    }

    #[test]
    fn a_padded_row_still_reads() {
        let (width, height, bgra) = paint("Ada\n0123abcd");
        let stride = width * 4 + 16;
        let mut padded = vec![0u8; stride as usize * height as usize];
        for y in 0..height as usize {
            let from = y * width as usize * 4;
            let to = y * stride as usize;
            padded[to..to + width as usize * 4]
                .copy_from_slice(&bgra[from..from + width as usize * 4]);
        }
        let (name, _) = read_bgra(&padded, width, height, stride).expect("code");
        assert_eq!(name, "Ada");
    }

    fn paint(payload: &str) -> (u32, u32, Vec<u8>) {
        use image::{ImageBuffer, Luma};
        use qrcode::types::Color;
        use qrcode::QrCode;
        let code = QrCode::new(payload.as_bytes()).unwrap();
        let modules = code.width();
        let scale = 8u32;
        let quiet = 4 * scale;
        let size = modules as u32 * scale + quiet * 2;
        let mut gray: ImageBuffer<Luma<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(size, size, Luma([255]));
        for y in 0..modules {
            for x in 0..modules {
                if code[(x, y)] != Color::Dark {
                    continue;
                }
                let origin_x = quiet + x as u32 * scale;
                let origin_y = quiet + y as u32 * scale;
                for dy in 0..scale {
                    for dx in 0..scale {
                        gray.put_pixel(origin_x + dx, origin_y + dy, Luma([0]));
                    }
                }
            }
        }
        let mut bgra = Vec::with_capacity(gray.len() * 4);
        for pixel in gray.pixels() {
            let value = pixel.0[0];
            bgra.extend_from_slice(&[value, value, value, 255]);
        }
        (size, size, bgra)
    }
}
