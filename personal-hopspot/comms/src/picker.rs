//! The iPhone file picker.
//!
//! The page's file input never opens a picker inside the journal's web view,
//! so File asks iOS itself.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool, NSObjectProtocol};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_foundation::{NSObject, NSString};

#[link(name = "UniformTypeIdentifiers", kind = "framework")]
extern "C" {}

pub enum Picked {
    Ready { name: String, bytes: Vec<u8> },
    Over,
    Missing,
    Cancelled,
}

static BUSY: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static OPEN: Mutex<Option<Retained<FilePicker>>> = Mutex::new(None);

struct PickerIvars {
    tx: Mutex<Option<mpsc::Sender<Picked>>>,
    max: u64,
    generation: u64,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AllocAnyThread]
    #[name = "JournalFilePicker"]
    #[ivars = PickerIvars]
    struct FilePicker;

    unsafe impl NSObjectProtocol for FilePicker {}

    impl FilePicker {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn did_pick(&self, picker: &AnyObject, urls: &AnyObject) {
            let picked = read_urls(urls, self.ivars().max);
            dismiss(picker);
            self.finish(picked);
        }

        #[unsafe(method(documentPickerWasCancelled:))]
        fn cancelled(&self, picker: &AnyObject) {
            dismiss(picker);
            self.finish(Picked::Cancelled);
        }
    }
);

impl FilePicker {
    fn new(tx: mpsc::Sender<Picked>, max: u64) -> Retained<Self> {
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        let this = Self::alloc().set_ivars(PickerIvars {
            tx: Mutex::new(Some(tx)),
            max,
            generation,
        });
        // SAFETY: `init` is NSObject's initializer, called on the object we just allocated.
        unsafe { msg_send![super(this), init] }
    }

    fn finish(&self, picked: Picked) {
        let generation = self.ivars().generation;
        if let Some(tx) = self
            .ivars()
            .tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = tx.send(picked);
        }
        BUSY.store(false, Ordering::Release);
        dispatch2::DispatchQueue::main().exec_async(move || {
            let mut open = OPEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if open
                .as_ref()
                .is_some_and(|picker| picker.ivars().generation == generation)
            {
                *open = None;
            }
        });
    }
}

/// Shows the system file picker and waits until the person chooses or cancels.
pub fn choose(max_bytes: u64) -> Picked {
    if BUSY.swap(true, Ordering::AcqRel) {
        return Picked::Cancelled;
    }
    let (tx, rx) = mpsc::channel();
    dispatch2::DispatchQueue::main().exec_async(move || present(tx, max_bytes));
    rx.recv_timeout(Duration::from_secs(600))
        .unwrap_or(Picked::Cancelled)
}

fn present(tx: mpsc::Sender<Picked>, max_bytes: u64) {
    if let Err(picked) = show(tx, max_bytes) {
        BUSY.store(false, Ordering::Release);
        let _ = picked;
    }
}

fn show(tx: mpsc::Sender<Picked>, max_bytes: u64) -> Result<(), Picked> {
    let fail = |tx: mpsc::Sender<Picked>, picked: Picked| {
        let _ = tx.send(picked);
        Err(Picked::Missing)
    };
    let Some(picker_cls) = AnyClass::get(c"UIDocumentPickerViewController") else {
        return fail(tx, Picked::Missing);
    };
    let Some(ut_cls) = AnyClass::get(c"UTType") else {
        return fail(tx, Picked::Missing);
    };
    let Some(array_cls) = AnyClass::get(c"NSArray") else {
        return fail(tx, Picked::Missing);
    };
    let ident = NSString::from_str("public.item");
    let item: Option<Retained<AnyObject>> =
        unsafe { msg_send![ut_cls, typeWithIdentifier: &*ident] };
    let Some(item) = item else {
        return fail(tx, Picked::Missing);
    };
    let objects = [Retained::as_ptr(&item)];
    let types: Option<Retained<AnyObject>> =
        unsafe { msg_send![array_cls, arrayWithObjects: objects.as_ptr(), count: 1usize] };
    let Some(types) = types else {
        return fail(tx, Picked::Missing);
    };
    let delegate = FilePicker::new(tx, max_bytes);
    let picker: Option<Retained<AnyObject>> = unsafe {
        msg_send![
            msg_send![picker_cls, alloc],
            initForOpeningContentTypes: &*types,
            asCopy: Bool::YES
        ]
    };
    let Some(picker) = picker else {
        return Err(Picked::Missing);
    };
    let Some(root) = presenter() else {
        if let Some(tx) = delegate
            .ivars()
            .tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = tx.send(Picked::Missing);
        }
        return Err(Picked::Missing);
    };
    unsafe {
        let _: () = msg_send![&*picker, setDelegate: &*delegate];
    }
    *OPEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(delegate);
    unsafe {
        let _: () = msg_send![
            &*root,
            presentViewController: &*picker,
            animated: Bool::YES,
            completion: std::ptr::null::<AnyObject>()
        ];
    }
    Ok(())
}

