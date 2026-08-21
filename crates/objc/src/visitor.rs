#![cfg(target_os = "macos")]
#![allow(non_upper_case_globals)] // clang-sys constants use CXCursor_* naming
//! libclang AST visitor → SCIP Index builder.
//!
//! Uses a two-level cursor walk:
//!   1. Top-level pass: visit TU direct children; dispatch to typed handlers.
//!   2. Per-type pass: visit class/protocol/category children for methods and
//!      properties.
//!
//! Reference occurrences (ObjCMessageExpr) are collected during the per-method
//! body walk via a separate ref_walk callback.
//!
//! Safety contract: all `unsafe` blocks in this module cross the FFI boundary
//! to libclang only. No raw pointer is stored beyond the scope of a single
//! `clang_visitChildren` call chain: the client_data pointers are stack-
//! allocated and their lifetimes are bounded by the visit.

use clang_sys::*;
use scip::types::{
    symbol_information::Kind as ScipKind, Document, Index, Occurrence, Relationship,
    SymbolInformation,
};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_int, c_uint};
use std::path::{Path, PathBuf};

use crate::compdb;
use crate::symbol;

// ── Index builder ─────────────────────────────────────────────────────────────

/// Accumulates SCIP documents from one or more translation units.
struct IndexBuilder {
    corpus: String,
    root: PathBuf,
    /// Relative path → (occurrences, symbol_infos)
    docs: HashMap<String, DocData>,
}

#[derive(Default)]
struct DocData {
    occs: Vec<Occurrence>,
    syms: Vec<SymbolInformation>,
}

impl IndexBuilder {
    fn new(root: &Path, corpus: &str) -> Self {
        Self {
            corpus: corpus.to_string(),
            root: root.to_owned(),
            docs: HashMap::new(),
        }
    }

    fn add_occurrence(&mut self, rel_path: &str, occ: Occurrence) {
        self.docs
            .entry(rel_path.to_string())
            .or_default()
            .occs
            .push(occ);
    }

    fn add_symbol_info(&mut self, rel_path: &str, si: SymbolInformation) {
        self.docs
            .entry(rel_path.to_string())
            .or_default()
            .syms
            .push(si);
    }

