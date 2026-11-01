//! The most detailed place the device can produce.
//!
//! A reading keeps the smallest uncertainty it sees, and stops once that is
//! within twenty metres or twelve seconds have passed.
//! macOS and iOS need `NSLocationWhenInUseUsageDescription` in the app's Info.plist
//! ("N10A uses your location when you add it to a note.") or the system
//! will not ask, and this returns nothing.

use crate::model::Place;

#[cfg(any(target_os = "macos", target_os = "ios"))]
use objc2::encode::{Encode, Encoding, RefEncode};
#[cfg(any(target_os = "macos", target_os = "ios"))]
use objc2::rc::Retained;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use objc2::runtime::{AnyObject, NSObjectProtocol};
#[cfg(any(target_os = "macos", target_os = "ios"))]
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
#[cfg(any(target_os = "macos", target_os = "ios"))]
use objc2_foundation::NSObject;

#[cfg(not(target_arch = "wasm32"))]
pub async fn current_place() -> Option<Place> {
    tokio::task::spawn_blocking(blocking_place)
        .await
        .ok()
        .flatten()
}

#[cfg(target_arch = "wasm32")]
pub async fn current_place() -> Option<Place> {
    let window = web_sys::window()?;
    let geo = window.navigator().geolocation().ok()?;
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        let ok = wasm_bindgen::closure::Closure::once(move |pos: web_sys::Position| {
            let coords = pos.coords();
            let text = format!("{},{}", coords.latitude(), coords.longitude());
            let _ = resolve.call1(&wasm_bindgen::JsValue::NULL, &text.into());
        });
        let err = wasm_bindgen::closure::Closure::once(move |_err: wasm_bindgen::JsValue| {
            let _ = reject.call1(&wasm_bindgen::JsValue::NULL, &wasm_bindgen::JsValue::NULL);
        });
        use wasm_bindgen::JsCast;
        let mut options = web_sys::PositionOptions::new();
        options.enable_high_accuracy(true);
        options.maximum_age(0);
        options.timeout(20_000);
        let _ = geo.get_current_position_with_error_callback_and_options(
            ok.as_ref().unchecked_ref(),
            err.as_ref().unchecked_ref(),
            &options,
        );
        ok.forget();
        err.forget();
    });
    let value = wasm_bindgen_futures::JsFuture::from(promise).await.ok()?;
    Place::parse(&value.as_string()?)
}

/// Opens this place in the device's map app.
pub fn open_place(place: Place) {
    let lat = place.latitude();
    let lon = place.longitude();
    #[cfg(target_os = "macos")]
    {
        let url = format!("maps://?ll={lat:.7},{lon:.7}&q={lat:.7},{lon:.7}");
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "ios")]
    open_apple_maps(lat, lon);
    #[cfg(target_os = "android")]
    {
        let url = format!("geo:{lat:.7},{lon:.7}?q={lat:.7},{lon:.7}");
        crate::session::open_map(&url);
    }
    #[cfg(target_arch = "wasm32")]
    {
        let url = format!("https://maps.apple.com/?ll={lat:.7},{lon:.7}&q={lat:.7},{lon:.7}");
        if let Some(window) = web_sys::window() {
            let _ = window.open_with_url_and_target(&url, "_blank");
        }
    }
    #[cfg(all(
        unix,
        not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "android",
            target_arch = "wasm32"
        ))
    ))]
    {
        let url = format!("geo:{lat:.7},{lon:.7}?q={lat:.7},{lon:.7}");
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let url = format!("https://maps.apple.com/?ll={lat:.7},{lon:.7}&q={lat:.7},{lon:.7}");
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn();
    }
}

