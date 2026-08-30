//! In-band range-check protocol for Personal Text.
//!
//! One-shot request: `Range check (lat, lon)`
//! Auto start: `Auto range check (lat, lon)`
//! Cycle pings reuse one-shot requests while a session is active.
//! Reply: `(lat, lon) - <distance>`
//! Stop: bare `stop` (any capitalization) ends an auto session.

/// Phrase that triggers a one-shot range check (compared case-insensitively when trimmed).
pub const RANGE_CHECK_PHRASE: &str = "Range check";
/// Phrase that starts a continuous auto range-check session.
pub const AUTO_RANGE_CHECK_PHRASE: &str = "Auto range check";
/// Phrase that stops an auto range-check session.
pub const STOP_PHRASE: &str = "stop";
/// Pause after a completed auto cycle ping before starting the next one.
pub const AUTO_RANGE_INTERVAL_MS: u64 = 10_000;

const EARTH_RADIUS_M: f64 = 6_371_000.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoPoint {
    pub latitude: f64,
    pub longitude: f64,
}

impl GeoPoint {
    pub fn try_new(latitude: f64, longitude: f64) -> Result<Self, RangeCheckParseError> {
        if !(-90.0..=90.0).contains(&latitude) {
            return Err(RangeCheckParseError::LatitudeOutOfRange);
        }
        if !(-180.0..=180.0).contains(&longitude) {
            return Err(RangeCheckParseError::LongitudeOutOfRange);
        }
        if !latitude.is_finite() || !longitude.is_finite() {
            return Err(RangeCheckParseError::NonFinite);
        }
        Ok(Self {
            latitude,
            longitude,
        })
    }