    fn finish(self) -> Index {
        let mut index = Index::default();
        for (path, data) in self.docs {
            index.documents.push(Document {
                relative_path: path,
                language: "objective-c".to_string(),
                occurrences: data.occs,
                symbols: data.syms,
                ..Default::default()
            });
        }
        index
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn build_index(root: &Path, corpus: &str, files: Option<&[String]>) -> anyhow::Result<Index> {
    let entries = compdb::discover(root, files);
    if entries.is_empty() {
        tracing::debug!("no ObjC source files found under {}", root.display());
        return Ok(Index::default());
    }

    let mut builder = IndexBuilder::new(root, corpus);

    let cx_index = unsafe { clang_createIndex(0, 0) };
    if cx_index.is_null() {
        anyhow::bail!("clang_createIndex returned null");
    }

    let mut parsed_ok = 0usize;
    for entry in &entries {
        match process_tu(cx_index, &entry.file, &entry.args, &mut builder) {
            Ok(()) => parsed_ok += 1,
            Err(e) => tracing::warn!(
                file = %entry.file.display(),
                "TU parse failed: {e:#}"
            ),
        }
    }

    unsafe { clang_disposeIndex(cx_index) };

    // If EVERY translation unit failed to parse, this is a hard failure (libclang
    // could not read a required path, or no SDK resolves), not a repo that
    // legitimately has no Objective-C symbols. Returning Ok(empty) here is
    // indistinguishable from "nothing to index" and produced a silent zero-node
    // result upstream, so surface it as an error and the cause is diagnosable
    // (the host forwards the sidecar's stderr on a zero-node/failed invoke).
    if parsed_ok == 0 {
        anyhow::bail!(
            "all {} Objective-C translation unit(s) failed to parse. libclang could \
             not process any source file; check the active toolchain/SDK and the \
             sandbox read grants",
            entries.len()
        );
    }

    let index = builder.finish();
    tracing::info!(
        documents = index.documents.len(),
        parsed_ok,
        total = entries.len(),
        "objc visitor complete"
    );
    Ok(index)
}

// ── Translation unit parsing ──────────────────────────────────────────────────

fn process_tu(
    cx_index: CXIndex,
    file: &Path,
    args: &[String],
    builder: &mut IndexBuilder,
) -> anyhow::Result<()> {
    let c_file =
        CString::new(file.to_string_lossy().as_ref()).map_err(|e| anyhow::anyhow!("{e}"))?;

    let c_args: Vec<CString> = args
        .iter()
        .filter_map(|a| CString::new(a.as_str()).ok())
        .collect();
    let arg_ptrs: Vec<*const i8> = c_args.iter().map(|s| s.as_ptr()).collect();

    // CXTranslationUnit_Incomplete | CXTranslationUnit_KeepGoing
    let flags: c_int = (0x02u32 | 0x200u32) as c_int;

    let tu = unsafe {
        clang_parseTranslationUnit(
            cx_index,
            c_file.as_ptr(),
            arg_ptrs.as_ptr(),
            arg_ptrs.len() as c_int,
            std::ptr::null_mut(),
            0,
            flags,
        )
    };

    if tu.is_null() {
        anyhow::bail!("clang_parseTranslationUnit returned null");
    }

    let root_cursor = unsafe { clang_getTranslationUnitCursor(tu) };

    let mut ctx = VisitorCtx { builder, tu };

    unsafe {
        clang_visitChildren(root_cursor, visit_top_level, &mut ctx as *mut _ as _);
    }

    // Diag stub: diag::collect(tu as *mut c_void, &builder.root), called
    // here when RFC-016 Phase 1 lands. Currently a no-op.

    unsafe { clang_disposeTranslationUnit(tu) };
    Ok(())
}

// ── Visitor context ───────────────────────────────────────────────────────────

struct VisitorCtx<'a> {
    builder: &'a mut IndexBuilder,
    // `tu` is reserved for RFC-016 diag::collect(), held here so we can pass
    // it without changing the callback signatures when the stub is activated.
    #[allow(dead_code)]
    tu: CXTranslationUnit,
}

// ── Top-level visitor callback ────────────────────────────────────────────────

/// Visits direct children of the translation unit cursor.
/// Does NOT auto-recurse; instead, dispatches manually to typed handlers so we
/// can thread `class_name` context without a stack.
extern "C" fn visit_top_level(
    cursor: CXCursor,
    _parent: CXCursor,
    data: CXClientData,
) -> CXChildVisitResult {
    let ctx = unsafe { &mut *(data as *mut VisitorCtx) };

    // Skip anything outside the repo root (system headers, SDK frameworks).
    let loc = unsafe { clang_getCursorLocation(cursor) };
    if unsafe { clang_Location_isInSystemHeader(loc) } != 0 {
        return CXChildVisit_Continue;
    }

    let kind = unsafe { clang_getCursorKind(cursor) };

    match kind {
        CXCursor_ObjCInterfaceDecl => {
            handle_interface(cursor, ctx);
        }
        CXCursor_ObjCImplementationDecl => {
            handle_implementation(cursor, ctx);
        }
        CXCursor_ObjCCategoryDecl | CXCursor_ObjCCategoryImplDecl => {
            handle_category(cursor, kind, ctx);
        }
        CXCursor_ObjCProtocolDecl => {
            handle_protocol(cursor, ctx);
        }
        CXCursor_FunctionDecl => {
            if let Some((rel_path, occ, si)) = make_function_def(cursor, ctx) {
                ctx.builder.add_occurrence(&rel_path, occ);
                ctx.builder.add_symbol_info(&rel_path, si);

                // #596: walk the C-function body for ObjCMessageExpr call sites
                // (e.g. `main()` calling `[obj doThing]`). ObjC method bodies get
                // this via visit_members; a plain C function is dispatched here at
                // the top level, so without this walk its calls were never
                // collected and produced no ref/call edges.
                let mut ref_ctx = RefCtx {
                    corpus: ctx.builder.corpus.clone(),
                    root: ctx.builder.root.clone(),
                    builder: ctx.builder,
                };
                unsafe {
                    clang_visitChildren(cursor, visit_refs, &mut ref_ctx as *mut _ as _);
                }
            }
        }
        _ => {}
    }

    // Do NOT recurse: we handle children explicitly in each handler.
    CXChildVisit_Continue
}

// ── @interface handler ────────────────────────────────────────────────────────

fn handle_interface(cursor: CXCursor, ctx: &mut VisitorCtx) {
    let class_name = cursor_spelling(cursor);
    if class_name.is_empty() {
        return;
    }
    let sym = symbol::class_symbol(&ctx.builder.corpus, &class_name);

    if let Some((rel_path, occ)) = make_def_occurrence(cursor, &sym, &ctx.builder.root) {
        let mut si = make_symbol_info(&sym, ScipKind::Class);

        // Collect superclass + protocol conformance as IsImplementation edges.
        let rels = collect_relationships(cursor, &ctx.builder.corpus);
        si.relationships = rels;

        ctx.builder.add_occurrence(&rel_path, occ);
        ctx.builder.add_symbol_info(&rel_path, si);
    }

    // Visit methods and properties declared in this interface.
    walk_members(cursor, class_name, ctx.builder);
}

// ── @implementation handler ───────────────────────────────────────────────────

fn handle_implementation(cursor: CXCursor, ctx: &mut VisitorCtx) {
    let class_name = cursor_spelling(cursor);
    if class_name.is_empty() {
        return;
    }
    let sym = symbol::class_symbol(&ctx.builder.corpus, &class_name);

    // Emit definition occurrence for the implementation too (same symbol as the
    // interface). The scip-reader last-writer-wins, so the impl line becomes the
    // canonical node line, and both definitions refer to the same NodeId.
    if let Some((rel_path, occ)) = make_def_occurrence(cursor, &sym, &ctx.builder.root) {
        ctx.builder.add_occurrence(&rel_path, occ);
        // SymbolInformation already emitted by the @interface pass; skip here to
        // avoid duplicating the class node with potentially empty relationships.
    }

    walk_members(cursor, class_name, ctx.builder);
}

// ── Category handler ──────────────────────────────────────────────────────────

fn handle_category(cursor: CXCursor, kind: CXCursorKind, ctx: &mut VisitorCtx) {
    // Category methods are emitted under the base class symbol, not under
    // the category name.
    let base_class = category_base_class(cursor);
    if base_class.is_empty() {
        tracing::debug!("category cursor has no ObjCClassRef child, skipping");
        return;
    }

    // Emit a definition occurrence for the category itself only for the
    // declaration form (not the implementation) to avoid duplicate nodes.
    if kind == CXCursor_ObjCCategoryDecl {
        let sym = symbol::class_symbol(&ctx.builder.corpus, &base_class);
        if let Some((rel_path, occ)) = make_def_occurrence(cursor, &sym, &ctx.builder.root) {
            ctx.builder.add_occurrence(&rel_path, occ);
            // SymbolInformation for the base class was already emitted from the
            // @interface pass. No new node needed here.
        }
    }

    walk_members(cursor, base_class, ctx.builder);
}

// ── @protocol handler ─────────────────────────────────────────────────────────

fn handle_protocol(cursor: CXCursor, ctx: &mut VisitorCtx) {
    let proto_name = cursor_spelling(cursor);
    if proto_name.is_empty() {
        return;
    }
    let sym = symbol::protocol_symbol(&ctx.builder.corpus, &proto_name);

    if let Some((rel_path, occ)) = make_def_occurrence(cursor, &sym, &ctx.builder.root) {
        let si = make_symbol_info(&sym, ScipKind::Interface);
        ctx.builder.add_occurrence(&rel_path, occ);
        ctx.builder.add_symbol_info(&rel_path, si);
    }

    // Protocol methods are ObjCMethodDecl children, emitted under the
    // protocol name (used as the "class_name" for selector scoping).
    walk_members(cursor, proto_name, ctx.builder);
}

// ── Member visitor (methods + properties inside a type) ───────────────────────

/// Collect the `(line, col)` of every `@property` declaration under `container`,
/// then walk its members (skipping synthesized property accessors, #596).
fn walk_members(container: CXCursor, class_name: String, builder: &mut IndexBuilder) {
    let mut property_locs: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
    unsafe {
        clang_visitChildren(
            container,
            collect_property_locs,
            &mut property_locs as *mut _ as _,
        );
    }
    let mut member_ctx = MemberCtx {
        class_name,
        builder,
        property_locs,
    };
    unsafe {
        clang_visitChildren(container, visit_members, &mut member_ctx as *mut _ as _);
    }
}

extern "C" fn collect_property_locs(
    cursor: CXCursor,
    _parent: CXCursor,
    data: CXClientData,
) -> CXChildVisitResult {
    if unsafe { clang_getCursorKind(cursor) } == CXCursor_ObjCPropertyDecl {
        if let Some((_, line, col)) = cursor_name_location(cursor) {
            let set = unsafe { &mut *(data as *mut std::collections::HashSet<(u32, u32)>) };
            set.insert((line, col));
        }
    }
    CXChildVisit_Continue
}

struct MemberCtx<'a> {
    class_name: String,
    builder: &'a mut IndexBuilder,
    /// `(line, col)` of every `@property` declaration in this container. A
    /// clang-synthesized getter/setter `ObjCMethodDecl` shares its property's
    /// source location, so methods at one of these positions are skipped
    /// (#596): they have no tree-sitter Phase A counterpart and would survive
    /// as un-unifiable orphan def nodes.
    property_locs: std::collections::HashSet<(u32, u32)>,
}

