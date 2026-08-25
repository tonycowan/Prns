//! Fetch a single GPS fix for range-check.
//!
//! - macOS: CoreLocation (WebView geolocation hangs under WKWebView).
//! - Android / other live targets: WebView `navigator.geolocation`.
//! - Always bounded by a hard timeout so the UI cannot stall forever.

use std::time::Duration;

use crate::range_check::GeoPoint;

const FIX_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocationError {
    Unsupported,
    Denied,
    Unavailable,
    Timeout,
    BadPayload,
    Platform(String),
}

impl LocationError {
    pub fn label(&self) -> String {
        match self {
            Self::Unsupported => "Location is not available on this build.".into(),
            Self::Denied => "Location permission denied.".into(),
            Self::Unavailable => "Location unavailable.".into(),
            Self::Timeout => "Timed out waiting for GPS.".into(),
            Self::BadPayload => "Could not read GPS coordinates.".into(),
            Self::Platform(message) => message.clone(),
        }
    }
}

/// Current position. Call from the UI async context (not the RNS engine thread).
#[cfg(any(feature = "desktop", feature = "mobile"))]
pub async fn current_fix() -> Result<GeoPoint, LocationError> {
    match tokio::time::timeout(FIX_TIMEOUT, current_fix_inner()).await {
        Ok(result) => result,
        Err(_) => Err(LocationError::Timeout),
    }
}

#[cfg(all(any(feature = "desktop", feature = "mobile"), target_os = "macos"))]
async fn current_fix_inner() -> Result<GeoPoint, LocationError> {
    tokio::task::spawn_blocking(macos_fix_blocking)
        .await
        .map_err(|_| LocationError::Platform("Location worker failed.".into()))?
}

#[cfg(all(
    any(feature = "desktop", feature = "mobile"),
    not(target_os = "macos")
))]
async fn current_fix_inner() -> Result<GeoPoint, LocationError> {
    webview_fix().await
}

#[cfg(not(any(feature = "desktop", feature = "mobile")))]
pub async fn current_fix() -> Result<GeoPoint, LocationError> {
    Err(LocationError::Unsupported)
}

#[cfg(all(any(feature = "desktop", feature = "mobile"), target_os = "macos"))]
fn macos_fix_blocking() -> Result<GeoPoint, LocationError> {
    use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
    use std::time::{Duration, Instant};

    use core_foundation::base::TCFType;
    use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoopRunInMode};
    use core_foundation::string::CFString;
    use corelocation::prelude::*;
    use corelocation::manager::LOCATION_ACCURACY_HUNDRED_METERS;

    enum FixEvent {
        Location(Location),
        Error(LocationManagerErrorInfo),
        Auth(AuthorizationStatus),
    }

    if !LocationManager::location_services_enabled() {
        return Err(LocationError::Unavailable);
    }

    let (tx, rx) = mpsc::channel();
    let tx_err = tx.clone();
    let tx_auth = tx.clone();

    let callbacks = LocationManagerCallbacks::new()
        .on_locations(move |locations| {
            if let Some(location) = locations.into_iter().next() {
                let _ = tx.send(FixEvent::Location(location));
            }
        })
        .on_error(move |error| {
            let _ = tx_err.send(FixEvent::Error(error));
        })
        .on_authorization_change(move |status| {
            let _ = tx_auth.send(FixEvent::Auth(status));
        });

    let manager = LocationManager::with_callbacks(callbacks).map_err(|error| {
        LocationError::Platform(format!("CoreLocation manager: {error}"))
    })?;
    manager.set_desired_accuracy(LOCATION_ACCURACY_HUNDRED_METERS);

    match manager.authorization_status() {
        AuthorizationStatus::Denied | AuthorizationStatus::Restricted => {
            return Err(LocationError::Denied);
        }
        AuthorizationStatus::NotDetermined => {
            manager.request_when_in_use_authorization();
        }
        AuthorizationStatus::AuthorizedAlways | AuthorizationStatus::AuthorizedWhenInUse => {
            manager.request_location();
        }
    }

    let deadline = Instant::now() + FIX_TIMEOUT;
    let mode = unsafe { CFString::wrap_under_get_rule(kCFRunLoopDefaultMode) };

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(LocationError::Timeout);
        }

        // Keep CoreLocation's run-loop callbacks (and the OS permission sheet) alive.
        let _ = unsafe {
            CFRunLoopRunInMode(mode.as_concrete_TypeRef(), 0.05, false as _)
        };

        match rx.try_recv() {
            Ok(FixEvent::Location(location)) => {
                manager.stop_updating_location();
                return GeoPoint::try_new(
                    location.coordinate.latitude,
                    location.coordinate.longitude,
                )
                .map_err(|_| LocationError::BadPayload);
            }
            Ok(FixEvent::Error(error)) => {
                manager.stop_updating_location();
                return Err(map_corelocation_error(&error));
            }
            Ok(FixEvent::Auth(status)) => match status {
                AuthorizationStatus::AuthorizedAlways
                | AuthorizationStatus::AuthorizedWhenInUse => {
                    manager.request_location();
                }
                AuthorizationStatus::Denied | AuthorizationStatus::Restricted => {
                    return Err(LocationError::Denied);
                }
                AuthorizationStatus::NotDetermined => {}
            },
            Err(TryRecvError::Empty) => {
                // Brief park so we do not busy-spin; still pump above each loop.
                match rx.recv_timeout(Duration::from_millis(50).min(remaining)) {
                    Ok(event) => {
                        // Re-queue by handling via a one-step match using a tiny helper.
                        match event {
                            FixEvent::Location(location) => {
                                manager.stop_updating_location();
                                return GeoPoint::try_new(
                                    location.coordinate.latitude,
                                    location.coordinate.longitude,
                                )
                                .map_err(|_| LocationError::BadPayload);
                            }
                            FixEvent::Error(error) => {
                                manager.stop_updating_location();
                                return Err(map_corelocation_error(&error));
                            }
                            FixEvent::Auth(status) => match status {
                                AuthorizationStatus::AuthorizedAlways
                                | AuthorizationStatus::AuthorizedWhenInUse => {
                                    manager.request_location();
                                }
                                AuthorizationStatus::Denied
                                | AuthorizationStatus::Restricted => {
                                    return Err(LocationError::Denied);
                                }
                                AuthorizationStatus::NotDetermined => {}
                            },
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(LocationError::Platform(
                            "Location channel closed.".into(),
                        ));
                    }
                }
            }
            Err(TryRecvError::Disconnected) => {
                return Err(LocationError::Platform(
                    "Location channel closed.".into(),
                ));
            }
        }
    }
}

