//! Live frames from the phone camera. Each frame is already the size of a code scan.

use image::GrayImage;
use jni::objects::{JByteArray, JClass};
use jni::sys::{jint, jstring};
use jni::JNIEnv;

use crate::scan::contact_from_luma;

#[no_mangle]
pub extern "system" fn Java_dev_dioxus_main_CodeScan_nativeReadCode(
    mut env: JNIEnv,
    _class: JClass,
    width: jint,
    height: jint,
    luma: JByteArray,
) -> jstring {
    let Some(text) = read_code(&mut env, width, height, luma) else {
        return std::ptr::null_mut();
    };
    env.new_string(text)
        .map(|value| value.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

fn read_code(env: &mut JNIEnv, width: jint, height: jint, luma: JByteArray) -> Option<String> {
    let width = u32::try_from(width).ok()?;
    let height = u32::try_from(height).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let bytes = env.convert_byte_array(luma).ok()?;
    if bytes.len() != (width as usize).checked_mul(height as usize)? {
        return None;
    }
    let image = GrayImage::from_raw(width, height, bytes)?;
    let (name, identity) = contact_from_luma(&image)?;
    Some(format!("{name}\n{identity}"))
}
