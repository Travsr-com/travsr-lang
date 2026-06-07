/// Travsr Phase B — Swift structural emitter using SwiftSyntax.
///
/// Parses every .swift file in <root> and emits a JSON index of definitions
/// and call-site references for Travsr's Phase B ingestion pipeline.
///
/// This is a parse-level (not type-resolved) analysis:
///   • All named declarations are accurately extracted.
///   • Static/type-level call sites (UpperCaseReceiver.method()) are resolved.
///   • Instance method calls on runtime-typed values are omitted — there is no
///     type inference without full compilation. Add IndexStore integration later
///     for full cross-file resolution.
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
}

struct Reference: Encodable {
    let symbol: String
    let line: Int
}

struct Document: Encodable {
    let path: String
    let definitions: [Definition]
    let references: [Reference]
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

    if !visitor.definitions.isEmpty || !visitor.references.isEmpty {
        documents.append(Document(
            path: relPath,
            definitions: visitor.definitions,
            references: visitor.references
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

    // Stack of enclosing type names; extensions push the extended type name.
    private var typeStack: [String] = []
    private var currentType: String? { typeStack.last }

    init(converter: SourceLocationConverter) {
        self.converter = converter
        super.init(viewMode: .sourceAccurate)
    }

    private func lineOf(_ node: some SyntaxProtocol) -> Int {
        node.startLocation(converter: converter).line
    }

    // "swift::Type.member" when inside a type, "swift::name" at top level.
    private func memberSymbol(_ name: String) -> String {
        if let t = currentType { return "swift::\(t).\(name)" }
        return "swift::\(name)"
    }

    // ── Nominal type declarations ──────────────────────────────────────────────

    override func visit(_ node: ClassDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "class", line: lineOf(node.name)))
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: ClassDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: StructDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "class", line: lineOf(node.name)))
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: StructDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: EnumDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "class", line: lineOf(node.name)))
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: EnumDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: ProtocolDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "protocol", line: lineOf(node.name)))
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: ProtocolDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: ActorDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: "swift::\(name)", kind: "class", line: lineOf(node.name)))
        typeStack.append(name)
        return .visitChildren
    }
    override func visitPost(_ node: ActorDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: ExtensionDeclSyntax) -> SyntaxVisitorContinueKind {
        // Push the extended type name so extension members share symbols with
        // the original type's definitions (e.g. "swift::UserModel.validate").
        let typeName = node.extendedType.trimmedDescription
        typeStack.append(typeName)
        return .visitChildren
    }
    override func visitPost(_ node: ExtensionDeclSyntax) { typeStack.removeLast() }

    override func visit(_ node: TypeAliasDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(symbol: memberSymbol(name), kind: "class", line: lineOf(node.name)))
        return .visitChildren
    }

    // ── Member declarations ────────────────────────────────────────────────────

    override func visit(_ node: FunctionDeclSyntax) -> SyntaxVisitorContinueKind {
        let name = node.name.text
        definitions.append(Definition(
            symbol: memberSymbol(name),
            kind: "function",
            line: lineOf(node.name)
        ))
        return .visitChildren
    }

    override func visit(_ node: InitializerDeclSyntax) -> SyntaxVisitorContinueKind {
        if let t = currentType {
            definitions.append(Definition(
                symbol: "swift::\(t).init",
                kind: "constructor",
                line: lineOf(node.initKeyword)
            ))
        }
        return .visitChildren
    }

    override func visit(_ node: SubscriptDeclSyntax) -> SyntaxVisitorContinueKind {
        if let t = currentType {
            definitions.append(Definition(
                symbol: "swift::\(t).subscript",
                kind: "function",
                line: lineOf(node.subscriptKeyword)
            ))
        }
        return .visitChildren
    }

    override func visit(_ node: VariableDeclSyntax) -> SyntaxVisitorContinueKind {
        for binding in node.bindings {
            guard let idPat = binding.pattern.as(IdentifierPatternSyntax.self) else { continue }
            let name = idPat.identifier.text
            definitions.append(Definition(
                symbol: memberSymbol(name),
                kind: typeStack.isEmpty ? "variable" : "field",
                line: lineOf(idPat.identifier)
            ))
        }
        return .visitChildren
    }

    override func visit(_ node: EnumCaseDeclSyntax) -> SyntaxVisitorContinueKind {
        for el in node.elements {
            let name = el.name.text
            definitions.append(Definition(
                symbol: memberSymbol(name),
                kind: "field",
                line: lineOf(el.name)
            ))
        }
        return .visitChildren
    }

    // ── References (call sites) ────────────────────────────────────────────────

    override func visit(_ node: FunctionCallExprSyntax) -> SyntaxVisitorContinueKind {
        let ln = lineOf(node)

        if let memberAccess = node.calledExpression.as(MemberAccessExprSyntax.self) {
            let memberName = memberAccess.declName.baseName.text
            if let base = memberAccess.base {
                if let declRef = base.as(DeclReferenceExprSyntax.self) {
                    let baseName = declRef.baseName.text
                    if baseName.first?.isUppercase == true {
                        // UpperCase receiver → static or type method call.
                        references.append(Reference(symbol: "swift::\(baseName).\(memberName)", line: ln))
                    }
                    // Lowercase → instance call; skip (no type info available).
                }
                // Complex base expression (subscript, nested call): skip.
            } else {
                // Implicit self inside a method body.
                if let t = currentType {
                    references.append(Reference(symbol: "swift::\(t).\(memberName)", line: ln))
                }
            }
        } else if let declRef = node.calledExpression.as(DeclReferenceExprSyntax.self) {
            // Direct call: foo() or MyType() (constructor).
            let name = declRef.baseName.text
            references.append(Reference(symbol: "swift::\(name)", line: ln))
        }

        return .visitChildren
    }

    override func visit(_ node: ImportDeclSyntax) -> SyntaxVisitorContinueKind {
        let modulePath = node.path.map { $0.name.text }.joined(separator: ".")
        if !modulePath.isEmpty {
            references.append(Reference(
                symbol: "import::\(modulePath)",
                line: lineOf(node)
            ))
        }
        return .visitChildren
    }
}
