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
use std::collections::{BTreeMap, HashSet};
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
    /// Relative path → (occurrences, symbol_infos).
    ///
    /// A `BTreeMap`, not a `HashMap`, so `finish()` emits documents in a stable
    /// path order. Downstream SCIP ingest keys its definition table on the SCIP
    /// symbol string, and a symbol defined in two documents (e.g. a test file
    /// that re-declares a category method) is resolved first-document-wins, so
    /// a randomized document order made identical input produce a different
    /// edge target from run to run.
    ///
    /// Reproducible, not necessarily right. `travsr-lang-scip-reader` already
    /// prefers a `.m` definition over a `.h` declaration, so the header case is
    /// handled; between two implementation files it is first-document-wins, and
    /// ascending byte order puts `Tests/FooTests.m` (`T`) before `src/Foo.m`
    /// (`s`). For that case the stable winner is the test file. Deciding it the
    /// other way is a definition-preference question, and it belongs where the
    /// header preference already lives, not in this emitter's document order.
    docs: BTreeMap<String, DocData>,
    /// #833 phase 1: class-message sends recovered lexically from translation
    /// units clang reported errors on. Held aside until every TU is processed,
    /// because the phase-2 gate ("does the index define this method?") cannot
    /// be evaluated before the last definition has been seen.
    recovery: Vec<SendCandidate>,
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
            docs: BTreeMap::new(),
            recovery: Vec::new(),
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

    /// Emit the accumulated documents in ascending `relative_path` order.
    ///
    /// The ordering is load-bearing, not cosmetic: see the `docs` field docs.
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

    // #833 phase 2: every TU has been processed, so the index now holds every
    // definition it will ever hold. Only now can a lexically recovered send be
    // checked against it.
    let recovered = apply_error_tu_recovery(&mut builder);

    let index = builder.finish();
    tracing::info!(
        documents = index.documents.len(),
        parsed_ok,
        total = entries.len(),
        recovered,
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

    // #833 phase 1: when clang reported an error for this TU, an undeclared
    // receiver made Sema drop the whole enclosing `ObjCMessageExpr` (no
    // RecoveryExpr is built), so `visit_refs` never saw a send to gate on and
    // the #831 token fallback inside it was unreachable. Re-scan the TU's
    // method and function bodies lexically and buffer the candidates; they are
    // filtered against the index in phase 2.
    if tu_has_error(tu) {
        let root_path = builder.root.clone();
        let mut rec = RecoveryCtx {
            tu,
            root: root_path,
            out: &mut builder.recovery,
        };
        unsafe {
            clang_visitChildren(root_cursor, visit_recovery, &mut rec as *mut _ as _);
        }
    }

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
                let tu = ctx.tu;
                let mut ref_ctx = RefCtx {
                    corpus: ctx.builder.corpus.clone(),
                    root: ctx.builder.root.clone(),
                    builder: ctx.builder,
                    tu,
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
    walk_members(cursor, class_name, ctx.builder, ctx.tu);
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

    walk_members(cursor, class_name, ctx.builder, ctx.tu);
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

    walk_members(cursor, base_class, ctx.builder, ctx.tu);
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
    walk_members(cursor, proto_name, ctx.builder, ctx.tu);
}

// ── Member visitor (methods + properties inside a type) ───────────────────────

/// Collect the `(line, col)` of every `@property` declaration under `container`,
/// then walk its members (skipping synthesized property accessors, #596).
fn walk_members(
    container: CXCursor,
    class_name: String,
    builder: &mut IndexBuilder,
    tu: CXTranslationUnit,
) {
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
        tu,
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
    // #831: threaded to the method-body `RefCtx` so an unresolved class-message
    // send can be tokenized from source.
    tu: CXTranslationUnit,
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
                let tu = ctx.tu;
                let mut ref_ctx = RefCtx {
                    corpus: ctx.builder.corpus.clone(),
                    root: ctx.builder.root.clone(),
                    builder: ctx.builder,
                    tu,
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
    // #831: needed to tokenize an unresolved class-message send's source extent
    // when clang could not resolve the callee (the semantic receiver/selector
    // are then empty). `CXTranslationUnit` is a Copy pointer bounded by the
    // enclosing `process_tu` visit, same as every other cursor here.
    tu: CXTranslationUnit,
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
            //
            // #831: when the *whole* callee class is undeclared in the TU (a
            // test/example target whose `#import` chain dies on a missing
            // framework header under the glob fallback), the receiver is not an
            // `ObjCClassRef` and `cursor_spelling` on the message expr is empty,
            // so the semantic pair above is `("", "")` and every
            // `[ClassName selector]` call (the most common Obj-C pattern)
            // produced no ref occurrence at all. Recover the receiver and
            // selector lexically from the send's own source tokens, which are
            // present regardless of semantic resolution.
            let mut selector = cursor_spelling(cursor);
            let mut receiver = first_objc_class_ref(cursor);
            if (selector.is_empty() || receiver.is_empty()) && !ctx.tu.is_null() {
                if let Some((r, s)) = syntactic_class_send(ctx.tu, cursor) {
                    receiver = r;
                    selector = s;
                }
            }
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

/// #831: recover `(receiver, selector)` for a class-message send from its source
/// tokens, for the case clang could not resolve semantically (the callee class
/// is undeclared in the TU, so there is no `ObjCClassRef` receiver and the
/// message expr spells empty).
///
/// The token grammar itself lives in [`parse_send_at`], shared with the #833
/// error-TU recovery pass, which scans whole bodies rather than one message.
/// A wrong selector (e.g. a bare ternary colon at message depth) yields a
/// symbol that matches no definition and is dropped downstream as a safe miss,
/// never a wrong edge.
fn syntactic_class_send(tu: CXTranslationUnit, cursor: CXCursor) -> Option<(String, String)> {
    let extent = unsafe { clang_getCursorExtent(cursor) };
    let toks = tokenize_range(tu, extent);
    // A message expr's extent starts at its own `[`; find it rather than
    // assuming index 0 so a leading cast or attribute cannot shift the grammar.
    let open = toks.iter().position(|t| t.spelling == "[")?;
    let send = parse_send_at(&toks, open)?;
    Some((send.receiver, send.selector))
}

/// One lexed source token: the pieces the send grammar needs, captured while
/// the libclang token array is still alive.
struct Tok {
    kind: CXTokenKind,
    spelling: String,
    /// 1-based spelling line/column. Zero when libclang has no location.
    line: u32,
    col: u32,
}

/// Lex a source range into owned tokens.
///
/// `clang_tokenize` is purely lexical, so every token comes from the raw text
/// of `extent` in a single file; the caller supplies that file's path.
fn tokenize_range(tu: CXTranslationUnit, extent: CXSourceRange) -> Vec<Tok> {
    let mut tokens: *mut CXToken = std::ptr::null_mut();
    let mut num: c_uint = 0;
    unsafe { clang_tokenize(tu, extent, &mut tokens, &mut num) };
    if tokens.is_null() || num == 0 {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(num as usize);
    for i in 0..num {
        let tok = unsafe { *tokens.add(i as usize) };
        let kind = unsafe { clang_getTokenKind(tok) };
        let spelling = cx_string_to_owned(unsafe { clang_getTokenSpelling(tu, tok) });
        let loc = unsafe { clang_getTokenLocation(tu, tok) };
        let mut line: c_uint = 0;
        let mut col: c_uint = 0;
        let mut offset: c_uint = 0;
        unsafe {
            clang_getSpellingLocation(loc, std::ptr::null_mut(), &mut line, &mut col, &mut offset);
        }
        out.push(Tok {
            kind,
            spelling,
            line,
            col,
        });
    }

    // The `CXToken` values borrow this buffer, so nothing may outlive it; every
    // field `Tok` needs has already been copied out.
    unsafe { clang_disposeTokens(tu, tokens, num) };
    out
}

/// A `[Receiver keyword:arg …]` send recovered from source tokens.
struct ParsedSend {
    receiver: String,
    selector: String,
    /// Index into the token slice of the selector's first keyword token. This
    /// is the send's "selector start", the same location libclang reports for
    /// an `ObjCMessageExpr` cursor, so a recovered occurrence lands on the same
    /// range a parsed one would.
    selector_tok: usize,
}

/// Parse the class-message send whose opening `[` is `toks[open]`.
///
/// The receiver is the first identifier at the bracket's own depth; a selector
/// keyword is any identifier at that depth immediately followed by a `:` at
/// that depth, concatenated with colons (`setWidth:height:`). Depth tracking
/// over `[](){}` keeps nested sends, casts and parenthesized arguments from
/// being read as selector parts, and the scan stops at the matching `]`.
///
/// Gated to a class receiver (upper-case leading character, the Obj-C class
/// naming convention): `[self …]`, `[super …]` and lower-case locals are
/// instance dispatch on an unknown type and stay skipped, exactly as the
/// semantic path already skips them. This also rejects the array-subscript and
/// collection-literal brackets a body-wide scan necessarily walks over.
fn parse_send_at(toks: &[Tok], open: usize) -> Option<ParsedSend> {
    let mut depth: i32 = 0;
    let mut receiver: Option<String> = None;
    let mut first_after_receiver: Option<(String, usize)> = None;
    // Last identifier seen at message depth, pending a `:` that would make it a
    // selector keyword.
    let mut pending_keyword: Option<(String, usize)> = None;
    let mut selector = String::new();
    let mut selector_tok: Option<usize> = None;
    let mut has_colon = false;

    for (i, tok) in toks.iter().enumerate().skip(open) {
        match tok.spelling.as_str() {
            "[" | "(" | "{" => {
                depth += 1;
                continue;
            }
            "]" | ")" | "}" => {
                depth -= 1;
                if depth <= 0 {
                    break;
                }
                continue;
            }
            _ => {}
        }
        // Only the outer message's own tokens (depth 1) form its receiver and
        // selector; anything deeper belongs to a nested send or an argument.
        if depth != 1 {
            continue;
        }
        if tok.spelling == ":" {
            if let Some((k, idx)) = pending_keyword.take() {
                selector.push_str(&k);
                selector.push(':');
                has_colon = true;
                if selector_tok.is_none() {
                    selector_tok = Some(idx);
                }
            }
            continue;
        }
        if tok.kind == CXToken_Identifier {
            if receiver.is_none() {
                receiver = Some(tok.spelling.clone());
            } else {
                if first_after_receiver.is_none() {
                    first_after_receiver = Some((tok.spelling.clone(), i));
                }
                pending_keyword = Some((tok.spelling.clone(), i));
            }
        } else {
            // A non-identifier, non-colon token cannot be a selector keyword and
            // breaks the `identifier :` adjacency.
            pending_keyword = None;
        }
    }

    let receiver = receiver?;
    if !receiver.chars().next().is_some_and(|c| c.is_uppercase()) {
        return None;
    }
    let (selector, selector_tok) = if has_colon {
        (selector, selector_tok?)
    } else {
        // Unary selector: `[Class method]`, the single identifier after the
        // receiver is the whole selector, no colon.
        let (name, idx) = first_after_receiver?;
        (name, idx)
    };
    if selector.is_empty() {
        return None;
    }
    Some(ParsedSend {
        receiver,
        selector,
        selector_tok,
    })
}

// ── Error-TU token recovery (#833) ───────────────────────────────────────────

/// A class-message send recovered lexically from a TU clang reported errors on.
/// Buffered in phase 1; only turned into an occurrence in phase 2, and only if
/// the index actually defines `receiver` + `selector`.
struct SendCandidate {
    rel_path: String,
    /// 1-based selector-start location.
    line: u32,
    col: u32,
    receiver: String,
    selector: String,
}

/// True when clang emitted at least one Error or Fatal diagnostic for this TU.
///
/// `diag.rs` is an RFC-016 stub that surfaces nothing, so the severity read
/// lives here where the libclang handle already is.
fn tu_has_error(tu: CXTranslationUnit) -> bool {
    let n = unsafe { clang_getNumDiagnostics(tu) };
    for i in 0..n {
        let d = unsafe { clang_getDiagnostic(tu, i) };
        if d.is_null() {
            continue;
        }
        let severity = unsafe { clang_getDiagnosticSeverity(d) };
        unsafe { clang_disposeDiagnostic(d) };
        if severity >= CXDiagnostic_Error {
            return true;
        }
    }
    false
}

struct RecoveryCtx<'a> {
    tu: CXTranslationUnit,
    root: PathBuf,
    out: &'a mut Vec<SendCandidate>,
}

/// Walks an error TU looking for bodies to re-scan lexically.
extern "C" fn visit_recovery(
    cursor: CXCursor,
    _parent: CXCursor,
    data: CXClientData,
) -> CXChildVisitResult {
    let ctx = unsafe { &mut *(data as *mut RecoveryCtx) };

    let loc = unsafe { clang_getCursorLocation(cursor) };
    if unsafe { clang_Location_isInSystemHeader(loc) } != 0 {
        return CXChildVisit_Continue;
    }

    let kind = unsafe { clang_getCursorKind(cursor) };
    if matches!(
        kind,
        CXCursor_ObjCInstanceMethodDecl | CXCursor_ObjCClassMethodDecl | CXCursor_FunctionDecl
    ) {
        scan_body_for_sends(ctx, cursor);
        // The body is scanned whole; bodies do not nest.
        return CXChildVisit_Continue;
    }

    CXChildVisit_Recurse
}

/// Lex one method/function body and buffer every class-message send in it.
fn scan_body_for_sends(ctx: &mut RecoveryCtx, cursor: CXCursor) {
    let Some((abs_path, _, _)) = cursor_name_location(cursor) else {
        return;
    };
    let Some(rel_path) = to_rel_path(&ctx.root, &abs_path) else {
        return;
    };

    let extent = unsafe { clang_getCursorExtent(cursor) };
    let toks = tokenize_range(ctx.tu, extent);

    // Unlike the #831 fallback, the extent here is a whole body, so every `[`
    // is a candidate send opener rather than exactly one message.
    //
    // `clang_tokenize` is a raw lexer over the source text: it yields the
    // contents of preprocessor regions this build never compiles, so a body
    // holding `#if 0 [Legacy start]; #endif` would otherwise recover a send
    // that does not exist in this configuration. Nothing here knows which
    // branch is live (the raw token stream carries no macro state), so every
    // conditional region is declined rather than guessed at. Declining costs
    // recall only inside error TUs, where the alternative is a wrong edge.
    let mut cond_depth = 0usize;
    for (i, tok) in toks.iter().enumerate() {
        if tok.spelling == "#" {
            match toks.get(i + 1).map(|t| t.spelling.as_str()) {
                Some("if") | Some("ifdef") | Some("ifndef") => cond_depth += 1,
                Some("endif") => cond_depth = cond_depth.saturating_sub(1),
                _ => {}
            }
            continue;
        }
        if cond_depth > 0 {
            continue;
        }
        if tok.spelling != "[" {
            continue;
        }
        let Some(send) = parse_send_at(&toks, i) else {
            continue;
        };
        let at = &toks[send.selector_tok];
        if at.line == 0 || at.col == 0 {
            continue;
        }
        ctx.out.push(SendCandidate {
            rel_path: rel_path.clone(),
            line: at.line,
            col: at.col,
            receiver: send.receiver,
            selector: send.selector,
        });
    }
}

/// #833 phase 2: turn buffered candidates into reference occurrences.
///
/// Two gates keep this from being grep with extra steps:
///   1. `receiver` + `selector` must canonicalize to a method symbol the index
///      already defines. An unknown `[NSFileManager defaultManager]` resolves
///      to nothing and is dropped; `[AFSecurityPolicy policyWithPinningMode:]`
///      matches the definition in `AFSecurityPolicy.m` and is kept.
///   2. A send that parsed fine already produced its own occurrence, so a
///      candidate for a symbol already referenced on that line is dropped
///      instead of double-counting the call.
///
/// Returns the number of occurrences added.
fn apply_error_tu_recovery(builder: &mut IndexBuilder) -> usize {
    let candidates = std::mem::take(&mut builder.recovery);
    if candidates.is_empty() {
        return 0;
    }

    let corpus = builder.corpus.clone();
    let defined: HashSet<String> = builder
        .docs
        .values()
        .flat_map(|d| d.syms.iter().map(|si| si.symbol.clone()))
        .collect();

    // (file, symbol, line) of every occurrence already in the index. Keyed on
    // the line rather than the exact range so a one-column difference between
    // the lexical selector-start and libclang's own cursor location can never
    // let the same call in twice.
    let mut seen: HashSet<(String, String, i32)> = HashSet::new();
    for (path, data) in &builder.docs {
        for occ in &data.occs {
            if let Some(&line) = occ.range.first() {
                seen.insert((path.clone(), occ.symbol.clone(), line));
            }
        }
    }

    let mut added = 0usize;
    for c in candidates {
        let target = symbol::method_symbol(&corpus, &c.receiver, &c.selector);
        if !defined.contains(&target) {
            continue;
        }
        let line = (c.line - 1) as i32;
        let start_col = (c.col - 1) as i32;
        if !seen.insert((c.rel_path.clone(), target.clone(), line)) {
            continue;
        }
        let occ = Occurrence {
            symbol: target,
            symbol_roles: 0, // reference
            range: vec![line, start_col, start_col + c.selector.len() as i32],
            ..Default::default()
        };
        builder.add_occurrence(&c.rel_path, occ);
        added += 1;
    }
    added
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

#[cfg(all(test, target_os = "macos"))]
mod ref_recovery_tests {
    use super::*;

    /// Run the visitor over a fixture directory and return the built index.
    fn build(root: &Path) -> Index {
        crate::init_libclang_env();
        let lib = crate::shared_libclang().expect("libclang");
        std::thread::scope(|s| {
            let root = root.to_path_buf();
            s.spawn(move || {
                clang_sys::set_library(Some(lib));
                build_index(&root, "t", None)
            })
            .join()
            .unwrap()
        })
        .expect("build_index")
    }

    /// All reference-occurrence symbols in the index (`symbol_roles` even).
    fn ref_symbols(index: &Index) -> Vec<String> {
        index
            .documents
            .iter()
            .flat_map(|d| d.occurrences.iter())
            .filter(|o| o.symbol_roles & 1 == 0)
            .map(|o| o.symbol.clone())
            .collect()
    }

    /// Number of reference occurrences whose symbol ends with `suffix`.
    fn ref_count(index: &Index, suffix: &str) -> usize {
        ref_symbols(index)
            .iter()
            .filter(|s| s.ends_with(suffix))
            .count()
    }

    #[test]
    fn class_send_to_forward_declared_class_is_recovered() {
        // #831: the callee class is only `@class`-forward-declared in the caller
        // TU, so clang cannot resolve the class method and the message expr
        // spells empty. The syntactic token fallback must still recover
        // `Widget#make:().` from the source tokens (class-cased receiver).
        if !crate::libclang_available() {
            eprintln!("skipping: libclang not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // The definition lives in its own TU (never imported by the caller).
        std::fs::write(
            root.join("Def.m"),
            "@interface Widget\n\
             + (Widget *)make:(int)m;\n\
             @end\n\
             @implementation Widget\n\
             + (Widget *)make:(int)m { return 0; }\n\
             @end\n",
        )
        .unwrap();
        // The caller only forward-declares Widget, so the send is unresolved.
        std::fs::write(
            root.join("Use.m"),
            "@class Widget;\n\
             @interface U\n\
             - (void)go;\n\
             @end\n\
             @implementation U\n\
             - (void)go { [Widget make:1]; }\n\
             @end\n",
        )
        .unwrap();

        let index = build(root);
        let refs = ref_symbols(&index);
        assert!(
            refs.iter().any(|s| s.ends_with("Widget#make:().")),
            "forward-declared class send must be recovered from tokens; got {refs:?}"
        );
    }

    #[test]
    fn instance_send_to_lowercase_receiver_is_not_synthesized() {
        // Precision guard: the token fallback is class-message only. An
        // unresolved instance send (`[helper doWork]`, lower-case receiver on an
        // unknown type) must stay skipped, not be synthesized as `helper#…`.
        if !crate::libclang_available() {
            eprintln!("skipping: libclang not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Use.m"),
            "@interface U\n\
             - (void)go;\n\
             @end\n\
             @implementation U\n\
             - (void)go { id helper = 0; [helper doWork]; }\n\
             @end\n",
        )
        .unwrap();

        let index = build(root);
        let refs = ref_symbols(&index);
        assert!(
            !refs.iter().any(|s| s.contains("doWork")),
            "instance send on unknown type must not be synthesized; got {refs:?}"
        );
    }

    #[test]
    fn class_send_across_subdirectory_header_resolves() {
        // #831 (Fix A): the callee's header lives in a subdirectory. With the
        // header directory on the include path the quoted `#import` resolves,
        // the class is declared, and `[Thing build]` parses as a real message
        // expr whose ref is emitted.
        if !crate::libclang_available() {
            eprintln!("skipping: libclang not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(
            root.join("sub/Thing.h"),
            "@interface Thing\n+ (Thing *)build;\n@end\n",
        )
        .unwrap();
        std::fs::write(
            root.join("sub/Thing.m"),
            "#import \"Thing.h\"\n\
             @implementation Thing\n\
             + (Thing *)build { return 0; }\n\
             @end\n",
        )
        .unwrap();
        std::fs::write(
            root.join("app/Main.m"),
            "#import \"Thing.h\"\n\
             @interface App\n\
             - (void)r;\n\
             @end\n\
             @implementation App\n\
             - (void)r { [Thing build]; }\n\
             @end\n",
        )
        .unwrap();

        let index = build(root);
        let refs = ref_symbols(&index);
        assert!(
            refs.iter().any(|s| s.ends_with("Thing#build().")),
            "cross-subdirectory class send must resolve and emit a ref; got {refs:?}"
        );
    }
    // ── #833: error-TU token recovery ────────────────────────────────────────

    #[test]
    fn error_tu_recovery_drops_sends_to_undefined_classes() {
        // #833 PRECISION GUARD. `Bad.h` uses `@import`, which is a hard error
        // under the glob-fallback flags, so clang drops every expression whose
        // receiver it could not declare. The body-wide token re-scan therefore
        // sees BOTH `[Local ping]` and `[NSFileManager defaultManager]`.
        // Only the first resolves to a definition this index holds, so only the
        // first may become an edge. Recovering the second would make this
        // feature grep with extra steps.
        if !crate::libclang_available() {
            eprintln!("skipping: libclang not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Local.h"),
            "@interface Local\n+ (void)ping;\n@end\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Local.m"),
            "#import \"Local.h\"\n\
             @implementation Local\n\
             + (void)ping {}\n\
             @end\n",
        )
        .unwrap();
        std::fs::write(root.join("Bad.h"), "@import NoSuchModuleAnywhere;\n").unwrap();
        std::fs::write(
            root.join("Use.m"),
            "#import \"Bad.h\"\n\
             @interface U\n\
             + (void)go;\n\
             @end\n\
             @implementation U\n\
             + (void)go { [Local ping]; [NSFileManager defaultManager]; }\n\
             @end\n",
        )
        .unwrap();

        let index = build(root);
        let refs = ref_symbols(&index);
        // Positive control: proves the recovery pass actually ran on this TU.
        assert!(
            refs.iter().any(|s| s.ends_with("Local#ping().")),
            "recovery must emit the send whose callee the index defines; got {refs:?}"
        );
        // The guard itself.
        assert!(
            !refs.iter().any(|s| s.contains("defaultManager")),
            "send to a class the index does not define must be dropped; got {refs:?}"
        );
    }

    #[test]
    fn class_send_under_module_import_error_is_recovered() {
        // #833, the AFNetworking miss minimised: the caller's header reaches the
        // callee class only through a Clang MODULE import. `@import` is a hard
        // error without `-fmodules`, so `Policy` is undeclared, Sema returns
        // ExprError for the receiver, and the enclosing `ObjCMessageExpr` is
        // never constructed at all (not even a RecoveryExpr). Nothing reaches
        // `visit_refs`, so the #831 fallback that lives inside its
        // `CXCursor_ObjCMessageExpr` branch cannot fire. Recovery has to sit
        // above that gate.
        if !crate::libclang_available() {
            eprintln!("skipping: libclang not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Policy.h"),
            "@interface Policy\n+ (Policy *)policyWithMode:(int)m;\n@end\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Policy.m"),
            "#import \"Policy.h\"\n\
             @implementation Policy\n\
             + (Policy *)policyWithMode:(int)m { return 0; }\n\
             @end\n",
        )
        .unwrap();
        // The caller's header pulls the callee in as a module, and nothing else.
        std::fs::write(
            root.join("Client.h"),
            "@import SomethingUnavailable;\n@interface Client\n+ (void)go;\n@end\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Client.m"),
            "#import \"Client.h\"\n\
             @implementation Client\n\
             + (void)go { [Policy policyWithMode:1]; }\n\
             @end\n",
        )
        .unwrap();

        let index = build(root);
        let refs = ref_symbols(&index);
        assert!(
            refs.iter()
                .any(|s| s.ends_with("Policy#policyWithMode:().")),
            "send through a failed module import must be recovered; got {refs:?}"
        );
    }

    #[test]
    fn recovery_does_not_double_count_a_send_that_parsed() {
        // #833: the TU still reports an error (the `@import` in Client.h), so
        // the recovery scan runs over this body too. But `Policy.h` is imported
        // directly here, so the send parses and `visit_refs` already emitted its
        // occurrence. Exactly one reference must survive.
        if !crate::libclang_available() {
            eprintln!("skipping: libclang not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Policy.h"),
            "@interface Policy\n+ (Policy *)policyWithMode:(int)m;\n@end\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Policy.m"),
            "#import \"Policy.h\"\n\
             @implementation Policy\n\
             + (Policy *)policyWithMode:(int)m { return 0; }\n\
             @end\n",
        )
        .unwrap();
        std::fs::write(root.join("Bad.h"), "@import NoSuchModuleAnywhere;\n").unwrap();
        std::fs::write(
            root.join("Client.m"),
            "#import \"Bad.h\"\n\
             #import \"Policy.h\"\n\
             @interface Client\n\
             + (void)go;\n\
             @end\n\
             @implementation Client\n\
             + (void)go { [Policy policyWithMode:1]; }\n\
             @end\n",
        )
        .unwrap();

        let index = build(root);
        assert_eq!(
            ref_count(&index, "Policy#policyWithMode:()."),
            1,
            "a send that parsed must not be counted twice; got {:?}",
            ref_symbols(&index)
        );
    }

    #[test]
    fn error_tu_recovery_skips_inactive_preprocessor_regions() {
        // #833 PRECISION GUARD. `clang_tokenize` is a raw lexer: it returns the
        // text of a `#if 0` region, which the compiler never sees. Verified
        // against libclang directly: the token stream for the method below
        // contains `[ Legacy start ]`. Recovering it would emit an edge to a
        // call that does not exist in this build configuration.
        if !crate::libclang_available() {
            eprintln!("skipping: libclang not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Legacy.h"),
            "@interface Legacy\n+ (void)start;\n@end\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Legacy.m"),
            "#import \"Legacy.h\"\n\
             @implementation Legacy\n\
             + (void)start {}\n\
             @end\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Modern.h"),
            "@interface Modern\n+ (void)start;\n@end\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Modern.m"),
            "#import \"Modern.h\"\n\
             @implementation Modern\n\
             + (void)start {}\n\
             @end\n",
        )
        .unwrap();
        // `@import` is a hard error under the glob-fallback flags, which is what
        // puts this TU on the lexical recovery path in the first place.
        std::fs::write(root.join("Bad.h"), "@import NoSuchModuleAnywhere;\n").unwrap();
        std::fs::write(
            root.join("Use.m"),
            "#import \"Bad.h\"\n\
             @interface U\n\
             + (void)go;\n\
             @end\n\
             @implementation U\n\
             + (void)go {\n\
             [Modern start];\n\
             #if 0\n\
             [Legacy start];\n\
             #endif\n\
             }\n\
             @end\n",
        )
        .unwrap();

        let index = build(root);
        let refs = ref_symbols(&index);
        // Positive control: `Modern` is never imported here, so this send cannot
        // parse and only the lexical recovery can produce it. It proves the
        // recovery pass really ran on this TU.
        assert!(
            refs.iter().any(|s| s.ends_with("Modern#start().")),
            "recovery must still emit the send outside the conditional; got {refs:?}"
        );
        // The guard itself.
        assert!(
            !refs.iter().any(|s| s.contains("Legacy#start")),
            "a send inside an inactive #if region must not be recovered; got {refs:?}"
        );
    }
}