extern "C" fn visit_members(
    cursor: CXCursor,
    _parent: CXCursor,
    data: CXClientData,
) -> CXChildVisitResult {
    let ctx = unsafe { &mut *(data as *mut MemberCtx) };

    let loc = unsafe { clang_getCursorLocation(cursor) };
    if unsafe { clang_Location_isInSystemHeader(loc) } != 0 {
        return CXChildVisit_Continue;
    }

    let kind = unsafe { clang_getCursorKind(cursor) };

    match kind {
        CXCursor_ObjCInstanceMethodDecl | CXCursor_ObjCClassMethodDecl => {
            let selector = cursor_spelling(cursor);
            if selector.is_empty() {
                return CXChildVisit_Continue;
            }
            // #596: skip clang-synthesized @property getter/setter methods.
            // Their ObjCMethodDecl shares the property's source location, and
            // Phase A (tree-sitter) models the property, not its accessors, so
            // emitting them yields orphan def nodes that never unify.
            if let Some((_, line, col)) = cursor_name_location(cursor) {
                if ctx.property_locs.contains(&(line, col)) {
                    return CXChildVisit_Continue;
                }
            }
            let sym = symbol::method_symbol(&ctx.builder.corpus, &ctx.class_name, &selector);

            if let Some((rel_path, occ)) = make_def_occurrence(cursor, &sym, &ctx.builder.root) {
                let si = make_symbol_info(&sym, ScipKind::Method);
                ctx.builder.add_occurrence(&rel_path, occ);
                ctx.builder.add_symbol_info(&rel_path, si);

                // Walk method body for ObjCMessageExpr call-site references.
                let mut ref_ctx = RefCtx {
                    corpus: ctx.builder.corpus.clone(),
                    root: ctx.builder.root.clone(),
                    builder: ctx.builder,
                };
                unsafe {
                    clang_visitChildren(cursor, visit_refs, &mut ref_ctx as *mut _ as _);
                }
            }
        }
        CXCursor_ObjCPropertyDecl => {
            let prop_name = cursor_spelling(cursor);
            if prop_name.is_empty() {
                return CXChildVisit_Continue;
            }
            let sym = symbol::property_symbol(&ctx.builder.corpus, &ctx.class_name, &prop_name);

            if let Some((rel_path, occ)) = make_def_occurrence(cursor, &sym, &ctx.builder.root) {
                let si = make_symbol_info(&sym, ScipKind::Property);
                ctx.builder.add_occurrence(&rel_path, occ);
                ctx.builder.add_symbol_info(&rel_path, si);
            }
        }
        // ObjCSuperClassRef / ObjCProtocolRef are handled by collect_relationships();
        // skip them here to avoid duplication.
        _ => {}
    }

    // Do NOT recurse into method bodies here; visit_refs handles that separately
    // for ObjCMethodDecl children. For property declarations, no recursion needed.
    CXChildVisit_Continue
}