    pub fn format_pair(self) -> String {
        format!("({:.6}, {:.6})", self.latitude, self.longitude)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeCheckParseError {
    NotARequest,
    MissingCoordinates,
    BadCoordinateFormat,
    LatitudeOutOfRange,
    LongitudeOutOfRange,
    NonFinite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeRequestKind {
    OneShot,
    Auto,
}

/// True when trimmed text is exactly the one-shot trigger phrase (any capitalization).
pub fn is_bare_range_check(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case(RANGE_CHECK_PHRASE)
}

/// True when trimmed text is exactly the auto trigger phrase (any capitalization).
pub fn is_bare_auto_range_check(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case(AUTO_RANGE_CHECK_PHRASE)
}

/// True when trimmed text is exactly `stop` (any capitalization).
pub fn is_stop(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case(STOP_PHRASE)
}

/// Format an outbound one-shot range-check request with the sender's GPS.
pub fn format_request(point: GeoPoint) -> String {
    format!("{RANGE_CHECK_PHRASE} {}", point.format_pair())
}

/// Format an outbound auto-session start request with the sender's GPS.
pub fn format_auto_request(point: GeoPoint) -> String {
    format!("{AUTO_RANGE_CHECK_PHRASE} {}", point.format_pair())
}

/// Parse a range-check request (one-shot or auto), including coordinates.
pub fn parse_request(text: &str) -> Result<(RangeRequestKind, GeoPoint), RangeCheckParseError> {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();

    let auto_prefix = AUTO_RANGE_CHECK_PHRASE.to_ascii_lowercase();
    if lower.starts_with(&auto_prefix) {
        let rest = trimmed[AUTO_RANGE_CHECK_PHRASE.len()..].trim_start();
        if rest.is_empty() {
            return Err(RangeCheckParseError::MissingCoordinates);
        }
        return Ok((RangeRequestKind::Auto, parse_coord_pair(rest)?));
    }

    let oneshot_prefix = RANGE_CHECK_PHRASE.to_ascii_lowercase();
    if lower.starts_with(&oneshot_prefix) {
        let rest = trimmed[RANGE_CHECK_PHRASE.len()..].trim_start();
        if rest.is_empty() {
            return Err(RangeCheckParseError::MissingCoordinates);
        }
        return Ok((RangeRequestKind::OneShot, parse_coord_pair(rest)?));
    }

    Err(RangeCheckParseError::NotARequest)
}

/// Format the single reply: `(lat, lon) - <distance>`.
pub fn format_reply(own: GeoPoint, peer: GeoPoint) -> String {
    let meters = haversine_meters(own, peer);
    format!("{} - {}", own.format_pair(), format_distance(meters))
}

pub fn haversine_meters(a: GeoPoint, b: GeoPoint) -> f64 {
    let lat1 = a.latitude.to_radians();
    let lat2 = b.latitude.to_radians();
    let d_lat = (b.latitude - a.latitude).to_radians();
    let d_lon = (b.longitude - a.longitude).to_radians();

    let sin_d_lat = (d_lat / 2.0).sin();
    let sin_d_lon = (d_lon / 2.0).sin();
    let h = sin_d_lat * sin_d_lat + lat1.cos() * lat2.cos() * sin_d_lon * sin_d_lon;
    let c = 2.0 * h.sqrt().atan2((1.0 - h).sqrt().max(0.0));
    EARTH_RADIUS_M * c
}

pub fn format_distance(meters: f64) -> String {
    let meters = meters.max(0.0);
    if meters >= 1000.0 {
        format!("{:.1} km", meters / 1000.0)
    } else {
        format!("{} m", meters.round() as u64)
    }
}

fn parse_coord_pair(text: &str) -> Result<GeoPoint, RangeCheckParseError> {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or(RangeCheckParseError::BadCoordinateFormat)?
        .trim();
    let (lat_s, lon_s) = inner
        .split_once(',')
        .ok_or(RangeCheckParseError::BadCoordinateFormat)?;
    let latitude = lat_s
        .trim()
        .parse::<f64>()
        .map_err(|_| RangeCheckParseError::BadCoordinateFormat)?;
    let longitude = lon_s
        .trim()
        .parse::<f64>()
        .map_err(|_| RangeCheckParseError::BadCoordinateFormat)?;
    GeoPoint::try_new(latitude, longitude)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_phrase_any_case() {
        assert!(is_bare_range_check("Range check"));
        assert!(is_bare_range_check("RANGE CHECK"));
        assert!(is_bare_range_check("  range check  "));
        assert!(!is_bare_range_check("Range check please"));
        assert!(!is_bare_range_check("hello"));
        assert!(is_bare_auto_range_check("Auto range check"));
        assert!(is_bare_auto_range_check("AUTO RANGE CHECK"));
        assert!(!is_bare_auto_range_check("Range check"));
        assert!(is_stop("stop"));
        assert!(is_stop("STOP"));
        assert!(!is_stop("stop please"));
    }

    #[test]
    fn request_round_trip() {
        let point = GeoPoint::try_new(40.712800, -74.006000).unwrap();
        let text = format_request(point);
        assert_eq!(text, "Range check (40.712800, -74.006000)");
        let (kind, parsed) = parse_request(&text).unwrap();
        assert_eq!(kind, RangeRequestKind::OneShot);
        assert!((parsed.latitude - point.latitude).abs() < 1e-9);
        assert!((parsed.longitude - point.longitude).abs() < 1e-9);
    }

    #[test]
    fn auto_request_round_trip() {
        let point = GeoPoint::try_new(1.5, -2.25).unwrap();
        let text = format_auto_request(point);
        assert_eq!(text, "Auto range check (1.500000, -2.250000)");
        let (kind, parsed) = parse_request(&text).unwrap();
        assert_eq!(kind, RangeRequestKind::Auto);
        assert!((parsed.latitude - 1.5).abs() < 1e-9);
        assert!((parsed.longitude - (-2.25)).abs() < 1e-9);
    }

    #[test]
    fn parse_request_case_insensitive_phrase() {
        let (kind, parsed) = parse_request("range CHECK (1.5, -2.25)").unwrap();
        assert_eq!(kind, RangeRequestKind::OneShot);
        assert!((parsed.latitude - 1.5).abs() < 1e-9);
        assert!((parsed.longitude - (-2.25)).abs() < 1e-9);
    }

    #[test]
    fn auto_prefix_wins_over_oneshot_prefix() {
        // "auto range check" also starts with letters that could confuse naive matching;
        // ensure the longer auto phrase is recognized first.
        let (kind, _) = parse_request("Auto range check (0.0, 0.0)").unwrap();
        assert_eq!(kind, RangeRequestKind::Auto);
    }

    #[test]
    fn reply_format_uses_km_or_meters() {
        let a = GeoPoint::try_new(40.0, -74.0).unwrap();
        let nearby = GeoPoint::try_new(40.001, -74.0).unwrap();
        let reply = format_reply(a, nearby);
        assert!(reply.starts_with("(40.000000, -74.000000) - "));
        assert!(reply.ends_with(" m") || reply.contains(" km"));

        let far = GeoPoint::try_new(41.0, -74.0).unwrap();
        let far_reply = format_reply(a, far);
        assert!(far_reply.contains(" km"));
    }

    #[test]
    fn haversine_nyc_sanity() {
        let empire = GeoPoint::try_new(40.7484, -73.9857).unwrap();
        let wtc = GeoPoint::try_new(40.7127, -74.0134).unwrap();
        let meters = haversine_meters(empire, wtc);
        assert!(meters > 4_000.0 && meters < 7_000.0, "got {meters}");
    }

    #[test]
    fn rejects_out_of_range() {
        assert_eq!(
            GeoPoint::try_new(91.0, 0.0),
            Err(RangeCheckParseError::LatitudeOutOfRange)
        );
        assert_eq!(
            parse_request("Range check"),
            Err(RangeCheckParseError::MissingCoordinates)
        );
    }
}
