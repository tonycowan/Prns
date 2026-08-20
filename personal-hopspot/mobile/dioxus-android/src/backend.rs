//! UI backend: mock (browser preview) or live Hopspot service bridge (WebView host).

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;

/// True when the Android WebView injected `window.HopspotBridge`.
#[cfg(target_arch = "wasm32")]
pub fn is_live() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    js_sys::Reflect::get(&window, &"HopspotBridge".into())
        .ok()
        .is_some_and(|value| !value.is_undefined() && !value.is_null())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn is_live() -> bool {
    false
}

#[cfg(target_arch = "wasm32")]
fn bridge() -> Option<js_sys::Object> {
    let window = web_sys::window()?;
    let value = js_sys::Reflect::get(&window, &"HopspotBridge".into()).ok()?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    value.dyn_into::<js_sys::Object>().ok()
}

#[cfg(target_arch = "wasm32")]
pub fn poll_snapshot_json() -> Option<String> {
    let bridge = bridge()?;
    let func = js_sys::Reflect::get(&bridge, &"getSnapshot".into()).ok()?;
    let func = func.dyn_into::<js_sys::Function>().ok()?;
    let result = func.call0(&bridge).ok()?;
    result.as_string()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn poll_snapshot_json() -> Option<String> {
    None
}

#[cfg(target_arch = "wasm32")]
fn call_bridge(method: &str, arg: Option<&str>) {
    let Some(bridge) = bridge() else {
        return;
    };
    let Ok(func) = js_sys::Reflect::get(&bridge, &method.into()) else {
        return;
    };
    let Ok(func) = func.dyn_into::<js_sys::Function>() else {
        return;
    };
    let _ = match arg {
        Some(value) => func.call1(&bridge, &wasm_bindgen::JsValue::from_str(value)),
        None => func.call0(&bridge),
    };
}

#[cfg(not(target_arch = "wasm32"))]
fn call_bridge(_method: &str, _arg: Option<&str>) {}

pub fn announce() {
    call_bridge("announce", None);
}

pub fn sleep_interfaces() {
    call_bridge("sleepInterfaces", None);
}

pub fn wake_interfaces() {
    call_bridge("wakeInterfaces", None);
}

pub fn toggle_interface(id_hex: &str) {
    call_bridge("toggleInterface", Some(id_hex));
}

/// Ask the host to copy [text] without calling `@JavascriptInterface` in this turn.
///
/// Live Android: stash on `window.__hopspotPendingCopy` and log a marker so Kotlin
/// can `evaluateJavascript` + ClipboardManager after navigation paints. Calling
/// bridge/clipboard/`WebView.onPause` during the click froze the WASM UI on
/// RNS Config (even `on_done` before copy never painted).
#[cfg(target_arch = "wasm32")]
pub fn schedule_copy(text: &str) {
    if is_live() {
        let Some(window) = web_sys::window() else {
            return;
        };
        let _ = js_sys::Reflect::set(
            &window,
            &"__hopspotPendingCopy".into(),
            &wasm_bindgen::JsValue::from_str(text),
        );
        web_sys::console::log_1(&"HOPSPOT_COPY_READY".into());
        return;
    }
    let _ = start_clipboard_write(text);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn schedule_copy(_text: &str) {}

#[cfg(target_arch = "wasm32")]
fn start_clipboard_write(text: &str) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let clipboard = window.navigator().clipboard();
    let _promise = clipboard.write_text(text);
    true
}
