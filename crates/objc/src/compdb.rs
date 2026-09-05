#![cfg(target_os = "macos")]
//! Compilation database discovery and fallback for ObjC Phase B.
//!
//! Priority:
//!   1. `compile_commands.json` in the repo root (CMake / Bear / compiledb)
//!   2. Glob-walk of `.m` / `.mm` files with conservative default flags

use std::path::{Path, PathBuf};

pub struct CompilationEntry {
    pub file: PathBuf,
    /// Args passed to libclang (everything after the implicit source filename).
    pub args: Vec<String>,
}

/// Discover compilation entries for the given repo root.
///
/// Prefers a `compile_commands.json` when present; falls back to a glob walk
/// that constructs minimal flags sufficient for structural ObjC analysis.
pub fn discover(root: &Path, files: Option<&[String]>) -> Vec<CompilationEntry> {
    if let Some(entries) = try_compile_commands(root) {
        return entries;
    }
    glob_fallback(root, files)
}

// ── compile_commands.json ─────────────────────────────────────────────────────

fn try_compile_commands(root: &Path) -> Option<Vec<CompilationEntry>> {
    let path = root.join("compile_commands.json");
    if !path.exists() {
        return None;
    }

    let text = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let arr = json.as_array()?;

    let mut entries = Vec::new();
    for item in arr {
        let file_str = item["file"].as_str()?;
        let file = PathBuf::from(file_str);

        // Only parse ObjC / ObjC++ files directly; headers are #import'd.
        let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "m" | "mm") {
            continue;
        }

        let dir = item["directory"].as_str().unwrap_or("");
        let file = if file.is_absolute() {
            file
        } else {
            PathBuf::from(dir).join(&file)
        };

        let args = extract_args(item, &file);
        entries.push(CompilationEntry { file, args });
    }

    if entries.is_empty() {
        None
    } else {
        tracing::debug!(
            entries = entries.len(),
            path = %path.display(),
            "compile_commands: loaded entries"
        );
        Some(entries)
    }
}

/// Extract the compiler arguments from a compile_commands entry.
/// Accepts both the `command` (shell string) and `arguments` (array) forms.
fn extract_args(item: &serde_json::Value, file: &Path) -> Vec<String> {
    let file_str = file.to_string_lossy();

    if let Some(arr) = item["arguments"].as_array() {
        // Array form: skip the compiler executable and the source filename itself.
        return arr
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| *s != file_str)
            .skip(1) // skip compiler binary (first element)
            .map(|s| s.to_string())
            .collect();
    }

    if let Some(cmd) = item["command"].as_str() {
        // Shell string: naïve whitespace split; good enough for typical CMake output.
        return cmd
            .split_whitespace()
            .filter(|s| *s != file_str.as_ref())
            .skip(1) // skip compiler binary
            .map(|s| s.to_string())
            .collect();
    }

    Vec::new()
}

// ── glob fallback ─────────────────────────────────────────────────────────────

