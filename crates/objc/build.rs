// Bake libclang's directory into the binary's RPATH so it loads at runtime
// without requiring DYLD_LIBRARY_PATH.  Only needed on macOS; on other
// platforms the `cfg(target_os = "macos")` guard on clang-sys means we never
// link against libclang at all.
#[cfg(target_os = "macos")]
fn main() {
    // Priority order:
    //   1. LIBCLANG_PATH env var (user/CI override)
    //   2. xcrun --find clang  → walk up to <toolchain>/usr/lib
    //   3. Xcode.app toolchain (users with full Xcode instead of CLT)
    //   4. Homebrew LLVM (common dev machine alternative)
    if let Some(dir) = libclang_dir() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn libclang_dir() -> Option<String> {
    // 1. Explicit env override — respect it unconditionally.
    if let Ok(p) = std::env::var("LIBCLANG_PATH") {
        let dir = std::path::Path::new(&p);
        let dir = if dir.is_file() {
            dir.parent()?.to_path_buf()
        } else {
            dir.to_path_buf()
        };
        if dir.join("libclang.dylib").exists() {
            return Some(dir.to_string_lossy().into_owned());
        }
    }

    // 2. xcrun: finds whichever clang is active (CLT or Xcode.app).
    if let Some(dir) = xcrun_libclang_dir() {
        return Some(dir);
    }

    // 3. Well-known fallbacks.
    let candidates = [
        // Xcode.app default toolchain
        "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib",
        // Command Line Tools
        "/Library/Developer/CommandLineTools/usr/lib",
        // Homebrew LLVM (Apple Silicon)
        "/opt/homebrew/opt/llvm/lib",
        // Homebrew LLVM (Intel)
        "/usr/local/opt/llvm/lib",
    ];
    for candidate in candidates {
        let p = std::path::Path::new(candidate);
        if p.join("libclang.dylib").exists() {
            return Some(candidate.to_string());
        }
    }

    None
}

#[cfg(target_os = "macos")]
fn xcrun_libclang_dir() -> Option<String> {
    let out = std::process::Command::new("xcrun")
        .args(["--find", "clang"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // clang is at <toolchain>/usr/bin/clang → lib is at <toolchain>/usr/lib
    let clang_path = std::str::from_utf8(&out.stdout).ok()?.trim();
    let lib_dir = std::path::Path::new(clang_path)
        .parent()? // usr/bin
        .parent()? // usr
        .join("lib");
    if lib_dir.join("libclang.dylib").exists() {
        Some(lib_dir.to_string_lossy().into_owned())
    } else {
        None
    }
}
