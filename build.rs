//! Windows: put the app icon (Explorer, taskbar pins, shortcuts) and the
//! version info (file properties, SmartScreen, code signing) into ytmini.exe.
//! The window icon itself is set at runtime in main.rs.
//! Needs `llvm-rc` (LLVM, also used when cross-compiling with cargo-xwin) or
//! Windows SDK's `rc.exe`; without either the exe just has no icon.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=Cargo.toml");
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
    std::fs::write(&rc, format!("1 ICON \"{icon_path}\"\n{}", version_info())).unwrap();

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
    // releases set YTMINI_REQUIRE_ICON: no icon there is a broken build
    println!("cargo:rerun-if-env-changed=YTMINI_REQUIRE_ICON");
    if std::env::var_os("YTMINI_REQUIRE_ICON").is_some() {
        panic!("no resource compiler (llvm-rc / rc.exe) for the exe icon");
    }
    println!("cargo:warning=no resource compiler (llvm-rc / rc.exe): ytmini.exe gets no icon");
}

/// A VERSIONINFO resource from Cargo.toml. Numeric flags only: the .rc is
/// compiled without the preprocessor, so winver.h names aren't available.
fn version_info() -> String {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut n = version
        .split(['.', '-', '+'])
        .map(|p| p.parse::<u16>().unwrap_or(0))
        .chain(std::iter::repeat(0));
    let (a, b, c) = (n.next().unwrap(), n.next().unwrap(), n.next().unwrap());
    let desc = std::env::var("CARGO_PKG_DESCRIPTION").unwrap_or_default();
    format!(
        r#"1 VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
FILEFLAGSMASK 0x3F
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "The ytmini authors"
      VALUE "FileDescription", "ytmini"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "ytmini"
      VALUE "LegalCopyright", "The ytmini authors (MIT)"
      VALUE "OriginalFilename", "ytmini.exe"
      VALUE "ProductName", "ytmini"
      VALUE "ProductVersion", "{version}"
      VALUE "Comments", "{desc}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    )
}
