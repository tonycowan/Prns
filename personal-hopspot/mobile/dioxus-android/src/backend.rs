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

/// Call into the Android host. Host methods must return immediately (post work);
/// a blocking `@JavascriptInterface` freezes the WebView while the UI thread waits.
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

/// Copy RNS config: live host posts clipboard work; mock/web uses the Clipboard API.
pub fn copy_rns_config(text: &str) {
    if is_live() {
        call_bridge("copyRnsConfig", None);
        return;
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = start_clipboard_write(text);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = text;
    }
}

#[cfg(target_arch = "wasm32")]
fn start_clipboard_write(text: &str) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let clipboard = window.navigator().clipboard();
    let _promise = clipboard.write_text(text);
    true
}

/// Fingerprint of a live snapshot ignoring fields that change every poll.
///
/// `uptime_ms` / byte counters / activity age change continuously; applying a
/// full `DemoState` replace for those remounts the detail screen mid-interaction.
pub fn stable_snapshot_key(json: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
    };
    let Some(object) = value.as_object_mut() else {
        return json.to_string();
    };
    object.remove("uptime_ms");
    object.remove("rx_bytes");
    object.remove("tx_bytes");
    if let Some(cards) = object.get_mut("cards").and_then(|cards| cards.as_array_mut()) {
        for card in cards {
            if let Some(card) = card.as_object_mut() {
                card.remove("tx_bytes");
                card.remove("rx_bytes");
                card.remove("activity_age_secs");
            }
        }
    }
    value.to_string()
}