// ── Reference walk (inside method bodies) ─────────────────────────────────────

struct RefCtx<'a> {
    corpus: String,
    root: PathBuf,
    builder: &'a mut IndexBuilder,
}

extern "C" fn visit_refs(
    cursor: CXCursor,
    _parent: CXCursor,
    data: CXClientData,
) -> CXChildVisitResult {
    let ctx = unsafe { &mut *(data as *mut RefCtx) };

    let loc = unsafe { clang_getCursorLocation(cursor) };
    if unsafe { clang_Location_isInSystemHeader(loc) } != 0 {
        return CXChildVisit_Continue;
    }

    let kind = unsafe { clang_getCursorKind(cursor) };

    if kind == CXCursor_ObjCMessageExpr {
        // clang_getCursorReferenced resolves the callee method when the receiver
        // type is known. Unresolvable receivers (id-typed, dynamic dispatch) are
        // silently skipped to avoid emitting wrong edges.
        let referenced = unsafe { clang_getCursorReferenced(cursor) };
        // Tracks whether clang *semantically resolved* the call, not whether an
        // occurrence was actually added: a resolved-but-system-header target
        // (e.g. `[NSDate date]`) is correctly resolved but intentionally not
        // indexed, and must not fall through to the syntactic fallback below,
        // which would otherwise re-derive the same system-framework symbol
        // from the receiver class ref and reintroduce the noise this skip
        // exists to avoid.
        let mut resolved = false;
        if unsafe { clang_Cursor_isNull(referenced) } == 0 {
            let ref_kind = unsafe { clang_getCursorKind(referenced) };
            if matches!(
                ref_kind,
                CXCursor_ObjCInstanceMethodDecl | CXCursor_ObjCClassMethodDecl
            ) {
                // Skip refs into system headers/SDK frameworks: their symbols
                // have no definition in this index and would only feed noise
                // into the daemon's unresolved-call resolution.
                let ref_loc = unsafe { clang_getCursorLocation(referenced) };
                if unsafe { clang_Location_isInSystemHeader(ref_loc) } == 0 {
                    let selector = cursor_spelling(referenced);
                    let parent = unsafe { clang_getCursorSemanticParent(referenced) };
                    let class_name = resolve_type_name(parent);

                    if !selector.is_empty() && !class_name.is_empty() {
                        let target_sym = symbol::method_symbol(&ctx.corpus, &class_name, &selector);
                        if let Some((rel_path, ref_occ)) =
                            make_ref_occurrence(cursor, &target_sym, &ctx.root)
                        {
                            ctx.builder.add_occurrence(&rel_path, ref_occ);
                        }
                    }
                }
                resolved = true;
            }
        }
        if !resolved {
            // #449: bridged or header-less calls, clang cannot resolve the
            // method decl (e.g. an ObjC → Swift call whose generated -Swift.h
            // is not visible under the glob-fallback compdb). For a class
            // message the receiver class is still syntactically present as an
            // ObjCClassRef child, so synthesize the target symbol from
            // receiver + selector. Instance receivers with unknown type remain
            // skipped (precision over recall).
            let selector = cursor_spelling(cursor);
            let receiver = first_objc_class_ref(cursor);
            if !selector.is_empty() && !receiver.is_empty() {
                let target_sym = symbol::method_symbol(&ctx.corpus, &receiver, &selector);
                if let Some((rel_path, ref_occ)) =
                    make_ref_occurrence(cursor, &target_sym, &ctx.root)
                {
                    ctx.builder.add_occurrence(&rel_path, ref_occ);
                }
            }
        }
    }

    CXChildVisit_Recurse
}

