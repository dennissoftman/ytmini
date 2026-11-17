//! Windows: put the app icon into ytmini.exe (Explorer, taskbar pins,
//! shortcuts). The window icon itself is set at runtime in main.rs.
//! Needs `llvm-rc` (LLVM, also used when cross-compiling with cargo-xwin) or
//! Windows SDK's `rc.exe`; without either the exe just has no icon.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let icon = manifest.join("assets").join("icon.ico");
    let rc = out.join("ytmini.rc");
    let res = out.join("ytmini.res");
    // forward slashes: rc treats backslashes in strings as escapes
    let icon_path = icon.to_string_lossy().replace('\\', "/");
    std::fs::write(&rc, format!("1 ICON \"{icon_path}\"\n")).unwrap();

    // llvm-rc: our .rc has no #includes, so skip its clang preprocessing
    let llvm = ["-no-preprocess"].as_slice();
    let mut tools: Vec<(PathBuf, &[&str])> = Vec::new();
    if let Some(t) = std::env::var_os("LLVM_RC") {
        tools.push((t.into(), llvm));
    }
    tools.push(("llvm-rc".into(), llvm));
    tools.push(("/opt/homebrew/opt/llvm/bin/llvm-rc".into(), llvm));
    tools.push(("/usr/local/opt/llvm/bin/llvm-rc".into(), llvm));
    tools.push(("rc".into(), &[]));
    for (tool, extra) in tools {
        let ok = Command::new(&tool)
            .args(extra)
            .arg("/nologo")
            // relative names: llvm-rc reads "/Users/…" as an option
            .current_dir(&out)
            .args(["/fo", "ytmini.res", "ytmini.rc"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            println!("cargo:rustc-link-arg-bins={}", res.display());
            return;
        }
    }
    println!("cargo:warning=no resource compiler (llvm-rc / rc.exe): ytmini.exe gets no icon");
}