fn presenter() -> Option<Retained<AnyObject>> {
    let app_cls = AnyClass::get(c"UIApplication")?;
    let app: Option<Retained<AnyObject>> = unsafe { msg_send![app_cls, sharedApplication] };
    let app = app?;
    let window = key_window(&app)?;
    let current: Option<Retained<AnyObject>> = unsafe { msg_send![&*window, rootViewController] };
    let mut current = current?;
    loop {
        let presented: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*current, presentedViewController] };
        match presented {
            Some(next) => current = next,
            None => return Some(current),
        }
    }
}

fn key_window(app: &AnyObject) -> Option<Retained<AnyObject>> {
    let scene_cls = AnyClass::get(c"UIWindowScene")?;
    let scenes: Option<Retained<AnyObject>> = unsafe { msg_send![app, connectedScenes] };
    let scenes = scenes?;
    let all: Option<Retained<AnyObject>> = unsafe { msg_send![&*scenes, allObjects] };
    let all = all?;
    let count: usize = unsafe { msg_send![&*all, count] };
    let mut fallback = None;
    for index in 0..count {
        let scene: Option<Retained<AnyObject>> = unsafe { msg_send![&*all, objectAtIndex: index] };
        let Some(scene) = scene else { continue };
        let is_window: Bool = unsafe { msg_send![&*scene, isKindOfClass: scene_cls] };
        if !is_window.as_bool() {
            continue;
        }
        let windows: Option<Retained<AnyObject>> = unsafe { msg_send![&*scene, windows] };
        let Some(windows) = windows else { continue };
        let windows_count: usize = unsafe { msg_send![&*windows, count] };
        for index in 0..windows_count {
            let window: Option<Retained<AnyObject>> =
                unsafe { msg_send![&*windows, objectAtIndex: index] };
            let Some(window) = window else { continue };
            let key: Bool = unsafe { msg_send![&*window, isKeyWindow] };
            if key.as_bool() {
                return Some(window);
            }
            if fallback.is_none() {
                fallback = Some(window);
            }
        }
    }
    fallback
}

fn dismiss(picker: &AnyObject) {
    unsafe {
        let _: () = msg_send![
            picker,
            dismissViewControllerAnimated: Bool::YES,
            completion: std::ptr::null::<AnyObject>()
        ];
    }
}

fn read_urls(urls: &AnyObject, max: u64) -> Picked {
    let count: usize = unsafe { msg_send![urls, count] };
    if count == 0 {
        return Picked::Cancelled;
    }
    let url: Option<Retained<AnyObject>> = unsafe { msg_send![urls, objectAtIndex: 0usize] };
    let Some(url) = url else {
        return Picked::Missing;
    };
    read_url(&url, max)
}

fn read_url(url: &AnyObject, max: u64) -> Picked {
    let started: Bool = unsafe { msg_send![url, startAccessingSecurityScopedResource] };
    let picked = autoreleasepool(|pool| read_open(url, max, pool));
    if started.as_bool() {
        unsafe {
            let _: () = msg_send![url, stopAccessingSecurityScopedResource];
        }
    }
    picked
}

fn read_open(url: &AnyObject, max: u64, pool: objc2::rc::AutoreleasePool<'_>) -> Picked {
    let path: Option<Retained<NSString>> = unsafe { msg_send![url, path] };
    let Some(path) = path else {
        return Picked::Missing;
    };
    let path = unsafe { path.to_str(pool) };
    if path.is_empty() {
        return Picked::Missing;
    }
    let name: Option<Retained<NSString>> = unsafe { msg_send![url, lastPathComponent] };
    let name = name
        .map(|name| unsafe { name.to_str(pool) }.to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "file".into());
    let len = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if max > 0 && len > max {
        return Picked::Over;
    }
    match std::fs::read(path) {
        Ok(bytes) if !bytes.is_empty() => Picked::Ready { name, bytes },
        _ => Picked::Missing,
    }
}
