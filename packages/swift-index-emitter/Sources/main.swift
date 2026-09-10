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
    /// 0-based column of the reference on its line (#813). SwiftSyntax reports
    /// a 1-based column, so this is that minus one. Travsr keeps it only where
    /// the line prefix is ASCII, so the unit needs no conversion here. The column
    /// names the referenced identifier, so a type-position use points at the type
    /// name, not the syntax that encloses it.
    let col: Int
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

    init(symbol: String, line: Int, col: Int, isCall: Bool = true) {
        self.symbol = symbol
        self.line = line
        self.col = col
        self.isCall = isCall
    }

    enum CodingKeys: String, CodingKey {
        case symbol, line, col
        case isCall = "is_call"
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(symbol, forKey: .symbol)
        try c.encode(line, forKey: .line)
        try c.encode(col, forKey: .col)
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
    /// Unit of every `col` in this output (#813). SwiftSyntax reports source
    /// locations in UTF-8 bytes, which is exactly the unit Travsr's occurrence
    /// store keeps, so no conversion is needed on either side. Declared rather
    /// than assumed by the consumer: it travels with the artifact, so if a
    /// future SwiftSyntax changes this, the value changes with it.
    let colUnit: String = "utf8"

    let version: Int
    let documents: [Document]

    enum CodingKeys: String, CodingKey {
        case colUnit = "col_unit"
        case version, documents
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Build identity of this emitter, reported by `--version`.
///
/// Keep in sync with the Cargo workspace version in `Cargo.toml`: the Rust
/// spawner (`crates/swift`) compares this against its own package version and
/// warns when they disagree, because `cargo build` does NOT rebuild this
/// binary and a stale emitter is otherwise indistinguishable from a current
/// one. Bump both together at release; CI fails when they disagree.
let emitterVersion = "0.4.2"

let args = CommandLine.arguments

if args.count >= 2, args[1] == "--version" {
    print("swift-index-emitter \(emitterVersion)")
    exit(0)
}

guard args.count >= 3 else {
    fputs("usage: swift-index-emitter <root-path> <output-json-path>\n", stderr)
    fputs("       swift-index-emitter --version\n", stderr)
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

// Two passes over the files. A generic pre-pass collects each type's generic
// parameter and associatedtype names so the emission pass can suppress those
// names inside extensions, which do not restate them (`extension Stack { … Item
// … }` must not reference a real type named `Item`). The map has to span the
// whole run before any file is emitted, since an extension can extend a type
// declared in another file. Each pass parses from disk one file at a time
// rather than holding every syntax tree at once: a SwiftSyntax tree is several
// times the size of its source, and this is a subprocess whose failure fails
// Phase B, so peak memory stays at one tree as it did before this pass existed.
func relativePath(_ fileURL: URL) -> String {
    String(fileURL.path.dropFirst(rootURL.path.count))
        .drop(while: { $0 == "/" })
        .description
}

let orderedFiles = swiftFiles.sorted(by: { $0.path < $1.path })

// Pre-pass: type name → its generic parameter / associatedtype names, the
// members each type declares, and each type's supertypes. Members and
// supertypes are collected here rather than during emission because the type
// that declares an inherited member usually lives in a different file.
let genericCollector = GenericCollector()
for fileURL in orderedFiles {
    guard let source = try? String(contentsOf: fileURL, encoding: .utf8) else { continue }
    genericCollector.walk(Parser.parse(source: source))
}
let typeGenerics = genericCollector.typeGenerics
let typeMembers = genericCollector.typeMembers
let typeParents = genericCollector.typeParents

// Emission pass.
var documents: [Document] = []
for fileURL in orderedFiles {
    let source: String
    do {
        source = try String(contentsOf: fileURL, encoding: .utf8)
    } catch {
        fputs("warning: could not read \(fileURL.path): \(error)\n", stderr)
        continue
    }

    let relPath = relativePath(fileURL)
    let tree = Parser.parse(source: source)
    let converter = SourceLocationConverter(fileName: relPath, tree: tree)
    let visitor = ScipVisitor(
        converter: converter,
        typeGenerics: typeGenerics,
        typeMembers: typeMembers,
        typeParents: typeParents,
        typeGenericConstraints: genericCollector.typeGenericConstraints,
        typePropertyTypes: genericCollector.typePropertyTypes
    )
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

// ── Generic-parameter pre-pass ──────────────────────────────────────────────────

/// Collects, per type name, the generic parameter names it introduces (and a
/// protocol's associatedtype names). An extension does not restate the extended
/// type's parameters, so `extension Stack { func peek() -> Item? }` looks
/// identical to a use of a real type `Item`. The emission pass consults this
/// map (keyed by the type's simple name) to suppress those names inside the
/// extension body. Collected across every file first, since the extension and
/// the type it extends can live in different files.
final class GenericCollector: SyntaxVisitor {
    var typeGenerics: [String: Set<String>] = [:]
    /// Type name -> the member names it declares, extensions included.
    ///
    /// A Swift static/type-method call is written on the receiver
    /// (`Concrete.parseAsRoot()`) but the member may be declared on a supertype
    /// or a protocol extension. The emitter keyed the symbol on the receiver, so
    /// every inherited call named a symbol no definition carried and the wrapper
    /// dropped it: 33% of all references on swift-argument-parser, and
    /// `references parseAsRoot` returned 0 of its 11 caller files. Collected in
    /// this pre-pass because the declaring type usually lives in another file.
    var typeMembers: [String: Set<String>] = [:]
    /// Type name -> (generic parameter or associatedtype name -> its
    /// constraint). `T.parse()` under `<T: ParsableCommand>` is really a call on
    /// `ParsableCommand`, so the constraint is the only real type the receiver
    /// can name. Without it such a call names `swift::T`, which matches no
    /// definition (or worse, an unrelated type that happens to be called `T`).
    var typeGenericConstraints: [String: [String: String]] = [:]
    /// Type name -> (property name -> the simple name of the type EXPLICITLY
    /// declared for it). Only annotated properties declared directly in a type's
    /// member block are recorded: `self.foo.bar()` can only be resolved when the
    /// source states what `foo` is, and an initializer expression is never read
    /// as evidence of a type. Collected here because the property and the call
    /// through it are often in different files.
    var typePropertyTypes: [String: [String: String]] = [:]
    /// Type name -> its declared superclass and conformances, extensions
    /// included (`extension Foo: Bar` adds `Bar` to `Foo`). Walked by
    /// `owningType(of:on:)` to find which type actually declares a member.
    ///
    /// An ordered array, not a `Set`: Swift seeds its `Hasher` randomly per
    /// process, so iterating a `Set` here gave a different supertype order in
    /// every run of the same unmodified repo. With two protocol default
    /// implementations of one member, the breadth-first walk then picked a
    /// different declarer, and the emitted symbol (and the edge target) flipped
    /// between runs. Declaration order is also the right tie-break: a
    /// superclass is written before the conformances, and the conformances in
    /// the order the source states them.
    var typeParents: [String: [String]] = [:]
    private var typeStack: [String] = []

    init() { super.init(viewMode: .sourceAccurate) }

    /// Record `Type: Parent, Other` so the member lookup can walk upward.
    private func recordParents(_ typeName: String, _ clause: InheritanceClauseSyntax?) {
        guard let clause = clause else { return }
        var parents = typeParents[typeName] ?? []
        for item in clause.inheritedTypes {
            let name = GenericCollector.simpleName(item.type)
            if !name.isEmpty, !parents.contains(name) { parents.append(name) }
        }
        typeParents[typeName] = parents
    }

    private func recordMember(_ name: String) {
        guard let owner = typeStack.last else { return }
        var members = typeMembers[owner] ?? []
        members.insert(name)
        typeMembers[owner] = members
    }

    /// Record the declared type of a property against its enclosing type, so
    /// `self.foo` inside `struct A` never reads a `foo` declared in `struct B`.
    private func recordPropertyType(_ name: String, _ typeName: String) {
        guard let owner = typeStack.last, !name.isEmpty, !typeName.isEmpty else { return }
        var props = typePropertyTypes[owner] ?? [:]
        props[name] = typeName
        typePropertyTypes[owner] = props
    }

    /// `Foo` from `Foo`, `Foo<T>`, `Foo?` or `Outer.Inner` (rightmost), matching
    /// how `ScipVisitor` keys extension and nested-type symbols.
    static func simpleName(_ type: TypeSyntax) -> String {
        if let id = type.as(IdentifierTypeSyntax.self) { return id.name.text }
        if let member = type.as(MemberTypeSyntax.self) { return member.name.text }
        if let opt = type.as(OptionalTypeSyntax.self) { return simpleName(opt.wrappedType) }
        if let iuo = type.as(ImplicitlyUnwrappedOptionalTypeSyntax.self) {
            return simpleName(iuo.wrappedType)
        }
        // `Foo.Type`: a property declared as `static var asCommand:
        // ParsableCommand.Type` is a receiver for `ParsableCommand`'s members,
        // exactly as `ScipVisitor.simpleTypeName` already unwraps it for a
        // parameter. Metatypes cannot appear in an inheritance clause or an
        // extended type, so the other callers are unaffected.
        if let meta = type.as(MetatypeTypeSyntax.self) {
            return simpleName(meta.baseType)
        }
        return ""
    }

    override func visit(_ node: FunctionDeclSyntax) -> SyntaxVisitorContinueKind {
        // Only a func directly inside a type's member block is a member. A
        // function nested in a method body is a local that happens to sit under
        // the same typeStack frame, and recording it made a bare call to a free
        // function of the same name resolve to `swift::T.name` instead.
        if node.parent?.is(MemberBlockItemSyntax.self) == true {
            recordMember(node.name.text)
        }
        return .visitChildren
    }

    override func visit(_ node: VariableDeclSyntax) -> SyntaxVisitorContinueKind {
        // A `var` directly inside a type's member block is a property; one
        // inside a function body is a local that happens to nest under the same
        // typeStack frame, and must not be recorded as a property of it.
        let isProperty = node.parent?.is(MemberBlockItemSyntax.self) == true
        guard isProperty else { return .visitChildren }
        for binding in node.bindings {
            if let pattern = binding.pattern.as(IdentifierPatternSyntax.self) {
                recordMember(pattern.identifier.text)
                if let annotation = binding.typeAnnotation {
                    recordPropertyType(
                        pattern.identifier.text,
                        GenericCollector.simpleName(annotation.type)
                    )
                }
            }
        }
        return .visitChildren
    }

    /// `extension ParsableCommand { static func parseAsRoot }` declares
    /// `parseAsRoot` on `ParsableCommand`, which is exactly the case the
    /// receiver-keyed symbol got wrong. Keyed on the rightmost name, as
    /// `ScipVisitor` does for the same declaration.
    override func visit(_ node: ExtensionDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = GenericCollector.simpleName(node.extendedType)
        recordParents(name, node.inheritanceClause)
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: ExtensionDeclSyntax) { typeStack.removeLast() }

    private func record(_ typeName: String, _ clause: GenericParameterClauseSyntax?) {
        guard let clause = clause else { return }
        var names = typeGenerics[typeName] ?? []
        var constraints = typeGenericConstraints[typeName] ?? [:]
        for param in clause.parameters {
            names.insert(param.name.text)
            if let inherited = param.inheritedType {
                let c = GenericCollector.simpleName(inherited)
                if !c.isEmpty { constraints[param.name.text] = c }
            }
        }
        typeGenerics[typeName] = names
        typeGenericConstraints[typeName] = constraints
    }

    override func visit(_ node: ClassDeclSyntax) -> SyntaxVisitorContinueKind {
        record(node.name.text, node.genericParameterClause)
        recordParents(node.name.text, node.inheritanceClause)
        typeStack.append(node.name.text)
        return .visitChildren
    }
    override func visitPost(_ node: ClassDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: StructDeclSyntax) -> SyntaxVisitorContinueKind {
        record(node.name.text, node.genericParameterClause)
        recordParents(node.name.text, node.inheritanceClause)
        typeStack.append(node.name.text)
        return .visitChildren
    }
    override func visitPost(_ node: StructDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: EnumDeclSyntax) -> SyntaxVisitorContinueKind {
        record(node.name.text, node.genericParameterClause)
        recordParents(node.name.text, node.inheritanceClause)
        typeStack.append(node.name.text)
        return .visitChildren
    }
    override func visitPost(_ node: EnumDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: ActorDeclSyntax) -> SyntaxVisitorContinueKind {
        record(node.name.text, node.genericParameterClause)
        recordParents(node.name.text, node.inheritanceClause)
        typeStack.append(node.name.text)
        return .visitChildren
    }
    override func visitPost(_ node: ActorDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: ProtocolDeclSyntax) -> SyntaxVisitorContinueKind {
        recordParents(node.name.text, node.inheritanceClause)
        typeStack.append(node.name.text)
        return .visitChildren
    }
    override func visitPost(_ node: ProtocolDeclSyntax) { typeStack.removeLast() }

    // `associatedtype Element` inside a protocol is a generic-like name that
    // shows up unrestated in the protocol's own extensions / default methods.
    override func visit(_ node: AssociatedTypeDeclSyntax) -> SyntaxVisitorContinueKind {
        if let owner = typeStack.last {
            var names = typeGenerics[owner] ?? []
            names.insert(node.name.text)
            typeGenerics[owner] = names
            if let inherited = node.inheritanceClause?.inheritedTypes.first?.type {
                let c = GenericCollector.simpleName(inherited)
                if !c.isEmpty {
                    var constraints = typeGenericConstraints[owner] ?? [:]
                    constraints[node.name.text] = c
                    typeGenericConstraints[owner] = constraints
                }
            }
        }
        return .visitChildren
    }
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

    // Names of ALL locals in scope (parameters and let/var bindings), whether or
    // not their type is known. A Swift local shadows a type of the same name, so
    // an uppercase-named local (`let Config = …; Config.count`) must not be read
    // as a static access on a type. Kept alongside scopeStack.
    private var scopeNames: [Set<String>] = []

    // Stack of in-scope generic parameter names, one frame per declaration that
    // introduces a generic parameter clause. `struct Stack<Item>` and
    // `func map<Element>(...)` bind names that look exactly like type names, so
    // without this a repo that also defines a real `Item` or `Element` type gets
    // a reference to the wrong thing. Pushed and popped alongside typeStack /
    // scopeStack so nesting works.
    private var genericParamStack: [Set<String>] = []

    // Type name → its generic parameter / associatedtype names, collected across
    // the whole run (see GenericCollector). Used to push the extended type's
    // parameters when entering an extension, which does not restate them.
    private let typeGenerics: [String: Set<String>]

    // Generic parameter names of common standard-library generic types, so
    // `extension Array { … Element … }` and friends do not emit a reference to a
    // user type that happens to share the name (Element, Key, Value, …). These
    // types are never in the repo's def set, so they are absent from typeGenerics.
    private static let stdlibTypeGenerics: [String: Set<String>] = [
        "Array": ["Element"], "ContiguousArray": ["Element"], "ArraySlice": ["Element"],
        "Set": ["Element"], "Sequence": ["Element"], "Collection": ["Element"],
        "Dictionary": ["Key", "Value"], "KeyValuePairs": ["Key", "Value"],
        "Optional": ["Wrapped"], "Result": ["Success", "Failure"],
        "Range": ["Bound"], "ClosedRange": ["Bound"],
        "Unmanaged": ["Instance"],
        "UnsafePointer": ["Pointee"], "UnsafeMutablePointer": ["Pointee"],
    ]

    /// From the pre-pass: which type declares which members, and each type's
    /// supertypes. Used by `owningType(of:on:)`.
    private let typeMembers: [String: Set<String>]
    private let typeParents: [String: [String]]
    private let typeGenericConstraints: [String: [String: String]]
    /// From the pre-pass: each type's properties whose type is explicitly
    /// declared in source. Used to resolve `self.<property>.<method>()`.
    private let typePropertyTypes: [String: [String: String]]
    /// Parallel to `genericParamStack`: the constraint for each generic name in
    /// that frame, when it declared one.
    private var genericConstraintStack: [[String: String]] = []

    init(
        converter: SourceLocationConverter,
        typeGenerics: [String: Set<String>] = [:],
        typeMembers: [String: Set<String>] = [:],
        typeParents: [String: [String]] = [:],
        typeGenericConstraints: [String: [String: String]] = [:],
        typePropertyTypes: [String: [String: String]] = [:]
    ) {
        self.converter = converter
        self.typeGenerics = typeGenerics
        self.typeMembers = typeMembers
        self.typeParents = typeParents
        self.typeGenericConstraints = typeGenericConstraints
        self.typePropertyTypes = typePropertyTypes
        super.init(viewMode: .sourceAccurate)
    }

    /// The type that actually declares `member`, starting at `recv` and walking
    /// its supertypes breadth-first so the nearest declarer wins.
    ///
    /// Returns `recv` unchanged when nothing in the chain declares it, i.e. an
    /// external or stdlib member. That keeps the existing behaviour for those:
    /// the symbol names no definition, the wrapper drops it, and the miss stays
    /// a safe miss rather than becoming a wrong edge.
    private func owningType(of member: String, on recv: String) -> String {
        if typeMembers[recv]?.contains(member) == true { return recv }
        var seen: Set<String> = [recv]
        var queue = typeParents[recv] ?? []
        while !queue.isEmpty {
            let t = queue.removeFirst()
            if !seen.insert(t).inserted { continue }
            if typeMembers[t]?.contains(member) == true { return t }
            queue.append(contentsOf: typeParents[t] ?? [])
        }
        return recv
    }

    /// The type explicitly declared for property `member` of `recv`, walking
    /// `recv`'s supertypes breadth-first as `owningType(of:on:)` does, so a
    /// property declared on a base class or a protocol resolves too.
    ///
    /// `nil` when nothing in the chain declares `member` with an explicit type
    /// annotation. That is the point: an inferred property type is not knowable
    /// at parse level, and the emitter says nothing rather than guessing one
    /// from the initializer expression or from the property name.
    private func declaredPropertyType(of member: String, on recv: String) -> String? {
        if let declared = typePropertyTypes[recv]?[member] { return declared }
        var seen: Set<String> = [recv]
        var queue = typeParents[recv] ?? []
        while !queue.isEmpty {
            let t = queue.removeFirst()
            if !seen.insert(t).inserted { continue }
            if let declared = typePropertyTypes[t]?[member] { return declared }
            queue.append(contentsOf: typeParents[t] ?? [])
        }
        return nil
    }

    private func colOf(_ node: some SyntaxProtocol) -> Int {
        node.startLocation(converter: converter).column - 1
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
        scopeNames.append([])
    }

    private func popScope() {
        if !scopeStack.isEmpty { scopeStack.removeLast() }
        if !scopeNames.isEmpty { scopeNames.removeLast() }
    }

    private func bindLocal(_ name: String, type typeName: String) {
        guard !scopeStack.isEmpty, !name.isEmpty, !typeName.isEmpty else { return }
        scopeStack[scopeStack.count - 1][name] = typeName
    }

    // Record a local name whether or not its type is known.
    private func noteLocalName(_ name: String) {
        guard !scopeNames.isEmpty, !name.isEmpty else { return }
        scopeNames[scopeNames.count - 1].insert(name)
    }

    private func isLocalName(_ name: String) -> Bool {
        for frame in scopeNames.reversed() where frame.contains(name) { return true }
        return false
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
            genericConstraintStack.append([:])
            return
        }
        var names: Set<String> = []
        var constraints: [String: String] = [:]
        for param in clause.parameters {
            names.insert(param.name.text)
            if let inherited = param.inheritedType {
                let c = GenericCollector.simpleName(inherited)
                if !c.isEmpty { constraints[param.name.text] = c }
            }
        }
        genericParamStack.append(names)
        genericConstraintStack.append(constraints)
        // Recorded after the frame is pushed so a constraint that mentions an
        // earlier parameter of the same clause is not emitted as a type.
        for param in clause.parameters {
            recordTypeReference(param.inheritedType)
        }
    }

    private func popGenerics() {
        if !genericConstraintStack.isEmpty { genericConstraintStack.removeLast() }
        if !genericParamStack.isEmpty { genericParamStack.removeLast() }
    }

    /// Push a frame with the extended type's generic parameters when entering an
    /// extension. An extension never restates them, so without this a use of one
    /// of those names inside the body (`extension Stack { func peek() -> Item? }`)
    /// is emitted as a reference to a real type `Item`. Always pushes exactly one
    /// frame (empty when the type is non-generic) so it balances popGenerics.
    private func pushExtensionGenerics(_ typeName: String) {
        let simpleName = typeName.components(separatedBy: ".").last ?? typeName
        let fromDecl = typeGenerics[simpleName] ?? []
        let fromStdlib = Self.stdlibTypeGenerics[simpleName] ?? []
        genericParamStack.append(fromDecl.union(fromStdlib))
        genericConstraintStack.append(typeGenericConstraints[simpleName] ?? [:])
    }

    /// The constraint a generic parameter or associatedtype declared, if any.
    private func genericConstraint(_ name: String) -> String? {
        for frame in genericConstraintStack.reversed() {
            if let c = frame[name] { return c }
        }
        return nil
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
        // `Foo.Type` (a metatype) is how a caller passes a type to dispatch
        // statically on: `func f(_ root: ParsableCommand.Type)` then
        // `root.parseAsRoot(args)`. Unwrapping to `Foo` makes the receiver
        // resolvable; the emitted symbol is the same `swift::Foo.member` shape a
        // static call on the type itself produces. Left unhandled, the binding
        // was never recorded, the receiver looked like an untyped local, and the
        // call was dropped.
        if let meta = type.as(MetatypeTypeSyntax.self) {
            return simpleTypeName(meta.baseType)
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
    /// is what Travsr's `swift::` scheme keys on. Function types (`(Foo) -> Bar`),
    /// protocol compositions (`Foo & Bar`) and metatypes (`Foo.Type`) are descended
    /// into as well; the `() -> Void` noise this once avoided is a non-issue since
    /// `Void` is already a builtin and skipped.
    private func recordTypeReference(_ type: TypeSyntax?) {
        guard let type = type else { return }
        if let id = type.as(IdentifierTypeSyntax.self) {
            let name = id.name.text
            if !name.isEmpty, !Self.builtinTypes.contains(name), !isGenericParam(name) {
                references.append(Reference(
                    symbol: "swift::\(name)", line: lineOf(id.name),
                    col: colOf(id.name), isCall: false))
            }
            if let generics = id.genericArgumentClause {
                for arg in generics.arguments { recordTypeReference(arg.argument) }
            }
        } else if let member = type.as(MemberTypeSyntax.self) {
            // `T.Element` / `Self.Element` is an associated-type projection, not a
            // reference to a real type named `Element`: when the base is a generic
            // parameter or `Self`, the member names a projection, so skip it (the
            // same false-positive class as a bare generic parameter). `Module.Foo`,
            // where the base is not a bound parameter, still records `swift::Foo`.
            let baseName = simpleTypeName(member.baseType)
            let isProjection = isGenericParam(baseName) || baseName == "Self"
            let name = member.name.text
            if !isProjection, !name.isEmpty, !Self.builtinTypes.contains(name), !isGenericParam(name) {
                references.append(Reference(
                    symbol: "swift::\(name)", line: lineOf(member.name),
                    col: colOf(member.name), isCall: false))
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
        } else if let fn = type.as(FunctionTypeSyntax.self) {
            for el in fn.parameters { recordTypeReference(el.type) }
            recordTypeReference(fn.returnClause.type)
        } else if let comp = type.as(CompositionTypeSyntax.self) {
            for el in comp.elements { recordTypeReference(el.type) }
        } else if let meta = type.as(MetatypeTypeSyntax.self) {
            recordTypeReference(meta.baseType)
        }
    }

    /// Emit references for the types named in a generic `where` clause. The
    /// left-hand side is usually one of the declaration's own generic parameters
    /// or associatedtypes (filtered by isGenericParam), the right-hand side a real
    /// constraint type. Called after the generic frame is pushed.
    private func recordWhereClause(_ clause: GenericWhereClauseSyntax?) {
        guard let clause = clause else { return }
        for req in clause.requirements {
            switch req.requirement {
            case .conformanceRequirement(let c):
                recordTypeReference(c.leftType)
                recordTypeReference(c.rightType)
            case .sameTypeRequirement(let s):
                recordTypeReference(s.leftType)
                recordTypeReference(s.rightType)
            case .layoutRequirement:
                break
            }
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
            noteLocalName(internalName)
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
        recordWhereClause(node.genericWhereClause)
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
        recordWhereClause(node.genericWhereClause)
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
        recordWhereClause(node.genericWhereClause)
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
        // A protocol's associatedtypes (collected in the pre-pass) act like
        // generic parameters inside its own body and where clause, so push them
        // as a frame to keep `func f() -> Element` from referencing a real type
        // named Element.
        genericParamStack.append(typeGenerics[name] ?? [])
        recordWhereClause(node.genericWhereClause)
        emitInheritances(for: name, clause: node.inheritanceClause)
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: ProtocolDeclSyntax) {
        typeStack.removeLast()
        popGenerics()
    }

    override func visit(_ node: ActorDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "class", line: lineOf(node.name), endLine: endLineOf(node.memberBlock.rightBrace)))
        pushGenerics(node.genericParameterClause)
        recordWhereClause(node.genericWhereClause)
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
        // Strip generic parameters ("Array<Element>" -> "Array") and, for a
        // nested type ("Outer.Inner"), key on the rightmost name, since that is
        // how nested type definitions are emitted (`swift::Inner`); otherwise the
        // members and the IsImplementation child would name `swift::Outer.Inner`,
        // which no definition carries and the wrapper drops.
        let fullName = node.extendedType.trimmedDescription
        let strippedGenerics = fullName.components(separatedBy: "<").first ?? fullName
        let typeName = strippedGenerics.components(separatedBy: ".").last ?? strippedGenerics
        // #830: the extended type is itself a type-position use, and an
        // extension's inheritance clause is a real conformance. Neither was
        // recorded, so `extension Foo: Proto {}` was invisible to both
        // find_references and the IsImplementation edges. Emitted before the
        // typeStack push so emitInheritances keys the child on `typeName`
        // rather than on an enclosing type.
        recordTypeReference(node.extendedType)
        emitInheritances(for: typeName, clause: node.inheritanceClause)
        // The extended type's generic parameters are in scope inside the body but
        // never restated by the extension, so bring them in before visiting it.
        pushExtensionGenerics(typeName)
        recordWhereClause(node.genericWhereClause)
        typeStack.append(typeName)
        return .visitChildren
    }
    override func visitPost(_ node: ExtensionDeclSyntax) {
        typeStack.removeLast()
        popGenerics()
    }

    override func visit(_ node: TypeAliasDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        let ln = lineOf(node.name)
        definitions.append(Definition(symbol: memberSymbol(name), kind: "class", line: ln, endLine: ln))
        pushGenerics(node.genericParameterClause)
        // The aliased type is a real use: `typealias Handler = (Foo) -> Bar`.
        recordTypeReference(node.initializer.value)
        recordWhereClause(node.genericWhereClause)
        return .visitChildren
    }
    override func visitPost(_ node: TypeAliasDeclSyntax) { popGenerics() }

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
        recordWhereClause(node.genericWhereClause)
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
        recordWhereClause(node.genericWhereClause)
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
        recordWhereClause(node.genericWhereClause)
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
            // Note the name as an in-scope local (a no-op at type level, where
            // there is no scope frame) so an uppercase-named local shadows a type.
            noteLocalName(name)
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
            // Associated-value types are uses: `case loaded(Payload)`.
            if let params = el.parameterClause {
                for p in params.parameters { recordTypeReference(p.type) }
            }
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
                    noteLocalName(name)
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

    // ── Types used in expression position ────────────────────────────────────────

    // A type named in expression position is a use of that type. This is how the
    // cast target of `x as Foo` / `x as? Foo` / `x is Foo` reaches us: the parser
    // leaves the sequence unfolded, so the cast operand is a TypeExprSyntax rather
    // than the folded As/IsExprSyntax. Matching on TypeExprSyntax also avoids
    // depending on operator folding having run.
    override func visit(_ node: TypeExprSyntax) -> SyntaxVisitorContinueKind {
        recordTypeReference(node.type)
        return .visitChildren
    }

    // ── References (call sites) ────────────────────────────────────────────────

    override func visit(_ node: FunctionCallExprSyntax) -> SyntaxVisitorContinueKind {
        let ln = lineOf(node)

        if let memberAccess = node.calledExpression.as(MemberAccessExprSyntax.self) {
            let memberName = memberAccess.declName.baseName.text
            // #813: the column names the identifier that is being referenced,
            // not the start of the whole call expression. For `svc.charge()`
            // the referenced symbol is `charge`, so pointing at `svc` would
            // send the editor's definition provider to the receiver variable
            // (or, for `ClassC.registerEnvironments()`, to the type) instead of
            // the member. Every other producer travsr reads reports the
            // referenced identifier's own start.
            let cl = colOf(memberAccess.declName.baseName)
            if let base = memberAccess.base {
                if let declRef = base.as(DeclReferenceExprSyntax.self) {
                    let baseName = declRef.baseName.text
                    if let resolvedType = lookupType(baseName) {
                        // Instance call on an explicitly-typed local: instance.method().
                        // Checked before the uppercase heuristic so an uppercase-named
                        // local (`let Config = …; Config.count`) is resolved as an
                        // instance access, not mistaken for a static access on a type.
                        // Keyed on the declaring type, as in the static path: a
                        // typed local calling an inherited member is the same
                        // problem as `Concrete.inheritedStatic()`.
                        references.append(Reference(
                            symbol: "swift::\(owningType(of: memberName, on: resolvedType)).\(memberName)",
                            line: ln,
                            col: cl
                        ))
                    } else if isLocalName(baseName) {
                        // In-scope local of unknown (inferred) type: the member
                        // cannot be resolved, and the local shadows any type of the
                        // same name, so an uppercase name here is not a type access.
                    } else if isGenericParam(baseName) {
                        // `T.parse()` / `Command.parseAsRoot()`: the receiver is a
                        // generic parameter or associatedtype, which names no real
                        // type. #830's receiver capture emitted `swift::Command`
                        // here, resolving to unrelated types that share the name.
                        // Its constraint IS a real type and is where the member is
                        // declared, so resolve through that; with no constraint
                        // there is nothing to say, so say nothing. The receiver
                        // type reference is never emitted either way.
                        if let constraint = genericConstraint(baseName) {
                            let owner = owningType(of: memberName, on: constraint)
                            references.append(Reference(
                                symbol: "swift::\(owner).\(memberName)", line: ln, col: cl))
                        }
                    } else if baseName.first?.isUppercase == true {
                        // Static or type method call: SomeType.method(). Keyed on
                        // the type that DECLARES the member, which for an
                        // inherited or protocol-extension member is not the
                        // receiver written at the call site.
                        let owner = owningType(of: memberName, on: baseName)
                        references.append(Reference(symbol: "swift::\(owner).\(memberName)", line: ln, col: cl))
                        // #830: the receiver type itself is used here too, so a
                        // query for `SomeType` finds this qualified access. #813: the
                        // column names the receiver type, so point at the base.
                        // Stays keyed on the receiver as written: that is the type
                        // the source names here.
                        if !Self.builtinTypes.contains(baseName) {
                            references.append(Reference(
                                symbol: "swift::\(baseName)", line: ln,
                                col: colOf(declRef.baseName), isCall: false))
                        }
                    }
                    // Unresolvable (inferred-type local, chained call): skip rather than guess.
                } else if let propertyAccess = base.as(MemberAccessExprSyntax.self),
                          let propertyBase = propertyAccess.base?
                              .as(DeclReferenceExprSyntax.self),
                          propertyBase.baseName.text == "self",
                          let selfType = lookupType("self"),
                          let propertyType = declaredPropertyType(
                              of: propertyAccess.declName.baseName.text,
                              on: selfType
                          ),
                          !isGenericParam(propertyType) {
                    // `self.<property>.<method>()`. The receiver is one level of
                    // member access deeper than the paths above, so it fell into
                    // the complex-base skip: `self.asCommand.parseAsRoot(...)`
                    // was the last miss on swift-argument-parser. Resolvable only
                    // when the property's own type is written in source, which
                    // the pre-pass table records per enclosing type. Keyed on the
                    // declaring type as every other resolved path is, so an
                    // inherited or protocol-extension member still lands on the
                    // symbol its definition carries.
                    // A property whose declared type is the enclosing type's own
                    // generic parameter (`struct Box<Element> { var item: Element }`)
                    // names no real type, so `self.item.foo()` must not resolve to
                    // a repo type that happens to be called `Element`. Same guard
                    // the sibling receiver branches apply.
                    references.append(Reference(
                        symbol: "swift::\(owningType(of: memberName, on: propertyType)).\(memberName)",
                        line: ln,
                        col: cl
                    ))
                }
                // Complex base (subscript, nested call, deeper chain): skip.
            } else {
                // No explicit base → implicit self inside a method body.
                if let t = currentType {
                    references.append(Reference(symbol: "swift::\(t).\(memberName)", line: ln, col: cl))
                }
            }
        } else if let declRef = node.calledExpression.as(DeclReferenceExprSyntax.self) {
            let name = declRef.baseName.text
            // The name token rather than the call expression: identical for a
            // bare `Foo()` / `foo()`, but stated the same way as the member
            // paths above so the rule does not have to be re-derived.
            let cl = colOf(declRef.baseName)
            if isGenericParam(name) {
                // `T(argument:)` constructs whatever the generic parameter is
                // bound to, which is not a type named `T`. Emitting it made
                // `references T` resolve to unrelated types that share the name.
            } else if name.first?.isUppercase == true {
                // Constructor call: MyType() → the type itself, not its `.init`
                // member (#449). find_references/get_callers query by type name
                // ("ClassA", not "ClassA.init"), and every type is guaranteed to
                // have a `swift::TypeName` definition regardless of whether it
                // declares an explicit initializer, unlike `.init`, which only
                // exists in def_ids when the type has one.
                references.append(Reference(symbol: "swift::\(name)", line: ln, col: cl))
            } else if let t = currentType,
                      typeMembers[owningType(of: name, on: t)]?.contains(name) == true {
                // Bare `foo()` inside a type or extension body is implicit-self
                // or same-type static dispatch, not a top-level function:
                // `parseAsRoot(arguments)` inside `extension ParsableCommand`
                // named `swift::parseAsRoot`, which no definition carries. Only
                // taken when the enclosing type's chain really declares it,
                // otherwise it is a free function and keeps the bare symbol.
                let owner = owningType(of: name, on: t)
                references.append(Reference(symbol: "swift::\(owner).\(name)", line: ln, col: cl))
            } else {
                // Top-level or local function call: foo()
                references.append(Reference(symbol: "swift::\(name)", line: ln, col: cl))
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
        // #813: as in the call path, the column names the member, not the base.
        let cl = colOf(node.declName.baseName)
        if let resolvedType = lookupType(baseName) {
            // Property access on an explicitly-typed local: svc.total. Checked
            // before the uppercase heuristic so an uppercase-named local
            // (`let Config: Foo = …; Config.count`) is not mistaken for a type access.
            references.append(Reference(
                symbol: "swift::\(owningType(of: memberName, on: resolvedType)).\(memberName)",
                line: ln, col: cl))
        } else if isLocalName(baseName) {
            // In-scope local of unknown (inferred) type: it shadows any type of the
            // same name, so an uppercase base here is not a static type access.
        } else if isGenericParam(baseName) {
            // Generic parameter / associatedtype receiver: names no real type.
            // See the matching branch in the call path.
            if let constraint = genericConstraint(baseName) {
                let owner = owningType(of: memberName, on: constraint)
                references.append(Reference(
                    symbol: "swift::\(owner).\(memberName)", line: ln, col: cl))
            }
        } else if baseName.first?.isUppercase == true {
            // Static member access without a call: ClassC.shared, Color.red.
            // Keyed on the declaring type, as in the call path.
            let owner = owningType(of: memberName, on: baseName)
            references.append(Reference(symbol: "swift::\(owner).\(memberName)", line: ln, col: cl))
            // #830: the receiver type itself is used here too. #813: the column
            // names the receiver type, so point at the base.
            if !Self.builtinTypes.contains(baseName) {
                references.append(Reference(
                    symbol: "swift::\(baseName)", line: ln,
                    col: colOf(declRef.baseName), isCall: false))
            }
        }
        // Unresolvable base (inferred type): skip rather than guess.
        return .visitChildren
    }
}
