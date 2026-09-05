/// Travsr Phase B: Swift structural emitter using SwiftSyntax.
///
/// Parses every .swift file in <root> and emits a JSON index of definitions,
/// call-site references, and type inheritance edges for Travsr's Phase B pipeline.
///
/// Analysis coverage (parse-level, no compilation required):
///   • All named declarations (class/struct/enum/protocol/actor, members, init).
///   • Static/type-level call sites: UpperCaseReceiver.method() → resolved.
///   • Implicit-self calls inside methods: method() → resolved to currentType.
///   • Instance method calls on explicitly-typed locals and parameters:
///       let svc: PaymentService = …  →  svc.charge() resolved.
///       func process(svc: PaymentService)  →  svc.validate() resolved.
///       Closure parameters with explicit type annotations: also resolved.
///   • Type inheritance / protocol conformance: class Dog: Animal, Serializable
///       → IsImplementation edges in Travsr graph for full blast radius.
///   • Unresolvable instance calls (inferred-type locals, chained calls) are
///     omitted, since a full IndexStore integration would be needed for those.
///
/// Usage:
///   swift-index-emitter <root-path> <output-json-path>
///
/// Build (required before travsr Phase B activates Swift):
///   cd packages/swift-index-emitter && swift build -c release
///
/// Symbol scheme:
///   "swift::<TypeName>"               : class / struct / enum / protocol / actor
///   "swift::<TypeName>.<memberName>"  : method, property, init, subscript, case
///   "swift::<name>"                   : top-level function or variable

import Foundation
import SwiftParser
import SwiftSyntax
#if canImport(Glibc)
import Glibc
#endif

// ── Data types ────────────────────────────────────────────────────────────────

struct Definition: Encodable {
    let symbol: String
    let kind: String
    let line: Int
    let endLine: Int

    enum CodingKeys: String, CodingKey {
        case symbol, kind, line
        case endLine = "end_line"
    }
}

struct Reference: Encodable {
    let symbol: String
    let line: Int
    /// Whether this occurrence is a call site.
    ///
    /// `true` (the default) makes the Rust wrapper set `ScipRef.is_call`, which
    /// `travsr-store::write_scip_attributed_batch` turns into a `ref/call` edge
    /// from the enclosing function. Type-position uses (annotations, parameter
    /// and return types, generic arguments, conformances, the receiver type of a
    /// qualified access) are NOT calls: they must record only their occurrence,
    /// so `find_references` enumerates them while `get_callers` and blast radius
    /// stay a call graph.
    ///
    /// Serialised only when `false`, so the field is additive: a wrapper built
    /// before it existed reads no key and keeps today's `true` default.
    let isCall: Bool

    init(symbol: String, line: Int, isCall: Bool = true) {
        self.symbol = symbol
        self.line = line
        self.isCall = isCall
    }

    enum CodingKeys: String, CodingKey {
        case symbol, line
        case isCall = "is_call"
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(symbol, forKey: .symbol)
        try c.encode(line, forKey: .line)
        if !isCall { try c.encode(false, forKey: .isCall) }
    }
}

/// Type-level inheritance or protocol conformance.
/// `child` depends on `parent`: a change to `parent` may break `child`.
/// Emitted by Travsr as an IsImplementation edge: Edge(child, parent, IsImplementation).
struct Inheritance: Encodable {
    let child: String   // e.g. "swift::Dog"
    let parent: String  // e.g. "swift::Animal" or "swift::Serializable"
}

struct Document: Encodable {
    let path: String
    let definitions: [Definition]
    let references: [Reference]
    let inheritances: [Inheritance]
}

struct Output: Encodable {
    let version: Int
    let documents: [Document]
}

// ── Entry point ───────────────────────────────────────────────────────────────

let args = CommandLine.arguments
guard args.count >= 3 else {
    fputs("usage: swift-index-emitter <root-path> <output-json-path>\n", stderr)
    exit(1)
}

