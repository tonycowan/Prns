//! iOS scene lifecycle for the Dioxus mobile Controller.
//!
//! iOS 17+ traps at launch unless Info.plist names a real scene delegate, and it
//! only shows a window that belongs to that scene. Dioxus creates its UIWindow
//! before the scene connects, so this module:
//! 1. registers `PrnsControllerSceneDelegate`
//! 2. watches `UIWindow.setRootViewController:`
//! 3. attaches every seen window to the connected scene and makes it visible

use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};

/// The window scene iOS has connected, remembered so a window created later can join it.
static SCENE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// `UIWindow`s created before the scene connects.
fn seen_windows() -> &'static Mutex<Vec<usize>> {
    static WINDOWS: OnceLock<Mutex<Vec<usize>>> = OnceLock::new();
    WINDOWS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Registers the scene delegate and starts watching for Dioxus windows.
pub fn adopt_scene_lifecycle() {
    watch_for_windows();
    register_scene_delegate();
}

/// Remembers each window as it gains a root controller, and gives it to the open scene.
fn watch_for_windows() {
    static ORIGINAL: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
        fn class_getInstanceMethod(class: *mut c_void, selector: *mut c_void) -> *mut c_void;
        fn method_setImplementation(
            method: *mut c_void,
            implementation: *mut c_void,
        ) -> *mut c_void;
    }

    unsafe extern "C" fn set_root_view_controller(
        window: *mut c_void,
        selector: *mut c_void,
        controller: *mut c_void,
    ) {
        // SAFETY: `ORIGINAL` is the UIKit implementation captured before this wrapper
        // was installed. Calling it performs the real assignment.
        let original: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) =
            unsafe { std::mem::transmute(ORIGINAL.load(Ordering::Relaxed)) };
        unsafe { original(window, selector, controller) };
        if !seen_windows()
            .lock()
            .expect("window list")
            .contains(&(window as usize))
        {
            seen_windows()
                .lock()
                .expect("window list")
                .push(window as usize);
            eprintln!("controller: saw a window");
        }
        attach_known_windows();
    }

    // SAFETY: this runs on the main thread before any window exists. Replacing
    // `setRootViewController:` keeps UIKit's original implementation and records the window.
    unsafe {
        let class = objc_getClass(c"UIWindow".as_ptr());
        let selector = sel_registerName(c"setRootViewController:".as_ptr());
        let method = class_getInstanceMethod(class, selector);
        let previous = method_setImplementation(
            method,
            set_root_view_controller
                as unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) as *mut c_void,
        );
        ORIGINAL.store(previous, Ordering::Relaxed);
    }
}