// ── Relationship collector ────────────────────────────────────────────────────

struct RelCtx {
    corpus: String,
    rels: Vec<Relationship>,
}

extern "C" fn visit_rels(
    cursor: CXCursor,
    _parent: CXCursor,
    data: CXClientData,
) -> CXChildVisitResult {
    let ctx = unsafe { &mut *(data as *mut RelCtx) };
    let kind = unsafe { clang_getCursorKind(cursor) };

    match kind {
        CXCursor_ObjCSuperClassRef => {
            let super_name = cursor_spelling(cursor);
            if !super_name.is_empty() {
                ctx.rels.push(Relationship {
                    symbol: symbol::class_symbol(&ctx.corpus, &super_name),
                    is_implementation: true,
                    ..Default::default()
                });
            }
        }
        CXCursor_ObjCProtocolRef => {
            let proto_name = cursor_spelling(cursor);
            if !proto_name.is_empty() {
                ctx.rels.push(Relationship {
                    symbol: symbol::protocol_symbol(&ctx.corpus, &proto_name),
                    is_implementation: true,
                    ..Default::default()
                });
            }
        }
        _ => {}
    }
    CXChildVisit_Continue
}

fn collect_relationships(interface_cursor: CXCursor, corpus: &str) -> Vec<Relationship> {
    let mut ctx = RelCtx {
        corpus: corpus.to_string(),
        rels: Vec::new(),
    };
    unsafe {
        clang_visitChildren(interface_cursor, visit_rels, &mut ctx as *mut _ as _);
    }
    ctx.rels
}