// Use realpath() to resolve all symlinks (e.g. /tmp → /private/tmp on macOS),
// ensuring the root prefix matches the canonical paths returned by FileManager.
var _realpathBuf = [CChar](repeating: 0, count: Int(PATH_MAX))
let _rootResolved = realpath(args[1], &_realpathBuf).map { String(cString: $0) }
let rootURL = URL(fileURLWithPath: _rootResolved ?? args[1])
let outputPath = args[2]

guard FileManager.default.fileExists(atPath: rootURL.path) else {
    fputs("root path does not exist: \(rootURL.path)\n", stderr)
    exit(1)
}

var swiftFiles: [URL] = []
if let enumerator = FileManager.default.enumerator(
    at: rootURL,
    includingPropertiesForKeys: [.isRegularFileKey],
    options: [.skipsHiddenFiles, .skipsPackageDescendants]
) {
    for case let url as URL in enumerator {
        guard url.pathExtension == "swift" else { continue }
        guard !isGenerated(url.lastPathComponent) else { continue }
        guard !url.path.contains("/.build/") else { continue }
        swiftFiles.append(url)
    }
}

var documents: [Document] = []

for fileURL in swiftFiles.sorted(by: { $0.path < $1.path }) {
    let source: String
    do {
        source = try String(contentsOf: fileURL, encoding: .utf8)
    } catch {
        fputs("warning: could not read \(fileURL.path): \(error)\n", stderr)
        continue
    }

    let relPath = String(fileURL.path.dropFirst(rootURL.path.count))
        .drop(while: { $0 == "/" })
        .description

    let tree = Parser.parse(source: source)
    let converter = SourceLocationConverter(fileName: relPath, tree: tree)
    let visitor = ScipVisitor(converter: converter)
    visitor.walk(tree)

    if !visitor.definitions.isEmpty || !visitor.references.isEmpty || !visitor.inheritances.isEmpty {
        documents.append(Document(
            path: relPath,
            definitions: visitor.definitions,
            references: visitor.references,
            inheritances: visitor.inheritances
        ))
    }
}

let output = Output(version: 1, documents: documents)
let encoder = JSONEncoder()
encoder.outputFormatting = .sortedKeys
let data = try encoder.encode(output)
try data.write(to: URL(fileURLWithPath: outputPath))
fputs(
    "swift-index-emitter: \(documents.count) documents written to \(outputPath)\n",
    stderr
)

// ── Helpers ───────────────────────────────────────────────────────────────────

func isGenerated(_ name: String) -> Bool {
    name.hasSuffix(".generated.swift")
        || name.hasSuffix(".pb.swift")
        || name.hasSuffix(".grpc.swift")
}

// ── Visitor ───────────────────────────────────────────────────────────────────

final class ScipVisitor: SyntaxVisitor {
    let converter: SourceLocationConverter
    var definitions: [Definition] = []
    var references: [Reference] = []
    var inheritances: [Inheritance] = []

    // Stack of enclosing type names; extensions push the extended type name.
    private var typeStack: [String] = []
    private var currentType: String? { typeStack.last }

    // Scope stack for instance-call resolution.
    // Each frame maps a local name to its simple (unqualified) type name.
    // Pushed on function/init/closure entry, popped on exit.
    // Only populated for explicitly type-annotated bindings. Inferred types
    // are left unresolved rather than guessed.
    private var scopeStack: [[String: String]] = []

    // Stack of in-scope generic parameter names, one frame per declaration that
    // introduces a generic parameter clause. `struct Stack<Item>` and
    // `func map<Element>(...)` bind names that look exactly like type names, so
    // without this a repo that also defines a real `Item` or `Element` type gets
    // a reference to the wrong thing. Pushed and popped alongside typeStack /
    // scopeStack so nesting works.
    private var genericParamStack: [Set<String>] = []

    init(converter: SourceLocationConverter) {
        self.converter = converter
        super.init(viewMode: .sourceAccurate)
    }

