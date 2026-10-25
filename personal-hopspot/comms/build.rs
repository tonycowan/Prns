use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    stamp_version();
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

/// Release builds show `v` plus the crate version. Everything else adds the commit.
fn stamp_version() {
    println!("cargo:rerun-if-env-changed=TOUCHBASE_RELEASE");
    watch_git_head();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let shown = if release_build() {
        format!("v{version}")
    } else {
        match commit_id() {
            Some(commit) => format!("v{version}+{commit}"),
            None => format!("v{version}"),
        }
    };
    println!("cargo:rustc-env=TOUCHBASE_VERSION={shown}");
}

fn release_build() -> bool {
    matches!(
        std::env::var("TOUCHBASE_RELEASE").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

fn watch_git_head() {
    let git = Path::new("../../.git");
    let head = git.join("HEAD");
    if !head.is_file() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());
    let Ok(text) = fs::read_to_string(&head) else {
        return;
    };
    if let Some(reference) = text.trim().strip_prefix("ref: ") {
        println!("cargo:rerun-if-changed={}", git.join(reference).display());
    }
}

fn commit_id() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    let sha = sha.trim();
    if sha.is_empty() {
        None
    } else {
        Some(sha.to_string())
    }
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
