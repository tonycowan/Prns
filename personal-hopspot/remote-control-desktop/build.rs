use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=android");
    let Ok(kotlin_dir) = env::var("WRY_ANDROID_KOTLIN_FILES_OUT_DIR") else {
        return;
    };
    let kotlin_dir = PathBuf::from(kotlin_dir);
    let app_main = kotlin_dir
        .ancestors()
        .nth(4)
        .expect("WRY kotlin dir is under app/src/main/kotlin");
    copy_tree(Path::new("android/src"), &app_main.join("java"));
    // Overlay launcher icons and USB filters onto the Dioxus-generated res tree.
    let res = app_main.join("res");
    copy_tree(Path::new("android/res"), &res);
    // Drop the stock adaptive-icon vector so our PNG foreground wins.
    let _ = fs::remove_file(res.join("drawable-v24").join("ic_launcher_foreground.xml"));
    // Dioxus also emits ic_launcher.webp from [bundle].icon; our PNG overlays
    // would otherwise collide as duplicate mipmap resources.
    remove_dioxus_launcher_webps(&res);
}

fn remove_dioxus_launcher_webps(res: &Path) {
    let Ok(entries) = fs::read_dir(res) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("mipmap-") {
            continue;
        }
        let webp = entry.path().join("ic_launcher.webp");
        let _ = fs::remove_file(webp);
    }
}

fn copy_tree(src: &Path, dst: &Path) {
    if !src.exists() {
        return;
    }
    for entry in fs::read_dir(src).into_iter().flatten().flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            let _ = fs::create_dir_all(&to);
            copy_tree(&from, &to);
        } else if let Some(parent) = to.parent() {
            let _ = fs::create_dir_all(parent);
            let _ = fs::copy(&from, &to);
        }
    }
}