#[cfg(test)]
mod document_order_tests {
    use super::*;

    /// Two builders fed the same documents must emit them in the same order.
    ///
    /// Regression guard for the objc nondeterminism bug: `docs` was a
    /// `HashMap`, so `finish()` walked it in a per-map randomized order. Two
    /// consecutive indexes of the same unmodified repo then disagreed on which
    /// document defined a symbol that appears in more than one (SCIP ingest
    /// resolves a duplicated symbol first-document-wins), and a handful of
    /// `ref/call` edges flipped target from run to run. `std`'s `RandomState`
    /// reseeds per map instance, so the two builders below take different
    /// orders under the old code even inside one process.
    #[test]
    fn finish_emits_documents_in_stable_sorted_order() {
        let build = || {
            let mut b = IndexBuilder::new(Path::new("/tmp"), "test/objc");
            // Insert in an order that is neither sorted nor reverse sorted.
            for i in [7, 1, 11, 3, 9, 0, 5, 2, 10, 4, 8, 6] {
                b.add_occurrence(&format!("src/File{i:02}.m"), Occurrence::default());
            }
            b.finish()
                .documents
                .iter()
                .map(|d| d.relative_path.clone())
                .collect::<Vec<_>>()
        };

        let first = build();
        assert_eq!(first, build(), "document order must not vary between runs");

        let mut sorted = first.clone();
        sorted.sort();
        assert_eq!(first, sorted, "documents must be emitted in path order");
    }
}
