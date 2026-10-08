use core::fmt::Write as _;

use embedded_graphics::mono_font::iso_8859_1::FONT_5X8;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::text::{Baseline, Text};
use heapless::String as HString;

#[cfg(test)]
use super::layout::NUMBER_GLYPH_WIDTH;

enum CompactQuantity {
    Count,
    Bytes,
}

fn fmt_compact_quantity(n: u64, quantity: CompactQuantity) -> HString<8> {
    let mut s = HString::new();
    let (unit, unit_value) = match quantity {
        CompactQuantity::Count if n < 1_000 => ("", 1),
        CompactQuantity::Count if n < 1_000_000 => ("K", 1_000),
        CompactQuantity::Count if n < 1_000_000_000 => ("M", 1_000_000),
        CompactQuantity::Count => ("B", 1_000_000_000),
        CompactQuantity::Bytes if n < 1_000 => ("B", 1),
        CompactQuantity::Bytes if n < 1_000_000 => ("K", 1_000),
        CompactQuantity::Bytes if n < 1_000_000_000 => ("M", 1_000_000),
        CompactQuantity::Bytes if n < 1_000_000_000_000 => ("G", 1_000_000_000),
        CompactQuantity::Bytes if n < 1_000_000_000_000_000 => ("T", 1_000_000_000_000),
        CompactQuantity::Bytes if n < 1_000_000_000_000_000_000 => ("P", 1_000_000_000_000_000),
        CompactQuantity::Bytes => ("E", 1_000_000_000_000_000_000),
    };

    if unit_value == 1 {
        let _ = write!(s, "{n}{unit}");
        return s;
    }

    let int_part = n / unit_value;
    if int_part < 10 {
        let tenths = n / (unit_value / 10);
        let _ = write!(s, "{}.{}{}", tenths / 10, tenths % 10, unit);
    } else {
        let _ = write!(s, "{int_part}{unit}");
    }
    s
}

pub(in crate::screen) fn fmt_bytes(n: u64) -> HString<8> {
    fmt_compact_quantity(n, CompactQuantity::Bytes)
}

pub(in crate::screen) fn fmt_count(n: u32) -> HString<8> {
    fmt_compact_quantity(u64::from(n), CompactQuantity::Count)
}

pub(in crate::screen) fn fmt_rate_bytes_per_sec(n: u32) -> HString<10> {
    let mut s = HString::new();
    let _ = write!(
        s,
        "{}/s",
        fmt_compact_quantity(u64::from(n), CompactQuantity::Bytes)
    );
    s
}

pub(in crate::screen) fn fmt_activity_age(age_secs: Option<u32>) -> HString<8> {
    let mut s = HString::new();
    match age_secs {
        None => {
            let _ = write!(s, "-");
        }
        Some(0) => {
            let _ = write!(s, "now");
        }
        Some(seconds) if seconds < 60 => {
            let _ = write!(s, "{seconds}s");
        }
        Some(seconds) if seconds < 3600 => {
            let _ = write!(s, "{}m", seconds / 60);
        }
        Some(seconds) => {
            let hours = (seconds / 3600).min(99);
            let _ = write!(s, "{hours}h");
        }
    }
    s
}

#[cfg(test)]
pub(in crate::screen) fn compact_numeric_width(text: &str) -> i32 {
    text.chars().count() as i32 * NUMBER_GLYPH_WIDTH
}

/// One text run, same as the status words on the card. Per-glyph draws were falling back to `?`.
pub(in crate::screen) fn draw_compact_number<D: DrawTarget<Color = BinaryColor>>(
    display: &mut D,
    text: &str,
    point: Point,
    color: BinaryColor,
) {
    let style = MonoTextStyle::new(&FONT_5X8, color);
    let _ = Text::with_baseline(text, point, style, Baseline::Top).draw(display);
}