fn glob_fallback(root: &Path, files: Option<&[String]>) -> Vec<CompilationEntry> {
    let sdk = sdk_path();

    // One traversal collects both the `.m`/`.mm` sources to index and every
    // directory that holds a header. #831: the pre-existing single `-I<root>`
    // could not resolve a quoted `#import "Foo.h"` when `Foo.h` lives in a
    // subdirectory (the common project layout: sources in `Tests/`, headers in
    // `Foo/`). An unresolved import leaves the imported class *undeclared*, and
    // clang then drops the whole `[Foo bar]` send from the AST, so no cursor
    // walk can recover its ref. Adding every header directory to the search
    // path lets those imports resolve so the sends parse as real message exprs.
    let mut sources = Vec::new();
    let mut header_dirs: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    // The root is always a search path (matches the previous behavior).
    header_dirs.insert(root.to_path_buf());
    walk_tree(root, &mut sources, &mut header_dirs);

    let source_files: Vec<PathBuf> = if let Some(list) = files {
        list.iter()
            .map(|f| root.join(f))
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| matches!(e, "m" | "mm"))
                    .unwrap_or(false)
            })
            .collect()
    } else {
        sources
    };

    // Search paths + sysroot + framework paths are identical for every TU, so
    // build them once and share.
    let mut common: Vec<String> = header_dirs
        .iter()
        .map(|d| format!("-I{}", d.to_string_lossy()))
        .collect();
    if let Some(ref sdk) = sdk {
        common.push("-isysroot".to_string());
        common.push(sdk.clone());
    }
    // Best-effort: point at the active platform's framework directory (where
    // XCTest.framework lives) so test targets can parse. Present only with a
    // full Xcode install; empty under the Command Line Tools, where no XCTest
    // exists to find and this is simply skipped.
    if let Some(ref fw) = platform_frameworks() {
        common.push("-iframework".to_string());
        common.push(fw.clone());
    }

    source_files
        .into_iter()
        .map(|file| {
            let is_mm = file
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e == "mm")
                .unwrap_or(false);

            let lang = if is_mm {
                "objective-c++"
            } else {
                "objective-c"
            };
            let mut args = vec!["-x".to_string(), lang.to_string(), "-fobjc-arc".to_string()];
            args.extend(common.iter().cloned());

            CompilationEntry { file, args }
        })
        .collect()
}

/// Single filesystem walk that collects `.m`/`.mm` sources to index and every
/// directory containing a C/ObjC header (so the header can be `#import`ed).
fn walk_tree(
    dir: &Path,
    sources: &mut Vec<PathBuf>,
    header_dirs: &mut std::collections::BTreeSet<PathBuf>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip hidden dirs and common non-source dirs.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.')
                || matches!(name, "build" | "DerivedData" | "Pods" | "node_modules")
            {
                continue;
            }
            walk_tree(&path, sources, header_dirs);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if matches!(ext, "m" | "mm") {
                sources.push(path);
            } else if matches!(ext, "h" | "hh" | "hpp" | "hxx" | "pch") {
                header_dirs.insert(dir.to_path_buf());
            }
        }
    }
}

static PLATFORM_FRAMEWORKS: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// The active platform's `Developer/Library/Frameworks` directory (home of
/// `XCTest.framework`), or `None` under the Command Line Tools where
/// `--show-sdk-platform-path` is empty and no such directory exists.
fn platform_frameworks() -> Option<String> {
    PLATFORM_FRAMEWORKS
        .get_or_init(|| {
            let out = std::process::Command::new("xcrun")
                .arg("--show-sdk-platform-path")
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let base = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if base.is_empty() {
                return None;
            }
            let fw = Path::new(&base).join("Developer/Library/Frameworks");
            fw.is_dir().then(|| fw.to_string_lossy().into_owned())
        })
        .clone()
}

static SDK_PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

fn sdk_path() -> Option<String> {
    SDK_PATH
        .get_or_init(|| {
            let out = std::process::Command::new("xcrun")
                .arg("--show-sdk-path")
                .output()
                .ok()?;
            if out.status.success() {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                None
            }
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_fallback_adds_subdirectory_header_dirs() {
        // #831: a header in a subdirectory must join the include search path so
        // a quoted `#import "Foo.h"` from another directory resolves. The
        // pre-#831 single `-I<root>` could not, leaving the class undeclared.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::create_dir_all(root.join("include/foo")).unwrap();
        std::fs::write(root.join("app/Main.m"), "int main(){return 0;}\n").unwrap();
        std::fs::write(root.join("include/foo/Foo.h"), "@interface Foo\n@end\n").unwrap();

        let entries = discover(root, None);
        let main = entries
            .iter()
            .find(|e| e.file.ends_with("app/Main.m"))
            .expect("Main.m discovered");

        let want = format!("-I{}", root.join("include/foo").to_string_lossy());
        assert!(
            main.args.contains(&want),
            "expected the header's subdirectory on the include path: {want}\n got {:?}",
            main.args
        );
        // The root itself stays a search path (prior behavior preserved).
        assert!(main.args.contains(&format!("-I{}", root.to_string_lossy())));
    }
}
