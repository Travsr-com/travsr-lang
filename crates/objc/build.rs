// travsr-lang#17: libclang is now loaded dynamically at runtime (clang-sys
// `runtime` feature — see Cargo.toml and src/main.rs), so the binary no longer
// links against libclang.dylib and needs no baked-in RPATH.
//
// The previous build script emitted `-Wl,-rpath,<build-host libclang dir>`,
// which hard-coded the CI machine's Xcode path (e.g. `/Applications/
// Xcode_26.6.app/...`) into every release binary. On any target machine without
// that exact path, dyld aborted the process before `main()` ran, and the host
// daemon only saw a truncated pipe. Resolving libclang at runtime against the
// active toolchain (`LIBCLANG_PATH`, then `xcrun`, then well-known dirs) fixes
// that at the root, so this build script is intentionally a no-op.
fn main() {}