    private func lineOf(_ node: some SyntaxProtocol) -> Int {
        node.startLocation(converter: converter).line
    }

    private func endLineOf(_ node: some SyntaxProtocol) -> Int {
        node.endLocation(converter: converter).line
    }

    // "swift::Type.member" when inside a type, "swift::name" at top level.
    private func memberSymbol(_ name: String) -> String {
        if let t = currentType { return "swift::\(t).\(name)" }
        return "swift::\(name)"
    }

    // ── Scope helpers ──────────────────────────────────────────────────────────

    private func pushScope() {
        scopeStack.append([:])
    }

    private func popScope() {
        if !scopeStack.isEmpty { scopeStack.removeLast() }
    }

    private func bindLocal(_ name: String, type typeName: String) {
        guard !scopeStack.isEmpty, !name.isEmpty, !typeName.isEmpty else { return }
        scopeStack[scopeStack.count - 1][name] = typeName
    }

    // Innermost-scope-first lookup.
    private func lookupType(_ name: String) -> String? {
        for frame in scopeStack.reversed() {
            if let t = frame[name] { return t }
        }
        return nil
    }

    // ── Generic parameter helpers ──────────────────────────────────────────────

    /// Push one frame holding the names bound by `clause` (an empty frame when
    /// the declaration is not generic, so every push has a matching pop), and
    /// record the constraint types: `<T: Proto>` is a real use of `Proto`.
    private func pushGenerics(_ clause: GenericParameterClauseSyntax?) {
        guard let clause = clause else {
            genericParamStack.append([])
            return
        }
        var names: Set<String> = []
        for param in clause.parameters {
            names.insert(param.name.text)
        }
        genericParamStack.append(names)
        // Recorded after the frame is pushed so a constraint that mentions an
        // earlier parameter of the same clause is not emitted as a type.
        for param in clause.parameters {
            recordTypeReference(param.inheritedType)
        }
    }

    private func popGenerics() {
        if !genericParamStack.isEmpty { genericParamStack.removeLast() }
    }

    private func isGenericParam(_ name: String) -> Bool {
        for frame in genericParamStack.reversed() where frame.contains(name) {
            return true
        }
        return false
    }

    // ── Type name extraction ───────────────────────────────────────────────────

    /// Return the simple (unqualified, non-generic) type name from a TypeSyntax.
    /// "Foo" → "Foo", "Foo?" → "Foo", "Foo!" → "Foo", "Foo<T>" → "Foo",
    /// "Module.Foo" → "Foo". Returns "" for function/tuple/array types.
    private func simpleTypeName(_ type: TypeSyntax) -> String {
        if let id = type.as(IdentifierTypeSyntax.self) {
            return id.name.text
        }
        if let member = type.as(MemberTypeSyntax.self) {
            return member.name.text
        }
        if let opt = type.as(OptionalTypeSyntax.self) {
            return simpleTypeName(opt.wrappedType)
        }
        if let iuo = type.as(ImplicitlyUnwrappedOptionalTypeSyntax.self) {
            return simpleTypeName(iuo.wrappedType)
        }
        return ""
    }

    // ── Type-position references (#830) ─────────────────────────────────────────

    /// Language builtins that never resolve to a user definition. Emitting a
    /// reference to one would simply be dropped by Travsr's ingestion (no
    /// matching def), so skipping them keeps the index lean without losing recall.
    private static let builtinTypes: Set<String> = [
        "Int", "Int8", "Int16", "Int32", "Int64",
        "UInt", "UInt8", "UInt16", "UInt32", "UInt64",
        "Float", "Float16", "Float32", "Float64", "Double",
        "Bool", "String", "Substring", "Character",
        "Void", "Any", "AnyObject", "AnyClass", "Never", "Self",
    ]

