//! Stub Clang diagnostic collector for the ObjC Phase B emitter.
//!
//! `collect()` is called after the SCIP visitor walk, reusing the same
//! translation unit at zero extra cost. The function body is left empty
//! until RFC-016 Phase 1 lands in travsr-core.

/// Placeholder type until RFC-016 Phase 1 introduces `travsr_core::NormalizedDiagnostic`.
#[allow(dead_code)]
pub struct NormalizedDiagnostic {}

#[allow(dead_code)]
/// Collect Clang diagnostics from an already-parsed translation unit.
///
/// `tu` is `CXTranslationUnit` (a `void *`), kept as `*mut c_void` here so the
/// stub compiles on all platforms without a compile-time libclang dependency.
///
/// To activate: implement using `clang_getDiagnosticSetFromTU()`, set
/// `ObjcPhaseB::supports_inline_diagnostics()` to `true`, and update
/// `InvokeResponse` to carry the result once RFC-016 Phase 1 merges.
pub fn collect(
    _tu: *mut std::ffi::c_void, // CXTranslationUnit — aliased as *mut c_void until RFC-016
    _repo_root: &std::path::Path,
) -> Vec<NormalizedDiagnostic> {
    // TODO(RFC-016): implement when NormalizedDiagnostic lands in travsr-core.
    vec![]
}
