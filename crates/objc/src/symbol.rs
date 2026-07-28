//! ObjC selector → SCIP symbol string canonicalization.
//!
//! Format: `objc . <corpus-or-dot> 0.0.0 <descriptor>`
//!
//! Descriptor rules:
//!   Class / Interface / Implementation  → `ClassName#`
//!   Protocol                            → `ProtocolName/`
//!   Instance or class method            → `ClassName#selector:parts:().`
//!   C function                          → `functionName.`
//!   Property                            → `ClassName#propertyName.`
//!
//! Category methods always resolve to the *base class* symbol — the caller is
//! responsible for passing the base class name, not the category name.

/// SCIP symbol for an ObjC @interface / @implementation declaration.
pub fn class_symbol(corpus: &str, class_name: &str) -> String {
    format!("objc . {} 0.0.0 {}#", pkg(corpus), escape(class_name))
}

/// SCIP symbol for an ObjC @protocol declaration.
///
/// Uses the `/` namespace suffix. The SymbolInformation.kind = Interface field
/// overrides the scip-reader's suffix-based fallback to return "class".
pub fn protocol_symbol(corpus: &str, protocol_name: &str) -> String {
    format!("objc . {} 0.0.0 {}/", pkg(corpus), escape(protocol_name))
}

/// SCIP symbol for an ObjC instance or class method.
///
/// The full ObjC selector (colons preserved) is used verbatim so that
/// `setWidth:height:` stays distinguishable from `setWidth:` and `height:`.
/// Category methods MUST pass the *base class* name, not the category name.
///
/// #449: the `().` method suffix makes the descriptor SCIP-grammar-conformant
/// so travsr core's `scip_name_kind` parses it as
/// `(container: Class, name: selector, kind: function)` and the node unifies
/// with its Phase A counterpart.
pub fn method_symbol(corpus: &str, class_name: &str, selector: &str) -> String {
    format!(
        "objc . {} 0.0.0 {}#{}().",
        pkg(corpus),
        escape(class_name),
        selector,
    )
}

/// SCIP symbol for an ObjC @property declaration.
pub fn property_symbol(corpus: &str, class_name: &str, property_name: &str) -> String {
    format!(
        "objc . {} 0.0.0 {}#{}.",
        pkg(corpus),
        escape(class_name),
        escape(property_name),
    )
}

/// SCIP symbol for a C function defined in an ObjC translation unit.
pub fn function_symbol(corpus: &str, func_name: &str) -> String {
    format!("objc . {} 0.0.0 {}.", pkg(corpus), escape(func_name))
}

/// Sanitize corpus for use as the SCIP package name.
/// Returns `.` (SCIP "no package") when corpus is empty.
fn pkg(corpus: &str) -> &str {
    if corpus.is_empty() {
        "."
    } else {
        corpus
    }
}

/// Backtick-escape a descriptor component that contains SCIP-special characters.
///
/// SCIP uses space as a delimiter, and `#`, `/`, `.`, `(`, `)`, backtick as
/// descriptor-level special characters. All others are safe.
fn escape(s: &str) -> String {
    let needs_escape = s
        .chars()
        .any(|c| matches!(c, ' ' | '`' | '#' | '/' | '.' | '(' | ')'));
    if needs_escape {
        format!("`{}`", s.replace('`', "``"))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_ends_with_hash() {
        assert!(class_symbol("corp", "Dog").ends_with("Dog#"));
    }

    #[test]
    fn protocol_ends_with_slash() {
        assert!(protocol_symbol("corp", "Speakable").ends_with("Speakable/"));
    }

    #[test]
    fn method_preserves_selector_colons() {
        let sym = method_symbol("corp", "Foo", "setWidth:height:");
        assert!(sym.contains("Foo#setWidth:height:"), "got: {sym}");
    }

    #[test]
    fn method_has_scip_function_suffix() {
        // #449: `().` suffix so travsr core parses the descriptor as a function.
        let sym = method_symbol("corp", "Foo", "setWidth:height:");
        assert!(sym.ends_with("Foo#setWidth:height:()."), "got: {sym}");
    }

    #[test]
    fn empty_corpus_uses_dot() {
        let sym = class_symbol("", "Bar");
        assert!(sym.starts_with("objc . . 0.0.0"), "got: {sym}");
    }

    #[test]
    fn function_ends_with_dot() {
        assert!(function_symbol("corp", "helper").ends_with("helper."));
    }

    #[test]
    fn identifier_with_space_is_escaped() {
        let sym = class_symbol("corp", "My Class");
        assert!(sym.contains("`My Class`"), "got: {sym}");
    }
}