    /// Emit a reference for every named type used in `type` (#830). Type
    /// annotations, parameter and return types, generic arguments, and the
    /// element types of optionals/arrays/dictionaries were previously invisible:
    /// a file that only *uses* a type in these positions produced a
    /// definition-only document with zero references, so `find_references` on the
    /// most-used API types returned a confident zero. Mirrors the Dart emitter's
    /// type-position capture (travsr-lang #14).
    ///
    /// Syntactic only: the rightmost name of a qualified type (`Module.Foo` → Foo)
    /// is what Travsr's `swift::` scheme keys on. Function/metatype shapes are
    /// intentionally not descended into, to avoid emitting noise from `() -> Void`.
    private func recordTypeReference(_ type: TypeSyntax?) {
        guard let type = type else { return }
        if let id = type.as(IdentifierTypeSyntax.self) {
            let name = id.name.text
            if !name.isEmpty, !Self.builtinTypes.contains(name), !isGenericParam(name) {
                references.append(Reference(
                    symbol: "swift::\(name)", line: lineOf(id.name), isCall: false))
            }
            if let generics = id.genericArgumentClause {
                for arg in generics.arguments { recordTypeReference(arg.argument) }
            }
        } else if let member = type.as(MemberTypeSyntax.self) {
            let name = member.name.text
            if !name.isEmpty, !Self.builtinTypes.contains(name), !isGenericParam(name) {
                references.append(Reference(
                    symbol: "swift::\(name)", line: lineOf(member.name), isCall: false))
            }
            if let generics = member.genericArgumentClause {
                for arg in generics.arguments { recordTypeReference(arg.argument) }
            }
        } else if let opt = type.as(OptionalTypeSyntax.self) {
            recordTypeReference(opt.wrappedType)
        } else if let iuo = type.as(ImplicitlyUnwrappedOptionalTypeSyntax.self) {
            recordTypeReference(iuo.wrappedType)
        } else if let arr = type.as(ArrayTypeSyntax.self) {
            recordTypeReference(arr.element)
        } else if let dict = type.as(DictionaryTypeSyntax.self) {
            recordTypeReference(dict.key)
            recordTypeReference(dict.value)
        } else if let tuple = type.as(TupleTypeSyntax.self) {
            for el in tuple.elements { recordTypeReference(el.type) }
        } else if let attributed = type.as(AttributedTypeSyntax.self) {
            recordTypeReference(attributed.baseType)
        } else if let someOrAny = type.as(SomeOrAnyTypeSyntax.self) {
            recordTypeReference(someOrAny.constraint)
        }
    }

    // ── Parameter binding ──────────────────────────────────────────────────────

    private func bindParameters(_ params: FunctionParameterListSyntax) {
        for param in params {
            // #830: the parameter type is referenced regardless of the parameter
            // name, so emit the reference before the `_`-name guard that only
            // governs local binding for instance-call resolution.
            recordTypeReference(param.type)
            // Use the internal (second) name when present, else the first name.
            // func foo(_ val: T) → firstName="_", secondName="val" → bind "val"
            // func foo(with val: T) → firstName="with", secondName="val" → bind "val"
            // func foo(val: T) → firstName="val", secondName=nil → bind "val"
            let internalName: String
            if let second = param.secondName {
                internalName = second.text
            } else {
                internalName = param.firstName.text
            }
            guard internalName != "_", !internalName.isEmpty else { continue }
            let typeName = simpleTypeName(param.type)
            if !typeName.isEmpty { bindLocal(internalName, type: typeName) }
        }
    }

    // ── Inheritance emission ───────────────────────────────────────────────────