#[cfg(all(any(feature = "desktop", feature = "mobile"), target_os = "macos"))]
fn map_corelocation_error(error: &corelocation::LocationManagerErrorInfo) -> LocationError {
    let message = error.message.to_ascii_lowercase();
    if message.contains("denied") || message.contains("authorization") {
        LocationError::Denied
    } else if message.contains("timeout") {
        LocationError::Timeout
    } else {
        LocationError::Platform(error.message.clone())
    }
}

#[cfg(all(
    any(feature = "desktop", feature = "mobile"),
    not(target_os = "macos")
))]
async fn webview_fix() -> Result<GeoPoint, LocationError> {
    use dioxus::prelude::*;

    let timeout_ms = u32::try_from(FIX_TIMEOUT.as_millis()).unwrap_or(15_000);
    let mut eval = document::eval(
        r#"
        const timeoutMs = await dioxus.recv();
        try {
            if (!navigator.geolocation) {
                dioxus.send({ ok: false, error: "unsupported" });
            } else {
                await new Promise((resolve) => {
                    const timer = setTimeout(() => {
                        dioxus.send({ ok: false, error: "timeout" });
                        resolve();
                    }, timeoutMs);
                    navigator.geolocation.getCurrentPosition(
                        (pos) => {
                            clearTimeout(timer);
                            dioxus.send({
                                ok: true,
                                lat: pos.coords.latitude,
                                lon: pos.coords.longitude,
                            });
                            resolve();
                        },
                        (err) => {
                            clearTimeout(timer);
                            let code = "unavailable";
                            if (err && err.code === 1) code = "denied";
                            else if (err && err.code === 3) code = "timeout";
                            else if (err && err.code === 2) code = "unavailable";
                            dioxus.send({
                                ok: false,
                                error: code,
                                message: err && err.message ? String(err.message) : "",
                            });
                            resolve();
                        },
                        {
                            enableHighAccuracy: true,
                            timeout: timeoutMs,
                            maximumAge: 0,
                        }
                    );
                });
            }
        } catch (e) {
            dioxus.send({
                ok: false,
                error: "platform",
                message: e && e.message ? String(e.message) : String(e),
            });
        }
        "#,
    );

    eval.send(timeout_ms).map_err(|_| {
        LocationError::Platform("Failed to start geolocation request.".into())
    })?;

    #[derive(serde::Deserialize)]
    struct GeoPayload {
        ok: bool,
        #[serde(default)]
        lat: Option<f64>,
        #[serde(default)]
        lon: Option<f64>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        message: Option<String>,
    }

    let payload: GeoPayload = eval.recv().await.map_err(|_| LocationError::BadPayload)?;
    if !payload.ok {
        return Err(match payload.error.as_deref() {
            Some("unsupported") => LocationError::Unsupported,
            Some("denied") => LocationError::Denied,
            Some("timeout") => LocationError::Timeout,
            Some("unavailable") => LocationError::Unavailable,
            Some("platform") => LocationError::Platform(
                payload
                    .message
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "Geolocation failed.".into()),
            ),
            _ => LocationError::Unavailable,
        });
    }

    let lat = payload.lat.ok_or(LocationError::BadPayload)?;
    let lon = payload.lon.ok_or(LocationError::BadPayload)?;
    GeoPoint::try_new(lat, lon).map_err(|_| LocationError::BadPayload)
}
