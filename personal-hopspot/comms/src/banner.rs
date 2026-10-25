//! A banner for a note that arrives while Optio is not the front app.
//!
//! On iOS the banner is a local notification, and a background refresh keeps the
//! journal awake long enough to take a note the sender is still offering.
//! On Android the activity posts the banner. The activity also asks to be left
//! out of battery optimization, so sleep does not park that session.
//! On the desktop, the banner is the system notification while the window is
//! not in front.

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "windows"
))]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(any(target_os = "macos", target_os = "ios"))]
use std::sync::atomic::AtomicU64;

#[cfg(target_os = "ios")]
use std::sync::atomic::AtomicPtr;

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[link(name = "UserNotifications", kind = "framework")]
extern "C" {}

#[cfg(target_os = "ios")]
#[link(name = "BackgroundTasks", kind = "framework")]
extern "C" {}

/// The desktop window is the one the person is looking at.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
static FRONT: AtomicBool = AtomicBool::new(true);

/// Records whether the desktop window is in front.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub fn set_in_front(front: bool) {
    FRONT.store(front, Ordering::Release);
}

/// The journal is the app the person is looking at.
pub fn in_front() -> bool {
    #[cfg(target_os = "ios")]
    {
        return ios_in_front();
    }
    #[cfg(target_os = "android")]
    {
        return crate::session::screen_is_on();
    }
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        return FRONT.load(Ordering::Acquire);
    }
    #[cfg(not(any(
        target_os = "ios",
        target_os = "android",
        target_os = "macos",
        target_os = "linux",
        target_os = "windows"
    )))]
    {
        true
    }
}

/// Shows `name` and `words` as a system banner.
pub fn note(name: &str, words: &str) {
    #[cfg(not(any(
        target_os = "ios",
        target_os = "android",
        target_os = "macos",
        target_os = "linux",
        target_os = "windows"
    )))]
    {
        let _ = (name, words);
    }
    #[cfg(any(
        target_os = "ios",
        target_os = "android",
        target_os = "macos",
        target_os = "linux",
        target_os = "windows"
    ))]
    {
        let name = if name.trim().is_empty() {
            "Someone"
        } else {
            name.trim()
        };
        let words = if words.trim().is_empty() {
            "A new note"
        } else {
            words.trim()
        };
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        apple_note(name, words);
        #[cfg(target_os = "android")]
        crate::session::show_banner(name, words);
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        desktop_note(name, words);
    }
}

/// Registers the background refresh before UIKit finishes launching.
#[cfg(target_os = "ios")]
pub fn install() {
    ios_install();
}

/// Asks for notification permission the first time the journal is on screen.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub fn became_active() {
    apple_ask();
}

/// Asks iOS to open the journal again after it has left the screen.
#[cfg(target_os = "ios")]
pub fn schedule_refresh() {
    ios_schedule_refresh();
}

#[cfg(target_os = "ios")]
fn ios_in_front() -> bool {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    let Some(cls) = AnyClass::get(c"UIApplication") else {
        return true;
    };
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: `sharedApplication` is this process's UIApplication. Active is 0.
        let state: isize = unsafe {
            let app: *mut AnyObject = msg_send![cls, sharedApplication];
            if app.is_null() {
                return true;
            }
            msg_send![app, applicationState]
        };
        state == 0
    })
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn desktop_note(name: &str, words: &str) {
    if notify_rust::Notification::new()
        .appname("Optio")
        .summary(name)
        .body(words)
        .show()
        .is_err()
    {
        eprintln!("touchbase: the banner could not be shown");
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn apple_note(name: &str, words: &str) {
    use objc2::msg_send;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::NSString;

    let Some(content_cls) = AnyClass::get(c"UNMutableNotificationContent") else {
        return;
    };
    let Some(center_cls) = AnyClass::get(c"UNUserNotificationCenter") else {
        return;
    };
    let Some(request_cls) = AnyClass::get(c"UNNotificationRequest") else {
        return;
    };
    static NEXT_NOTE: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_NOTE.fetch_add(1, Ordering::Relaxed);
    autoreleasepool(|_| {
        let title = NSString::from_str(name);
        let body = NSString::from_str(words);
        let ident = NSString::from_str(&format!("note-{id}"));
        // SAFETY: the UserNotifications classes are linked. `new` and the factory
        // return objects owned by this pool. A nil trigger delivers the banner now.
        unsafe {
            let content: *mut AnyObject = msg_send![content_cls, new];
            if content.is_null() {
                return;
            }
            let _: () = msg_send![content, setTitle: &*title];
            let _: () = msg_send![content, setBody: &*body];
            if let Some(sound_cls) = AnyClass::get(c"UNNotificationSound") {
                let sound: *mut AnyObject = msg_send![sound_cls, defaultSound];
                let _: () = msg_send![content, setSound: sound];
            }
            let request: *mut AnyObject = msg_send![
                request_cls,
                requestWithIdentifier: &*ident,
                content: content,
                trigger: std::ptr::null::<AnyObject>()
            ];
            if request.is_null() {
                return;
            }
            let center: *mut AnyObject = msg_send![center_cls, currentNotificationCenter];
            if center.is_null() {
                return;
            }
            let _: () = msg_send![
                center,
                addNotificationRequest: request,
                withCompletionHandler: std::ptr::null::<AnyObject>()
            ];
        }
    });
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn apple_ask() {
    if ASKED.swap(true, Ordering::AcqRel) {
        return;
    }
    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    let Some(cls) = AnyClass::get(c"UNUserNotificationCenter") else {
        return;
    };
    // Alert is 4, sound is 2.
    let options: usize = 6;
    let done = RcBlock::new(|_granted: Bool, _error: *mut AnyObject| {});
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: `currentNotificationCenter` is the process center. The options
        // are alert and sound. The center copies the completion block.
        unsafe {
            let center: *mut AnyObject = msg_send![cls, currentNotificationCenter];
            if center.is_null() {
                return;
            }
            let _: () = msg_send![
                center,
                requestAuthorizationWithOptions: options,
                completionHandler: &*done
            ];
        }
    });
}

/// Matches `BGTaskSchedulerPermittedIdentifiers` in the iOS bundle.
#[cfg(target_os = "ios")]
const REFRESH_ID: &str = "org.personal.prns.comms.refresh";

#[cfg(target_os = "ios")]
fn ios_install() {
    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2_foundation::NSString;

    let Some(cls) = AnyClass::get(c"BGTaskScheduler") else {
        eprintln!("touchbase: background refresh is unavailable");
        return;
    };
    let ident = NSString::from_str(REFRESH_ID);
    let handler = RcBlock::new(|task: *mut AnyObject| {
        // SAFETY: UIKit passes a live BGTask and holds it through this call.
        // `retain` keeps it until the refresh window ends.
        let Some(task) = (unsafe { Retained::<AnyObject>::retain(task) }) else {
            return;
        };
        hold_refresh(task);
    });
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: registration has to happen before UIKit finishes launching.
        // A nil queue lets the system choose one. The scheduler copies the handler.
        unsafe {
            let scheduler: *mut AnyObject = msg_send![cls, sharedScheduler];
            if scheduler.is_null() {
                return;
            }
            let registered: Bool = msg_send![
                scheduler,
                registerForTaskWithIdentifier: &*ident,
                usingQueue: std::ptr::null::<AnyObject>(),
                launchHandler: &*handler
            ];
            if !bool::from(registered) {
                eprintln!("touchbase: background refresh was not registered");
            }
        }
    });
    std::mem::forget(handler);
}

