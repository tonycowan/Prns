//! Platform sleep for Dioxus futures (gloo is wasm-only).

use std::time::Duration;

pub async fn sleep_ms(ms: u64) {
    #[cfg(target_arch = "wasm32")]
    {
        gloo_timers::future::TimeoutFuture::new(ms as u32).await;
    }
    #[cfg(all(not(target_arch = "wasm32"), feature = "live"))]
    {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    #[cfg(all(not(target_arch = "wasm32"), not(feature = "live")))]
    {
        // Desktop/mobile always enable `live`; this branch is for odd feature combos.
        let _ = ms;
        std::future::ready(()).await;
    }
}

/// Local date/time for chat bubbles, e.g. "Aug 22, 7:33 AM".
pub fn format_message_time() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_local(secs)
}

fn format_unix_local(secs: u64) -> String {
    #[cfg(unix)]
    {
        format_unix_local_unix(secs)
    }
    #[cfg(not(unix))]
    {
        format_unix_utc(secs)
    }
}

#[cfg(unix)]
fn format_unix_local_unix(secs: u64) -> String {
    extern "C" {
        fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
    }

    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
    }

    let mut tm = Tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
    };
    let secs_i64 = secs as i64;
    unsafe {
        if localtime_r(&secs_i64, &mut tm).is_null() {
            return format_unix_utc(secs);
        }
    }

    let month = MONTHS
        .get(tm.tm_mon as usize)
        .copied()
        .unwrap_or("???");
    let hour = tm.tm_hour.rem_euclid(24);
    let minute = tm.tm_min;
    let ampm = if hour < 12 { "AM" } else { "PM" };
    let hour12 = match hour {
        0 => 12,
        13..=23 => hour - 12,
        other => other,
    };
    format!(
        "{month} {}, {hour12}:{minute:02} {ampm}",
        tm.tm_mday,
    )
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn format_unix_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let day_secs = secs % 86_400;
    let hour = (day_secs / 3_600) as u32;
    let minute = ((day_secs % 3_600) / 60) as u32;
    let (_year, month, day) = civil_from_days(days as i64);
    let ampm = if hour < 12 { "AM" } else { "PM" };
    let hour12 = match hour {
        0 => 12,
        13..=23 => hour - 12,
        other => other,
    };
    format!(
        "{month} {day}, {hour12}:{minute:02} {ampm} UTC",
        month = MONTHS.get((month - 1) as usize).copied().unwrap_or("???"),
    )
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}
