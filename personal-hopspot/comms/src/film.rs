//! The camera while a video call is open.
//!
//! Frames are scaled down as they arrive. The search camera is a different session;
//! this one only feeds the call.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput,
    AVCaptureInput, AVCaptureOutput, AVCaptureSession, AVCaptureSessionPreset640x480,
    AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, kCVReturnSuccess, CVPixelBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSDictionary, NSNumber, NSObject, NSString};

use crate::video::i420_from_bgra;

static RUN: AtomicU64 = AtomicU64::new(0);
static CURRENT: AtomicU64 = AtomicU64::new(0);
static STOP: AtomicBool = AtomicBool::new(false);
static BUSY: AtomicBool = AtomicBool::new(false);
static FRAME: AtomicU64 = AtomicU64::new(0);
static SEEN: AtomicU64 = AtomicU64::new(0);
static HOLD: Mutex<()> = Mutex::new(());
static LATEST: Mutex<Option<(u64, u32, u32, Vec<u8>)>> = Mutex::new(None);

struct FilmIvars {
    run: u64,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AllocAnyThread]
    #[name = "JournalFilm"]
    #[ivars = FilmIvars]
    struct FilmFinder;

    unsafe impl NSObjectProtocol for FilmFinder {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for FilmFinder {
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

impl FilmFinder {
    fn new(run: u64) -> Retained<Self> {
        let this = Self::alloc().set_ivars(FilmIvars { run });
        // SAFETY: `init` is NSObject's initializer, called on the object we just allocated.
        unsafe { msg_send![super(this), init] }
    }
}

struct Opened {
    session: Retained<AVCaptureSession>,
    output: Retained<AVCaptureVideoDataOutput>,
    /// AVFoundation does not keep the delegate alive. Frames stop if it is dropped.
    _delegate: Retained<FilmFinder>,
    _queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
}

/// Opens the camera and returns once frames can arrive.
pub fn open() -> Result<(), ()> {
    STOP.store(false, Ordering::Release);
    let run = RUN.fetch_add(1, Ordering::AcqRel) + 1;
    CURRENT.store(run, Ordering::Release);
    *lock(&LATEST) = None;
    SEEN.store(0, Ordering::Release);
    FRAME.store(0, Ordering::Release);
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("journal-film".into())
        .spawn(move || camera_thread(run, tx))
        .map_err(|_| ())?;
    match rx.recv_timeout(Duration::from_secs(55)) {
        Ok(true) => Ok(()),
        _ => {
            close();
            Err(())
        }
    }
}

pub fn close() {
    STOP.store(true, Ordering::Release);
    CURRENT.store(0, Ordering::Release);
}

/// The newest frame, once. A repeat of the same frame is `None`.
pub fn take() -> Option<(u32, u32, Vec<u8>)> {
    let latest = lock(&LATEST);
    let (generation, width, height, frame) = latest.as_ref()?;
    if SEEN.load(Ordering::Acquire) == *generation {
        return None;
    }
    SEEN.store(*generation, Ordering::Release);
    Some((*width, *height, frame.clone()))
}

fn camera_thread(run: u64, ready: mpsc::SyncSender<bool>) {
    let _hold = HOLD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if CURRENT.load(Ordering::Acquire) != run || STOP.load(Ordering::Acquire) {
        let _ = ready.send(false);
        return;
    }
    // SAFETY: AVMediaTypeVideo is a process-wide media-type constant.
    let Some(media) = (unsafe { AVMediaTypeVideo }) else {
        let _ = ready.send(false);
        return;
    };
    if !allowed(media, run) {
        let _ = ready.send(false);
        return;
    }
    if CURRENT.load(Ordering::Acquire) != run || STOP.load(Ordering::Acquire) {
        let _ = ready.send(false);
        return;
    }
    // SAFETY: the session is configured, started, and stopped on this thread.
    // The frame receiver and its queue stay alive until the callback is cleared.
    let opened = unsafe { open_session(run, media) };
    let Some(opened) = opened else {
        let _ = ready.send(false);
        return;
    };
    let _ = ready.send(true);
    while CURRENT.load(Ordering::Acquire) == run && !STOP.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(40));
    }
    // SAFETY: this is the session and output opened above. The delegate is cleared
    // before either is dropped.
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
        return false;
    }
    let (tx, rx) = mpsc::channel();
    let block = block2::RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });
    // SAFETY: requests the system camera prompt. The handler only sends a bool.
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &block) };
    let granted = rx.recv_timeout(Duration::from_secs(50)).unwrap_or(false);
    granted && CURRENT.load(Ordering::Acquire) == run && !STOP.load(Ordering::Acquire)
}

/// SAFETY: `media` is the video type. The returned session is running and must be stopped
/// by the caller before it is dropped. The delegate must be kept until the callback is cleared.
unsafe fn open_session(run: u64, media: &objc2_av_foundation::AVMediaType) -> Option<Opened> {
    // SAFETY: the default video device for this process. Nil means there is no camera.
    let device = unsafe { AVCaptureDevice::defaultDeviceWithMediaType(media) }?;
    // SAFETY: opens that device for capture. An error means it is unavailable.
    let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }.ok()?;
    let output = unsafe { AVCaptureVideoDataOutput::new() };
    let format = NSNumber::numberWithUnsignedInt(kCVPixelFormatType_32BGRA);
    let key = NSString::from_str("PixelFormatType");
    let settings = NSDictionary::<NSString, AnyObject>::from_slices(&[&*key], &[format.as_ref()]);
    let delegate = FilmFinder::new(run);
    let queue = dispatch2::DispatchQueue::new("journal.film.frames", None);
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
        let preset = AVCaptureSessionPreset640x480;
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

fn on_frame(sample: &CMSampleBuffer) {
    if STOP.load(Ordering::Acquire) || BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Some((width, height, picture)) = grab_frame(sample) {
        let generation = FRAME.fetch_add(1, Ordering::AcqRel) + 1;
        *lock(&LATEST) = Some((generation, width, height, picture));
    }
    BUSY.store(false, Ordering::Release);
}

fn grab_frame(sample: &CMSampleBuffer) -> Option<(u32, u32, Vec<u8>)> {
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
    if CVPixelBufferGetPixelFormatType(pixel) != kCVPixelFormatType_32BGRA {
        return None;
    }
    let width = u32::try_from(CVPixelBufferGetWidth(pixel)).ok()?;
    let height = u32::try_from(CVPixelBufferGetHeight(pixel)).ok()?;
    let stride = u32::try_from(CVPixelBufferGetBytesPerRow(pixel)).ok()?;
    if width == 0 || height == 0 || stride < width.saturating_mul(4) {
        return None;
    }
    let base = CVPixelBufferGetBaseAddress(pixel);
    if base.is_null() {
        return None;
    }
    let len = usize::try_from(stride).ok()?.checked_mul(height as usize)?;
    // SAFETY: the base address is locked above, and `len` is the row stride times the height.
    let src = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), len) };
    i420_from_bgra(src, width, height, stride)
}

struct Unlock<'a>(&'a CVPixelBuffer);

impl Drop for Unlock<'_> {
    fn drop(&mut self) {
        // SAFETY: this lock was taken with the same read-only flag in `grab_frame`.
        unsafe {
            CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
