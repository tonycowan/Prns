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

pub fn copy_text(text: &str) {
    call_bridge("copyText", Some(text));
}
