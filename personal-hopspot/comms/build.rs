use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=android");
    let Ok(kotlin_dir) = std::env::var("WRY_ANDROID_KOTLIN_FILES_OUT_DIR") else {
        return;
    };
    let kotlin_dir = PathBuf::from(kotlin_dir);
    let _ = fs::copy(
        Path::new("android/WifiLan.kt"),
        kotlin_dir.join("WifiLan.kt"),
    );
    let _ = fs::copy(
        Path::new("android/CodeScan.kt"),
        kotlin_dir.join("CodeScan.kt"),
    );
    if let Some(kotlin_root) = kotlin_dir.ancestors().nth(3) {
        let controller = kotlin_root.join("org/personal/prns/controller");
        let _ = fs::create_dir_all(&controller);
        for name in ["BleLink.kt", "UsbLink.kt", "NativeBridge.kt"] {
            let _ = fs::copy(
                Path::new("android/org/personal/prns/controller").join(name),
                controller.join(name),
            );
        }
    }
    let Some(app_main) = kotlin_dir.ancestors().nth(4) else {
        return;
    };
    let xml = app_main.join("res").join("xml");
    let _ = fs::create_dir_all(&xml);
    let _ = fs::copy(
        Path::new("android/res/xml/file_paths.xml"),
        xml.join("file_paths.xml"),
    );
    let _ = fs::copy(
        Path::new("android/res/xml/usb_device_filter.xml"),
        xml.join("usb_device_filter.xml"),
    );
    let _ = fs::copy(
        Path::new("android/res/xml/usb_accessory_filter.xml"),
        xml.join("usb_accessory_filter.xml"),
    );
    let manifest = app_main.join("AndroidManifest.xml");
    ensure_photo_provider(&manifest);
    ensure_usb_attachment(&manifest);
}

fn ensure_photo_provider(manifest: &Path) {
    let Ok(text) = fs::read_to_string(manifest) else {
        return;
    };
    if text.contains("androidx.core.content.FileProvider") {
        return;
    }
    let provider = r#"
        <provider
            android:name="androidx.core.content.FileProvider"
            android:authorities="org.personal.prns.comms.fileprovider"
            android:exported="false"
            android:grantUriPermissions="true">
            <meta-data
                android:name="android.support.FILE_PROVIDER_PATHS"
                android:resource="@xml/file_paths" />
        </provider>
"#;
    if !text.contains("</application>") {
        return;
    }
    let updated = text.replacen(
        "</application>",
        &format!("{provider}    </application>"),
        1,
    );
    let _ = fs::write(manifest, updated);
}

fn ensure_usb_attachment(manifest: &Path) {
    let Ok(text) = fs::read_to_string(manifest) else {
        return;
    };
    if text.contains("USB_DEVICE_ATTACHED") {
        return;
    }
    let filters = r#"
            <intent-filter>
                <action android:name="android.hardware.usb.action.USB_DEVICE_ATTACHED" />
            </intent-filter>
            <intent-filter>
                <action android:name="android.hardware.usb.action.USB_ACCESSORY_ATTACHED" />
            </intent-filter>
            <meta-data
                android:name="android.hardware.usb.action.USB_DEVICE_ATTACHED"
                android:resource="@xml/usb_device_filter" />
            <meta-data
                android:name="android.hardware.usb.action.USB_ACCESSORY_ATTACHED"
                android:resource="@xml/usb_accessory_filter" />
"#;
    if !text.contains("</activity>") {
        return;
    }
    let updated = text.replacen("</activity>", &format!("{filters}        </activity>"), 1);
    let _ = fs::write(manifest, updated);
}