#[cfg(target_os = "ios")]
fn open_apple_maps(lat: f64, lon: f64) {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::{NSDictionary, NSString, NSURL};

    let url = format!("https://maps.apple.com/?ll={lat:.7},{lon:.7}&q={lat:.7},{lon:.7}");
    dispatch2::DispatchQueue::main().exec_async(move || {
        let Some(app_cls) = AnyClass::get(c"UIApplication") else {
            return;
        };
        let Some(nsurl) = NSURL::URLWithString(&NSString::from_str(&url)) else {
            return;
        };
        let options = NSDictionary::<AnyObject, AnyObject>::new();
        // SAFETY: `sharedApplication` is this process's UIApplication.
        // `openURL:options:completionHandler:` asks iOS to open that Apple Maps
        // URL. A nil completion handler is the documented way to ignore the result.
        unsafe {
            let app: *mut AnyObject = msg_send![app_cls, sharedApplication];
            if app.is_null() {
                return;
            }
            let _: () = msg_send![
                app,
                openURL: &*nsurl,
                options: &*options,
                completionHandler: std::ptr::null::<core::ffi::c_void>()
            ];
        }
    });
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn blocking_place() -> Option<Place> {
    use std::sync::mpsc;
    use std::time::Duration;

    let (tx, rx) = mpsc::channel();
    dispatch2::DispatchQueue::main().exec_async(move || request_on_main(tx));
    rx.recv_timeout(Duration::from_secs(60)).ok().flatten()
}

#[cfg(target_os = "android")]
fn blocking_place() -> Option<Place> {
    crate::session::device_location()
        .as_deref()
        .and_then(Place::parse)
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "android",
    target_arch = "wasm32"
)))]
fn blocking_place() -> Option<Place> {
    None
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[link(name = "CoreLocation", kind = "framework")]
extern "C" {
    static kCLLocationAccuracyBestForNavigation: f64;
}

/// A fix this sharp is detailed enough to stop waiting for a finer one.
#[cfg(any(target_os = "macos", target_os = "ios"))]
const DETAILED_METERS: f64 = 20.0;

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[repr(C)]
#[derive(Clone, Copy)]
struct Coordinate {
    latitude: f64,
    longitude: f64,
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
// SAFETY: CLLocationCoordinate2D is two double fields, latitude then longitude.
unsafe impl Encode for Coordinate {
    const ENCODING: Encoding =
        Encoding::Struct("CLLocationCoordinate2D", &[f64::ENCODING, f64::ENCODING]);
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
// SAFETY: A pointer to that struct is the reference encoding.
unsafe impl RefEncode for Coordinate {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
struct LocationIvars {
    tx: std::sync::Mutex<Option<std::sync::mpsc::Sender<Option<Place>>>>,
    started: std::sync::atomic::AtomicBool,
    best_accuracy: std::sync::Mutex<f64>,
    best: std::sync::Mutex<Option<Place>>,
    generation: u64,
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AllocAnyThread]
    #[name = "N10ALocationDelegate"]
    #[ivars = LocationIvars]
    struct LocationDelegate;

    unsafe impl NSObjectProtocol for LocationDelegate {}

    impl LocationDelegate {
        #[unsafe(method(locationManager:didUpdateLocations:))]
        fn did_update(&self, manager: &AnyObject, locations: &AnyObject) {
            // SAFETY: CoreLocation passes the locations array for this callback.
            // `count` is the array length. `lastObject` is a CLLocation or null.
            // `coordinate` is two f64 fields. `horizontalAccuracy` is the
            // uncertainty in metres, negative when the fix is not usable.
            let count: usize = unsafe { msg_send![locations, count] };
            if count == 0 {
                return;
            }
            let loc: *mut AnyObject = unsafe { msg_send![locations, lastObject] };
            if loc.is_null() {
                return;
            }
            let coord: Coordinate = unsafe { msg_send![loc, coordinate] };
            let accuracy: f64 = unsafe { msg_send![loc, horizontalAccuracy] };
            if !accuracy.is_finite() || accuracy < 0.0 {
                return;
            }
            let place = Place::from_degrees(coord.latitude, coord.longitude);
            let sharp = {
                let Ok(mut best_accuracy) = self.ivars().best_accuracy.lock() else {
                    return;
                };
                let Ok(mut best) = self.ivars().best.lock() else {
                    return;
                };
                if accuracy < *best_accuracy {
                    *best_accuracy = accuracy;
                    *best = place;
                }
                *best_accuracy <= DETAILED_METERS
            };
            if !sharp {
                return;
            }
            // SAFETY: A fix this sharp is the most detailed one worth waiting for.
            // Stopping updates ends this request.
            let _: () = unsafe { msg_send![manager, stopUpdatingLocation] };
            self.finish_best();
        }

        #[unsafe(method(locationManager:didFailWithError:))]
        fn did_fail(&self, manager: &AnyObject, _error: &AnyObject) {
            if !self
                .ivars()
                .started
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            // SAFETY: The manager argument is the CLLocationManager that failed
            // after a fix was requested. Stopping updates ends this request.
            // The finest fix already received, if any, is the one that is kept.
            let _: () = unsafe { msg_send![manager, stopUpdatingLocation] };
            self.finish_best();
        }

        #[unsafe(method(locationManagerDidChangeAuthorization:))]
        fn did_change(&self, manager: &AnyObject) {
            // SAFETY: `authorizationStatus` is this manager's CLAuthorizationStatus.
            let status: isize = unsafe { msg_send![manager, authorizationStatus] };
            self.on_status(manager, status);
        }

        #[unsafe(method(locationManager:didChangeAuthorizationStatus:))]
        fn did_change_status(&self, manager: &AnyObject, status: i32) {
            self.on_status(manager, status as isize);
        }
    }
);

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl LocationDelegate {
    fn new(tx: std::sync::mpsc::Sender<Option<Place>>, generation: u64) -> Retained<Self> {
        let this = Self::alloc().set_ivars(LocationIvars {
            tx: std::sync::Mutex::new(Some(tx)),
            started: std::sync::atomic::AtomicBool::new(false),
            best_accuracy: std::sync::Mutex::new(f64::MAX),
            best: std::sync::Mutex::new(None),
            generation,
        });
        // SAFETY: `init` is NSObject's initializer, called on the object just allocated.
        unsafe { objc2::msg_send![super(this), init] }
    }

    fn finish(&self, place: Option<Place>) {
        let Some(tx) = self.ivars().tx.lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        let _ = tx.send(place);
    }

    fn finish_best(&self) {
        let place = self
            .ivars()
            .best
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        self.finish(place);
    }

    fn on_status(&self, manager: &AnyObject, status: isize) {
        if status == 0 {
            return;
        }
        if status == 1 || status == 2 {
            // SAFETY: The user has refused location, or the system has restricted it.
            let _: () = unsafe { msg_send![manager, stopUpdatingLocation] };
            self.finish(None);
            return;
        }
        if self
            .ivars()
            .started
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        // SAFETY: Location is allowed. Updates continue until a fix is within
        // twenty metres, or twelve seconds pass, and the finest one is kept.
        let _: () = unsafe { msg_send![manager, startUpdatingLocation] };
        let generation = self.ivars().generation;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(12));
            dispatch2::DispatchQueue::main().exec_async(move || {
                HOLD.with(|hold| {
                    let hold = hold.borrow();
                    let Some(current) = hold.as_ref() else {
                        return;
                    };
                    if current.delegate.ivars().generation != generation {
                        return;
                    }
                    // SAFETY: `current.manager` is the CLLocationManager for this
                    // request. Twelve seconds is long enough for the finest fix
                    // the device is going to produce. Stopping updates ends it.
                    let _: () = unsafe { msg_send![&*current.manager, stopUpdatingLocation] };
                    current.delegate.finish_best();
                });
            });
        });
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
struct HeldFix {
    manager: Retained<AnyObject>,
    /// Retained so CoreLocation can message it after the request function returns.
    #[allow(dead_code)]
    delegate: Retained<LocationDelegate>,
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
thread_local! {
    static HOLD: std::cell::RefCell<Option<HeldFix>> = const { std::cell::RefCell::new(None) };
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn request_on_main(tx: std::sync::mpsc::Sender<Option<Place>>) {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};

    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;

    let Some(cls) = AnyClass::get(c"CLLocationManager") else {
        let _ = tx.send(None);
        return;
    };
    // SAFETY: `cls` is CLLocationManager. `new` returns a retained instance.
    // `kCLLocationAccuracyBestForNavigation` asks for the finest fix the
    // device can produce. `kCLDistanceFilterNone` (`-1`) delivers every one,
    // so the sharpest can be kept. `authorizationStatus` is the current
    // CLAuthorizationStatus. The delegate is a retained NSObject.
    let manager: Retained<AnyObject> = unsafe { msg_send![cls, new] };
    let accuracy = unsafe { kCLLocationAccuracyBestForNavigation };
    let _: () = unsafe { msg_send![&*manager, setDesiredAccuracy: accuracy] };
    let _: () = unsafe { msg_send![&*manager, setDistanceFilter: -1.0_f64] };
    let delegate = LocationDelegate::new(tx, generation);
    // SAFETY: The delegate receives the authorization dialog's result.
    // A fix is requested only after that result is allowed, so the dialog
    // is not cancelled by an early failure.
    let _: () = unsafe { msg_send![&*manager, setDelegate: &*delegate] };
    let status: isize = unsafe { msg_send![&*manager, authorizationStatus] };
    if status == 0 {
        let _: () = unsafe { msg_send![&*manager, requestWhenInUseAuthorization] };
    } else {
        delegate.on_status(&manager, status);
    }
    HOLD.with(|hold| {
        let mut hold = hold.borrow_mut();
        if let Some(old) = hold.take() {
            // SAFETY: `old.manager` is the CLLocationManager kept from the previous request.
            let _: () = unsafe { msg_send![&*old.manager, stopUpdatingLocation] };
        }
        *hold = Some(HeldFix { manager, delegate });
    });
}