#[cfg(target_os = "ios")]
fn ios_schedule_refresh() {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2_foundation::NSString;

    let Some(request_cls) = AnyClass::get(c"BGAppRefreshTaskRequest") else {
        return;
    };
    let Some(scheduler_cls) = AnyClass::get(c"BGTaskScheduler") else {
        return;
    };
    let Some(date_cls) = AnyClass::get(c"NSDate") else {
        return;
    };
    let ident = NSString::from_str(REFRESH_ID);
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: the request class is linked with BackgroundTasks. Fifteen minutes
        // is the earliest we ask for; iOS chooses the real time. A pending request
        // makes submit fail, and that failure is the request already waiting.
        unsafe {
            let request: *mut AnyObject = msg_send![request_cls, alloc];
            let request: *mut AnyObject = msg_send![request, initWithIdentifier: &*ident];
            if request.is_null() {
                return;
            }
            let earliest: *mut AnyObject =
                msg_send![date_cls, dateWithTimeIntervalSinceNow: 15.0_f64 * 60.0];
            let _: () = msg_send![request, setEarliestBeginDate: earliest];
            let scheduler: *mut AnyObject = msg_send![scheduler_cls, sharedScheduler];
            if scheduler.is_null() {
                return;
            }
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let submitted: Bool =
                msg_send![scheduler, submitTaskRequest: request, error: &mut error];
            if !bool::from(submitted) {
                eprintln!("touchbase: background refresh is already waiting");
            }
        }
    });
}

/// One refresh is open. The expiration handler and the timer both try to close it.
#[cfg(target_os = "ios")]
static REFRESH_OPEN: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "ios")]
static REFRESH_TASK: AtomicPtr<objc2::runtime::AnyObject> = AtomicPtr::new(std::ptr::null_mut());

#[cfg(target_os = "ios")]
fn hold_refresh(task: objc2::rc::Retained<objc2::runtime::AnyObject>) {
    use block2::RcBlock;
    use dispatch2::{DispatchQueue, DispatchTime};
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;

    let ptr = Retained::into_raw(task);
    let previous = REFRESH_TASK.swap(ptr, Ordering::AcqRel);
    if !previous.is_null() {
        // SAFETY: `previous` was stored by `into_raw` and has not been finished.
        drop(unsafe { Retained::<AnyObject>::from_raw(previous) });
    }
    REFRESH_OPEN.store(true, Ordering::Release);
    let expiration = RcBlock::new(|| finish_refresh(false));
    // SAFETY: `ptr` is the retained task. The system copies the expiration block.
    unsafe {
        let _: () = msg_send![ptr, setExpirationHandler: &*expiration];
    }
    std::mem::forget(expiration);
    let when =
        DispatchTime::try_from(std::time::Duration::from_secs(20)).unwrap_or(DispatchTime::FOREVER);
    let _ = DispatchQueue::main().after(when, || finish_refresh(true));
    ios_schedule_refresh();
}

#[cfg(target_os = "ios")]
fn finish_refresh(success: bool) {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};

    if !REFRESH_OPEN.swap(false, Ordering::AcqRel) {
        return;
    }
    let task = REFRESH_TASK.swap(std::ptr::null_mut(), Ordering::AcqRel);
    if task.is_null() {
        return;
    }
    // SAFETY: `task` is the retained BGTask from `hold_refresh`. Completing it
    // returns the runtime to iOS, and `from_raw` drops the retain.
    unsafe {
        let _: () = msg_send![task, setTaskCompletedWithSuccess: Bool::from(success)];
        drop(Retained::<AnyObject>::from_raw(task));
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
static ASKED: AtomicBool = AtomicBool::new(false);
