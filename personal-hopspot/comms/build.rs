use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    stamp_version();
    link_picture();
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
    let _ = fs::copy(Path::new("android/Film.kt"), kotlin_dir.join("Film.kt"));
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

/// Mac links the picture codec. Android uses the phone's own encoder instead.
fn link_picture() {
    if std::env::var("CARGO_CFG_TARGET_OS").ok().as_deref() != Some("macos") {
        return;
    }
    println!("cargo:rerun-if-changed=src/vpx_shim.c");
    let pkg = pkg_config_bin();
    let cflags = pkg_output(&pkg, &["--cflags", "vpx"]);
    let libs = pkg_output(&pkg, &["--libs", "--static", "vpx"]);
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let obj = out.join("vpx_shim.o");
    let archive = out.join("libn10a_vpx.a");
    let sdk = macos_sdk();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "arm64".to_string());
    let mut compile = Command::new("cc");
    compile.env_remove("IPHONEOS_DEPLOYMENT_TARGET");
    compile.env("SDKROOT", &sdk);
    compile.args([
        "-c",
        "-O2",
        "-target",
        &format!("{arch}-apple-macosx11.0"),
        "-isysroot",
    ]);
    compile.arg(&sdk);
    compile.arg("-o");
    compile.arg(&obj);
    compile.arg("src/vpx_shim.c");
    for flag in cflags.split_whitespace() {
        compile.arg(flag);
    }
    let compiled = compile.status().expect("cc");
    if !compiled.success() {
        panic!("the picture codec did not compile");
    }
    let archived = Command::new("ar")
        .args(["crus"])
        .arg(&archive)
        .arg(&obj)
        .status()
        .expect("ar");
    if !archived.success() {
        panic!("the picture codec archive was not written");
    }
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=n10a_vpx");
    for flag in libs.split_whitespace() {
        if let Some(dir) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={dir}");
        } else if let Some(lib) = flag.strip_prefix("-l") {
            if lib == "vpx" {
                println!("cargo:rustc-link-lib=static=vpx");
            } else {
                println!("cargo:rustc-link-lib={lib}");
            }
        }
    }
}

fn macos_sdk() -> String {
    let output = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .expect("xcrun");
    if !output.status.success() {
        panic!("the Mac SDK was not found");
    }
    String::from_utf8(output.stdout)
        .expect("sdk path")
        .trim()
        .to_string()
}

fn pkg_config_bin() -> String {
    for name in [
        "/opt/homebrew/bin/pkg-config",
        "/usr/local/bin/pkg-config",
        "pkg-config",
    ] {
        let Ok(status) = Command::new(name).args(["--exists", "vpx"]).status() else {
            continue;
        };
        if status.success() {
            return name.to_string();
        }
    }
    panic!("libvpx was not found. Install it before building the Mac journal.");
}

fn pkg_output(pkg: &str, args: &[&str]) -> String {
    let output = Command::new(pkg).args(args).output().expect("pkg-config");
    if !output.status.success() {
        panic!("pkg-config vpx failed");
    }
    String::from_utf8(output.stdout).expect("pkg-config output")
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
