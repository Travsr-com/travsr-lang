/// Travsr Phase B — Swift structural emitter using SwiftSyntax.
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
///     omitted — a full IndexStore integration would be needed for those.
///
/// Usage:
///   swift-index-emitter <root-path> <output-json-path>
///
/// Build (required before travsr Phase B activates Swift):
///   cd packages/swift-index-emitter && swift build -c release
///
/// Symbol scheme:
///   "swift::<TypeName>"               — class / struct / enum / protocol / actor
///   "swift::<TypeName>.<memberName>"  — method, property, init, subscript, case
///   "swift::<name>"                   — top-level function or variable

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
    // Only populated for explicitly type-annotated bindings — inferred types
    // are left unresolved rather than guessed.
    private var scopeStack: [[String: String]] = []

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

    // ── Parameter binding ──────────────────────────────────────────────────────

    private func bindParameters(_ params: FunctionParameterListSyntax) {
        for param in params {
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
    /// (class Dog: Serializable) are emitted the same way — both make `child`
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
        }
    }

    // ── Nominal type declarations ──────────────────────────────────────────────

    override func visit(_ node: ClassDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "class", line: lineOf(node.name), endLine: endLineOf(node.memberBlock.rightBrace)))
        emitInheritances(for: name, clause: node.inheritanceClause)
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: ClassDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: StructDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "class", line: lineOf(node.name), endLine: endLineOf(node.memberBlock.rightBrace)))
        emitInheritances(for: name, clause: node.inheritanceClause)
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: StructDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: EnumDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "class", line: lineOf(node.name), endLine: endLineOf(node.memberBlock.rightBrace)))
        emitInheritances(for: name, clause: node.inheritanceClause)
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: EnumDeclSyntax) { typeStack.removeLast() }

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
        emitInheritances(for: name, clause: node.inheritanceClause)
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: ActorDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: ExtensionDeclSyntax) -> SyntaxVisitorContinueKind {
        // Push the extended type name so extension members share symbols with
        // the original type's definitions (e.g. "swift::UserModel.validate").
        // Strip generic parameters: "Array<Element>" → "Array".
        let fullName = node.extendedType.trimmedDescription
        let typeName = fullName.components(separatedBy: "<").first ?? fullName
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
        if let t = currentType { bindLocal("self", type: t) }
        bindParameters(node.signature.parameterClause.parameters)
        return .visitChildren
    }
    override func visitPost(_ node: FunctionDeclSyntax) { popScope() }

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
        if let t = currentType { bindLocal("self", type: t) }
        bindParameters(node.signature.parameterClause.parameters)
        return .visitChildren
    }
    override func visitPost(_ node: InitializerDeclSyntax) { popScope() }

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
        return .visitChildren
    }

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
                // Constructor call: MyType() → matches InitializerDeclSyntax.
                references.append(Reference(symbol: "swift::\(name).init", line: ln))
            } else {
                // Top-level or local function call: foo()
                references.append(Reference(symbol: "swift::\(name)", line: ln))
            }
        }

        return .visitChildren
    }
}
