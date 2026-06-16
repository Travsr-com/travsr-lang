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
    let root_str = root.to_string_lossy().into_owned();

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
        walk_objc_files(root)
    };

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
            let mut args = vec![
                "-x".to_string(),
                lang.to_string(),
                "-fobjc-arc".to_string(),
                format!("-I{root_str}"),
            ];

            if let Some(ref sdk) = sdk {
                args.push("-isysroot".to_string());
                args.push(sdk.clone());
            }

            CompilationEntry { file, args }
        })
        .collect()
}

fn walk_objc_files(root: &Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    walk_dir(root, root, &mut results);
    results
}

fn walk_dir(_root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
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
            walk_dir(_root, &path, out);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if matches!(ext, "m" | "mm") {
                out.push(path);
            }
        }
    }
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