/// Gives every window we have seen to the scene iOS connected.
fn attach_known_windows() {
    let scene = SCENE.load(Ordering::Acquire);
    if scene.is_null() {
        return;
    }
    let windows = seen_windows().lock().expect("window list").clone();
    for window in windows {
        attach_window(window as *mut c_void, scene);
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IosPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IosSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IosRect {
    origin: IosPoint,
    size: IosSize,
}

/// Puts one window on the connected scene, stretches it to that scene, and shows it.
fn attach_window(window: *mut c_void, scene: *mut c_void) {
    unsafe extern "C" {
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    // SAFETY: these pointers come from UIKit on the main thread.
    unsafe {
        let send_obj: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let send_void: unsafe extern "C" fn(*mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        let send_void_obj: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        let send_rect: unsafe extern "C" fn(*mut c_void, *mut c_void) -> IosRect =
            std::mem::transmute(objc_msgSend as *const ());
        let send_void_rect: unsafe extern "C" fn(*mut c_void, *mut c_void, IosRect) =
            std::mem::transmute(objc_msgSend as *const ());
        let send_usize: unsafe extern "C" fn(*mut c_void, *mut c_void) -> usize =
            std::mem::transmute(objc_msgSend as *const ());
        let send_at: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let send_mask: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) =
            std::mem::transmute(objc_msgSend as *const ());

        let current = send_obj(window, sel_registerName(c"windowScene".as_ptr()));
        if current != scene {
            send_void_obj(window, sel_registerName(c"setWindowScene:".as_ptr()), scene);
        }

        let space = send_obj(scene, sel_registerName(c"coordinateSpace".as_ptr()));
        let bounds = send_rect(space, sel_registerName(c"bounds".as_ptr()));
        if bounds.size.width < 1.0 || bounds.size.height < 1.0 {
            eprintln!("controller: the scene has no size yet");
            return;
        }
        let frame = send_rect(window, sel_registerName(c"frame".as_ptr()));
        eprintln!(
            "controller: window {} by {} at {}, {}; scene {} by {}",
            frame.size.width,
            frame.size.height,
            frame.origin.x,
            frame.origin.y,
            bounds.size.width,
            bounds.size.height
        );
        send_void_rect(window, sel_registerName(c"setFrame:".as_ptr()), bounds);

        let root = send_obj(window, sel_registerName(c"rootViewController".as_ptr()));
        let view = send_obj(root, sel_registerName(c"view".as_ptr()));
        if !view.is_null() {
            let view_bounds = IosRect {
                origin: IosPoint { x: 0.0, y: 0.0 },
                size: bounds.size,
            };
            // Flexible width and height, so the page follows the window.
            send_mask(view, sel_registerName(c"setAutoresizingMask:".as_ptr()), 18);
            send_void_rect(view, sel_registerName(c"setFrame:".as_ptr()), view_bounds);
            let subviews = send_obj(view, sel_registerName(c"subviews".as_ptr()));
            let count = send_usize(subviews, sel_registerName(c"count".as_ptr()));
            for index in 0..count {
                let subview = send_at(
                    subviews,
                    sel_registerName(c"objectAtIndex:".as_ptr()),
                    index,
                );
                send_mask(
                    subview,
                    sel_registerName(c"setAutoresizingMask:".as_ptr()),
                    18,
                );
                send_void_rect(
                    subview,
                    sel_registerName(c"setFrame:".as_ptr()),
                    view_bounds,
                );
            }
            send_void(view, sel_registerName(c"layoutIfNeeded".as_ptr()));
        }
        send_void(window, sel_registerName(c"makeKeyAndVisible".as_ptr()));
    }
}

fn register_scene_delegate() {
    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
        fn objc_allocateClassPair(
            superclass: *mut c_void,
            name: *const std::ffi::c_char,
            extra_bytes: usize,
        ) -> *mut c_void;
        fn objc_registerClassPair(class: *mut c_void);
        fn class_addMethod(
            class: *mut c_void,
            selector: *mut c_void,
            implementation: *mut c_void,
            types: *const std::ffi::c_char,
        ) -> u8;
        fn class_addProtocol(class: *mut c_void, protocol: *mut c_void) -> u8;
        fn objc_getProtocol(name: *const std::ffi::c_char) -> *mut c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
    }

    unsafe extern "C" fn scene_will_connect(
        _this: *mut c_void,
        _cmd: *mut c_void,
        scene: *mut c_void,
        _session: *mut c_void,
        _options: *mut c_void,
    ) {
        remember_scene(scene);
    }

    unsafe extern "C" fn scene_did_become_active(
        _this: *mut c_void,
        _cmd: *mut c_void,
        scene: *mut c_void,
    ) {
        remember_scene(scene);
    }

    fn remember_scene(scene: *mut c_void) {
        unsafe extern "C" {
            fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
            fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
            fn objc_msgSend();
        }
        // SAFETY: `scene` is the UIScene UIKit passed into the delegate.
        let is_window_scene = unsafe {
            let send: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> u8 =
                std::mem::transmute(objc_msgSend as *const ());
            send(
                scene,
                sel_registerName(c"isKindOfClass:".as_ptr()),
                objc_getClass(c"UIWindowScene".as_ptr()),
            )
        };
        if is_window_scene == 0 {
            eprintln!("controller: the connected scene is not a window scene");
            return;
        }
        SCENE.store(scene, Ordering::Release);
        eprintln!("controller: a window scene connected");
        attach_known_windows();
    }

    // SAFETY: the class is registered before UIKit starts, and the methods match the
    // scene-delegate selectors UIKit calls.
    unsafe {
        let superclass = objc_getClass(c"UIResponder".as_ptr());
        let class = objc_allocateClassPair(superclass, c"PrnsControllerSceneDelegate".as_ptr(), 0);
        assert!(
            !class.is_null(),
            "the scene delegate class could not be created"
        );
        let added = class_addMethod(
            class,
            sel_registerName(c"scene:willConnectToSession:options:".as_ptr()),
            scene_will_connect
                as unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void)
                    as *mut c_void,
            c"v@:@@@".as_ptr(),
        );
        assert!(added != 0, "the scene connection method could not be added");
        let added = class_addMethod(
            class,
            sel_registerName(c"sceneDidBecomeActive:".as_ptr()),
            scene_did_become_active
                as unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) as *mut c_void,
            c"v@:@".as_ptr(),
        );
        assert!(added != 0, "the scene activation method could not be added");
        for protocol_name in [c"UISceneDelegate", c"UIWindowSceneDelegate"] {
            let protocol = objc_getProtocol(protocol_name.as_ptr());
            assert!(
                !protocol.is_null(),
                "a scene protocol is missing from UIKit"
            );
            let added = class_addProtocol(class, protocol);
            assert!(added != 0, "the scene protocol could not be adopted");
        }
        objc_registerClassPair(class);
    }
}