    /// Emit IsImplementation edges for all items in an inheritance clause.
    /// Both superclass inheritance (class Dog: Animal) and protocol conformance
    /// (class Dog: Serializable) are emitted the same way, since both make `child`
    /// depend on `parent` for blast radius purposes.
    private func emitInheritances(for childName: String, clause: InheritanceClauseSyntax?) {
        guard let clause = clause else { return }
        for inh in clause.inheritedTypes {
            let parentName = simpleTypeName(inh.type)
            guard !parentName.isEmpty, parentName != childName else { continue }
            inheritances.append(Inheritance(
                child: "swift::\(childName)",
                parent: "swift::\(parentName)"
            ))
            // #830: a base class or conformed protocol is also referenced here,
            // so find_references on the parent type includes the conformance site.
            recordTypeReference(inh.type)
        }
    }

    // ── Nominal type declarations ──────────────────────────────────────────────

    override func visit(_ node: ClassDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "class", line: lineOf(node.name), endLine: endLineOf(node.memberBlock.rightBrace)))
        pushGenerics(node.genericParameterClause)
        emitInheritances(for: name, clause: node.inheritanceClause)
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: ClassDeclSyntax) {
        typeStack.removeLast()
        popGenerics()
    }

    override func visit(_ node: StructDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "class", line: lineOf(node.name), endLine: endLineOf(node.memberBlock.rightBrace)))
        pushGenerics(node.genericParameterClause)
        emitInheritances(for: name, clause: node.inheritanceClause)
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: StructDeclSyntax) {
        typeStack.removeLast()
        popGenerics()
    }

    override func visit(_ node: EnumDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "class", line: lineOf(node.name), endLine: endLineOf(node.memberBlock.rightBrace)))
        pushGenerics(node.genericParameterClause)
        emitInheritances(for: name, clause: node.inheritanceClause)
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: EnumDeclSyntax) {
        typeStack.removeLast()
        popGenerics()
    }

    override func visit(_ node: ProtocolDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "protocol", line: lineOf(node.name), endLine: endLineOf(node.memberBlock.rightBrace)))
        emitInheritances(for: name, clause: node.inheritanceClause)
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: ProtocolDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: ActorDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "class", line: lineOf(node.name), endLine: endLineOf(node.memberBlock.rightBrace)))
        pushGenerics(node.genericParameterClause)
        emitInheritances(for: name, clause: node.inheritanceClause)
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: ActorDeclSyntax) {
        typeStack.removeLast()
        popGenerics()
    }

    override func visit(_ node: ExtensionDeclSyntax) -> SyntaxVisitorContinueKind {
        // Push the extended type name so extension members share symbols with
        // the original type's definitions (e.g. "swift::UserModel.validate").
        // Strip generic parameters: "Array<Element>" → "Array".
        let fullName = node.extendedType.trimmedDescription
        let typeName = fullName.components(separatedBy: "<").first ?? fullName
        // #830: the extended type is itself a type-position use, and an
        // extension's inheritance clause is a real conformance. Neither was
        // recorded, so `extension Foo: Proto {}` was invisible to both
        // find_references and the IsImplementation edges. Emitted before the
        // typeStack push so emitInheritances keys the child on `typeName`
        // rather than on an enclosing type.
        recordTypeReference(node.extendedType)
        emitInheritances(for: typeName, clause: node.inheritanceClause)
        typeStack.append(typeName)
        return .visitChildren
    }
    override func visitPost(_ node: ExtensionDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: TypeAliasDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        let ln = lineOf(node.name)
        definitions.append(Definition(symbol: memberSymbol(name), kind: "class", line: ln, endLine: ln))
        return .visitChildren
    }

    // ── Member declarations ────────────────────────────────────────────────────

    override func visit(_ node: FunctionDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        let endLine = node.body.map { endLineOf($0.rightBrace) } ?? lineOf(node.name)
        definitions.append(Definition(
            symbol: memberSymbol(name),
            kind: "function",
            line: lineOf(node.name),
            endLine: endLine
        ))
        pushScope()
        pushGenerics(node.genericParameterClause)
        if let t = currentType { bindLocal("self", type: t) }
        bindParameters(node.signature.parameterClause.parameters)
        // #830: the declared return type is a use of that type.
        recordTypeReference(node.signature.returnClause?.type)
        return .visitChildren
    }
    override func visitPost(_ node: FunctionDeclSyntax) {
        popGenerics()
        popScope()
    }

    override func visit(_ node: InitializerDeclSyntax) -> SyntaxVisitorContinueKind {
        if let t = currentType {
            let endLine = node.body.map { endLineOf($0.rightBrace) } ?? lineOf(node.initKeyword)
            definitions.append(Definition(
                symbol: "swift::\(t).init",
                kind: "constructor",
                line: lineOf(node.initKeyword),
                endLine: endLine
            ))
        }
        pushScope()
        pushGenerics(node.genericParameterClause)
        if let t = currentType { bindLocal("self", type: t) }
        bindParameters(node.signature.parameterClause.parameters)
        return .visitChildren
    }
    override func visitPost(_ node: InitializerDeclSyntax) {
        popGenerics()
        popScope()
    }

    override func visit(_ node: SubscriptDeclSyntax) -> SyntaxVisitorContinueKind {
        if let t = currentType {
            let ln = lineOf(node.subscriptKeyword)
            let endLine = node.accessorBlock.map { endLineOf($0.rightBrace) } ?? ln
            definitions.append(Definition(
                symbol: "swift::\(t).subscript",
                kind: "function",
                line: ln,
                endLine: endLine
            ))
        }
        pushGenerics(node.genericParameterClause)
        // #830: subscript parameter and result types are uses of those types.
        for param in node.parameterClause.parameters { recordTypeReference(param.type) }
        recordTypeReference(node.returnClause.type)
        return .visitChildren
    }
    override func visitPost(_ node: SubscriptDeclSyntax) { popGenerics() }

    override func visit(_ node: VariableDeclSyntax) -> SyntaxVisitorContinueKind {
        for binding in node.bindings {
            guard let idPat = binding.pattern.as(IdentifierPatternSyntax.self) else { continue }
            let name = idPat.identifier.text
            let ln = lineOf(idPat.identifier)
            definitions.append(Definition(
                symbol: memberSymbol(name),
                kind: typeStack.isEmpty ? "variable" : "field",
                line: ln,
                endLine: ln  // variables/fields are single-line declarations
            ))
            // Track explicit type annotation for instance-call resolution.
            // Only active inside a scope frame (i.e., inside a function body).
            if let typeAnn = binding.typeAnnotation {
                let typeName = simpleTypeName(typeAnn.type)
                if !typeName.isEmpty { bindLocal(name, type: typeName) }
                // #830: the annotation is also a use of that type.
                recordTypeReference(typeAnn.type)
            }
        }
        return .visitChildren
    }

    override func visit(_ node: EnumCaseDeclSyntax) -> SyntaxVisitorContinueKind {
        for el in node.elements {
            let name = el.name.text
            let ln = lineOf(el.name)
            definitions.append(Definition(
                symbol: memberSymbol(name),
                kind: "field",
                line: ln,
                endLine: ln  // enum cases are single-line
            ))
        }
        return .visitChildren
    }

    // ── Closure scope tracking ─────────────────────────────────────────────────

    override func visit(_ node: ClosureExprSyntax) -> SyntaxVisitorContinueKind {
        pushScope()
        if let sig = node.signature, let paramClause = sig.parameterClause {
            if case .parameterClause(let params) = paramClause {
                for param in params.parameters {
                    let name: String
                    if let second = param.secondName { name = second.text }
                    else { name = param.firstName.text }
                    // #830: as in bindParameters, the type is referenced
                    // regardless of the parameter name, so record it before the
                    // `_` guard that only governs local binding.
                    recordTypeReference(param.type)
                    guard name != "_", !name.isEmpty else { continue }
                    if let typeAnn = param.type {
                        let typeName = simpleTypeName(typeAnn)
                        if !typeName.isEmpty { bindLocal(name, type: typeName) }
                    }
                }
            }
        }
        return .visitChildren
    }
    override func visitPost(_ node: ClosureExprSyntax) { popScope() }

    // ── References (call sites) ────────────────────────────────────────────────

    override func visit(_ node: FunctionCallExprSyntax) -> SyntaxVisitorContinueKind {
        let ln = lineOf(node)

        if let memberAccess = node.calledExpression.as(MemberAccessExprSyntax.self) {
            let memberName = memberAccess.declName.baseName.text
            if let base = memberAccess.base {
                if let declRef = base.as(DeclReferenceExprSyntax.self) {
                    let baseName = declRef.baseName.text
                    if baseName.first?.isUppercase == true {
                        // Static or type method call: SomeType.method()
                        references.append(Reference(symbol: "swift::\(baseName).\(memberName)", line: ln))
                        // #830: the receiver type itself is used here too, so a
                        // query for `SomeType` finds this qualified access.
                        if !Self.builtinTypes.contains(baseName) {
                            references.append(Reference(
                                symbol: "swift::\(baseName)", line: ln, isCall: false))
                        }
                    } else {
                        // Instance call: instance.method()
                        // Resolve via scope if the variable has an explicit type annotation.
                        if let resolvedType = lookupType(baseName) {
                            references.append(Reference(
                                symbol: "swift::\(resolvedType).\(memberName)",
                                line: ln
                            ))
                        }
                        // Unresolvable (inferred type, chained call): skip rather than guess.
                    }
                }
                // Complex base (subscript, nested call, etc.): skip.
            } else {
                // No explicit base → implicit self inside a method body.
                if let t = currentType {
                    references.append(Reference(symbol: "swift::\(t).\(memberName)", line: ln))
                }
            }
        } else if let declRef = node.calledExpression.as(DeclReferenceExprSyntax.self) {
            let name = declRef.baseName.text
            if name.first?.isUppercase == true {
                // Constructor call: MyType() → the type itself, not its `.init`
                // member (#449). find_references/get_callers query by type name
                // ("ClassA", not "ClassA.init"), and every type is guaranteed to
                // have a `swift::TypeName` definition regardless of whether it
                // declares an explicit initializer, unlike `.init`, which only
                // exists in def_ids when the type has one.
                references.append(Reference(symbol: "swift::\(name)", line: ln))
            } else {
                // Top-level or local function call: foo()
                references.append(Reference(symbol: "swift::\(name)", line: ln))
            }
        }

        return .visitChildren
    }

    // ── References (non-call member accesses) ──────────────────────────────────

    override func visit(_ node: MemberAccessExprSyntax) -> SyntaxVisitorContinueKind {
        // Called-expressions are already handled (with implicit-self and
        // instance resolution) by visit(FunctionCallExprSyntax). Skip them
        // to avoid double emission.
        if let call = node.parent?.as(FunctionCallExprSyntax.self),
           call.calledExpression.id == node.id {
            return .visitChildren
        }
        guard let base = node.base, let declRef = base.as(DeclReferenceExprSyntax.self) else {
            // Complex or absent base (chained access, implicit member `.red`): skip.
            return .visitChildren
        }
        let memberName = node.declName.baseName.text
        let baseName = declRef.baseName.text
        let ln = lineOf(node)
        if baseName.first?.isUppercase == true {
            // Static member access without a call: ClassC.shared, Color.red.
            references.append(Reference(symbol: "swift::\(baseName).\(memberName)", line: ln))
            // #830: the receiver type itself is used here too.
            if !Self.builtinTypes.contains(baseName) {
                references.append(Reference(
                    symbol: "swift::\(baseName)", line: ln, isCall: false))
            }
        } else if let resolvedType = lookupType(baseName) {
            // Property access on an explicitly-typed local: svc.total.
            references.append(Reference(symbol: "swift::\(resolvedType).\(memberName)", line: ln))
        }
        // Unresolvable base (inferred type): skip rather than guess.
        return .visitChildren
    }
}