// ── Category base-class resolution ───────────────────────────────────────────

struct BaseClassCtx {
    name: String,
}

extern "C" fn visit_base_class(
    cursor: CXCursor,
    _parent: CXCursor,
    data: CXClientData,
) -> CXChildVisitResult {
    let ctx = unsafe { &mut *(data as *mut BaseClassCtx) };
    if unsafe { clang_getCursorKind(cursor) } == CXCursor_ObjCClassRef {
        ctx.name = cursor_spelling(cursor);
        return CXChildVisit_Break;
    }
    CXChildVisit_Continue
}

fn category_base_class(category_cursor: CXCursor) -> String {
    let mut ctx = BaseClassCtx {
        name: String::new(),
    };
    unsafe {
        clang_visitChildren(category_cursor, visit_base_class, &mut ctx as *mut _ as _);
    }
    ctx.name
}

/// Name of the first `ObjCClassRef` child of any cursor. For a class message
/// expr (`[ClassC method]`) this is the receiver class. Same walk as
/// [`category_base_class`]; named separately for call-site clarity (#449).
fn first_objc_class_ref(cursor: CXCursor) -> String {
    category_base_class(cursor)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Get the name of a type-declaration cursor (class, protocol, or category).
fn resolve_type_name(cursor: CXCursor) -> String {
    if unsafe { clang_Cursor_isNull(cursor) } != 0 {
        return String::new();
    }
    let kind = unsafe { clang_getCursorKind(cursor) };
    match kind {
        CXCursor_ObjCInterfaceDecl
        | CXCursor_ObjCImplementationDecl
        | CXCursor_ObjCProtocolDecl => cursor_spelling(cursor),
        CXCursor_ObjCCategoryDecl | CXCursor_ObjCCategoryImplDecl => category_base_class(cursor),
        _ => String::new(),
    }
}

fn make_function_def(
    cursor: CXCursor,
    ctx: &mut VisitorCtx,
) -> Option<(String, Occurrence, SymbolInformation)> {
    let func_name = cursor_spelling(cursor);
    if func_name.is_empty() {
        return None;
    }
    let sym = symbol::function_symbol(&ctx.builder.corpus, &func_name);
    let (rel_path, occ) = make_def_occurrence(cursor, &sym, &ctx.builder.root)?;
    let si = make_symbol_info(&sym, ScipKind::Function);
    Some((rel_path, occ, si))
}

/// Build a definition occurrence for a named cursor.
/// Returns `None` if the cursor is in a system header or outside the repo root.
fn make_def_occurrence(
    cursor: CXCursor,
    symbol: &str,
    root: &Path,
) -> Option<(String, Occurrence)> {
    let (abs_path, start_line, start_col) = cursor_name_location(cursor)?;
    let rel_path = to_rel_path(root, &abs_path)?;

    let end_col = start_col + cursor_spelling(cursor).len() as u32;

    // Provide the full body extent as enclosing_range so the daemon can attribute
    // reference occurrences to their enclosing function (G2 attribution).
    let enclosing_range = cursor_extent_range(cursor, start_line);
    let occ = Occurrence {
        symbol: symbol.to_string(),
        symbol_roles: 1, // SymbolRole::Definition
        // 0-indexed single-line name token range
        range: vec![
            (start_line - 1) as i32,
            (start_col - 1) as i32,
            (end_col - 1) as i32,
        ],
        enclosing_range,
        ..Default::default()
    };

    Some((rel_path, occ))
}

/// Build a reference occurrence (symbol_roles = 0).
fn make_ref_occurrence(
    cursor: CXCursor,
    target_symbol: &str,
    root: &Path,
) -> Option<(String, Occurrence)> {
    let (abs_path, start_line, start_col) = cursor_name_location(cursor)?;
    let rel_path = to_rel_path(root, &abs_path)?;

    let end_col = start_col + cursor_spelling(cursor).len() as u32;

    let occ = Occurrence {
        symbol: target_symbol.to_string(),
        symbol_roles: 0, // reference
        range: vec![
            (start_line - 1) as i32,
            (start_col - 1) as i32,
            (end_col - 1) as i32,
        ],
        ..Default::default()
    };

    Some((rel_path, occ))
}

fn make_symbol_info(symbol: &str, kind: ScipKind) -> SymbolInformation {
    SymbolInformation {
        symbol: symbol.to_string(),
        kind: kind.into(),
        ..Default::default()
    }
}

/// Location of the cursor's name token (spelling location).
fn cursor_name_location(cursor: CXCursor) -> Option<(String, u32, u32)> {
    let loc = unsafe { clang_getCursorLocation(cursor) };
    let mut file: CXFile = std::ptr::null_mut();
    let mut line: c_uint = 0;
    let mut col: c_uint = 0;
    let mut offset: c_uint = 0;
    unsafe {
        clang_getSpellingLocation(loc, &mut file, &mut line, &mut col, &mut offset);
    }
    if file.is_null() || line == 0 {
        return None;
    }
    let abs_path = cx_file_to_string(file);
    if abs_path.is_empty() {
        return None;
    }
    Some((abs_path, line, col))
}

/// Full body extent as a SCIP 4-element range [sl, sc, el, ec] (0-indexed).
fn cursor_extent_range(cursor: CXCursor, name_line: u32) -> Vec<i32> {
    let extent = unsafe { clang_getCursorExtent(cursor) };
    let start = unsafe { clang_getRangeStart(extent) };
    let end = unsafe { clang_getRangeEnd(extent) };

    let mut sl: c_uint = 0;
    let mut sc: c_uint = 0;
    let mut el: c_uint = 0;
    let mut ec: c_uint = 0;
    let mut offset: c_uint = 0;

    unsafe {
        clang_getSpellingLocation(start, std::ptr::null_mut(), &mut sl, &mut sc, &mut offset);
        clang_getSpellingLocation(end, std::ptr::null_mut(), &mut el, &mut ec, &mut offset);
    }

    // Fall back to name line if extent is unavailable.
    if sl == 0 {
        sl = name_line;
        el = name_line;
    }

    vec![
        (sl.saturating_sub(1)) as i32,
        (sc.saturating_sub(1)) as i32,
        (el.saturating_sub(1)) as i32,
        (ec.saturating_sub(1)) as i32,
    ]
}

fn cursor_spelling(cursor: CXCursor) -> String {
    let cx_str = unsafe { clang_getCursorSpelling(cursor) };
    cx_string_to_owned(cx_str)
}

fn cx_file_to_string(file: CXFile) -> String {
    let cx_str = unsafe { clang_getFileName(file) };
    cx_string_to_owned(cx_str)
}

fn cx_string_to_owned(cx_str: CXString) -> String {
    let ptr = unsafe { clang_getCString(cx_str) };
    let result = if ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    };
    unsafe { clang_disposeString(cx_str) };
    result
}

fn to_rel_path(root: &Path, abs: &str) -> Option<String> {
    Path::new(abs)
        .strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}
