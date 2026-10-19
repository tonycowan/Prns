//! Read a contact out of a photo of their code.
//!
//! The photo is the same mark shown on Your QR code: the contact's name,
//! then their identity public key, one field per line.

use image::{DynamicImage, GrayImage, ImageBuffer, Luma};
use qrcode::types::Color;
use qrcode::QrCode;

pub fn contact_from_code(payload: &str) -> Option<(String, String)> {
    let mut lines = payload.lines();
    let name = lines.next()?.trim();
    let identity = lines.next()?.trim();
    if name.is_empty() || identity.is_empty() || lines.next().is_some() {
        return None;
    }
    Some((name.to_string(), identity.to_string()))
}

pub fn contact_from_image(bytes: &[u8]) -> Option<(String, String)> {
    let image = image::load_from_memory(bytes).ok()?;
    contact_from_luma(&fit_for_scan(image))
}

/// A camera photo is several thousand pixels on a side. Searching that for a
/// code takes long enough to freeze the page, and the code is still readable
/// after the long edge is brought down to this.
const SCAN_EDGE: u32 = 1600;

fn fit_for_scan(image: DynamicImage) -> GrayImage {
    let longest = image.width().max(image.height());
    if longest <= SCAN_EDGE {
        return image.into_luma8();
    }
    let width =
        (u64::from(image.width()) * u64::from(SCAN_EDGE) / u64::from(longest)).max(1) as u32;
    let height =
        (u64::from(image.height()) * u64::from(SCAN_EDGE) / u64::from(longest)).max(1) as u32;
    image.thumbnail(width, height).into_luma8()
}

pub fn contact_from_luma(image: &GrayImage) -> Option<(String, String)> {
    let mut prepared = rqrr::PreparedImage::prepare(image.clone());
    let grids = prepared.detect_grids();
    let (_, content) = grids.first()?.decode().ok()?;
    contact_from_code(&content)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn contact_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<(String, String)> {
    let image = ImageBuffer::from_raw(width, height, rgba.to_vec())?;
    contact_from_luma(&DynamicImage::ImageRgba8(image).to_luma8())
}

#[cfg(test)]
pub fn code_png(payload: &str) -> Option<Vec<u8>> {
    let code = QrCode::new(payload.as_bytes()).ok()?;
    let modules = code.width();
    let scale = 8u32;
    let quiet = 4 * scale;
    let size = modules as u32 * scale + quiet * 2;
    let mut image: GrayImage = ImageBuffer::from_pixel(size, size, Luma([255]));
    for y in 0..modules {
        for x in 0..modules {
            if code[(x, y)] != Color::Dark {
                continue;
            }
            let origin_x = quiet + x as u32 * scale;
            let origin_y = quiet + y as u32 * scale;
            for dy in 0..scale {
                for dx in 0..scale {
                    image.put_pixel(origin_x + dx, origin_y + dy, Luma([0]));
                }
            }
        }
    }
    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    image::ImageEncoder::write_image(
        encoder,
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::L8,
    )
    .ok()?;
    Some(png)
}

#[cfg(test)]
mod tests {
    use super::{code_png, contact_from_code, contact_from_image, contact_from_luma};
    use crate::model::contact_code;

    #[test]
    fn a_photo_of_a_code_is_the_name_and_the_identity() {
        let payload = contact_code("Mara", "19ab44c0e8d15f2076c3aa91b0d4e6f2");
        let png = code_png(&payload).expect("the mark encodes");
        let (name, identity) = contact_from_image(&png).expect("the photo reads back");
        assert_eq!(name, "Mara");
        assert_eq!(identity, "19ab44c0e8d15f2076c3aa91b0d4e6f2");
    }

    #[test]
    fn a_blank_photo_is_not_a_contact() {
        let image = image::GrayImage::from_pixel(64, 64, image::Luma([255]));
        assert!(contact_from_luma(&image).is_none());
        assert!(contact_from_image(&[0, 1, 2, 3]).is_none());
    }

    #[test]
    fn a_code_needs_both_lines() {
        assert!(contact_from_code("Mara").is_none());
        assert!(contact_from_code("\n19ab").is_none());
        assert_eq!(
            contact_from_code(" Mara \n 19ab \n"),
            Some(("Mara".into(), "19ab".into()))
        );
    }
}
