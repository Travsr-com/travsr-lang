import SwiftParser
import SwiftSyntax
import XCTest

@testable import swift_index_emitter

/// Symbol-resolution tests over inline sources.
///
/// The reference COUNT does not move when a resolution change retargets a
/// symbol, so the CI smoke test cannot see one. These pin the four resolution
/// rules that a change would silently flip.
final class SymbolResolutionTests: XCTestCase {
    /// Run the emitter's two passes over inline sources and return every
    /// reference symbol, in emission order. Files are visited in sorted name
    /// order, exactly as the binary orders them.
    private func referenceSymbols(_ sources: [String: String]) -> [String] {
        let names = sources.keys.sorted()
        let collector = GenericCollector()
        for name in names {
            collector.walk(Parser.parse(source: sources[name] ?? ""))
        }
        var symbols: [String] = []
        for name in names {
            let tree = Parser.parse(source: sources[name] ?? "")
            let visitor = ScipVisitor(
                converter: SourceLocationConverter(fileName: name, tree: tree),
                typeGenerics: collector.typeGenerics,
                typeMembers: collector.typeMembers,
                typeParents: collector.typeParents,
                typeGenericConstraints: collector.typeGenericConstraints,
                typePropertyTypes: collector.typePropertyTypes
            )
            visitor.walk(tree)
            symbols.append(contentsOf: visitor.references.map { $0.symbol })
        }
        return symbols
    }

    /// Two protocol extensions declare the same member and one type conforms to
    /// both, so the supertype walk has to break a tie. The tie-break is the
    /// order the conformances are WRITTEN. `typeParents` was a `Set`, whose
    /// iteration order Swift reseeds per process, so the emitted symbol used to
    /// differ between runs of the same unmodified repo.
    ///
    /// A unit test runs in one process and so cannot observe the reseeding
    /// directly; what it pins is the rule that replaced it.
    func testDiamondConformanceResolvesToFirstDeclaredProtocol() {
        let symbols = referenceSymbols([
            "A.swift": """
            protocol ProtoA {}
            protocol ProtoB {}
            extension ProtoA { func run() {} }
            extension ProtoB { func run() {} }
            struct Foo: ProtoA, ProtoB {}
            func drive(_ f: Foo) { f.run() }
            """
        ])
        XCTAssertTrue(
            symbols.contains("swift::ProtoA.run"),
            "the first conformance as written must win; got \(symbols)"
        )
        XCTAssertFalse(
            symbols.contains("swift::ProtoB.run"),
            "the second conformance must not win; got \(symbols)"
        )
    }

    /// A nested type is keyed on its own simple name, so two enclosing types can
    /// declare the same nested name and share one symbol. Pinned because it is
    /// the ambiguity a qualified-symbol change would have to address.
    func testNestedTypeNameIsSharedAcrossEnclosingTypes() {
        let symbols = referenceSymbols([
            "A.swift": """
            struct Outer1 { struct Inner { static func ping() {} } }
            struct Outer2 { struct Inner { static func pong() {} } }
            func drive() {
                Inner.ping()
                Inner.pong()
            }
            """
        ])
        XCTAssertTrue(
            symbols.contains("swift::Inner.ping"),
            "nested type keyed on its simple name; got \(symbols)"
        )
        XCTAssertTrue(
            symbols.contains("swift::Inner.pong"),
            "both enclosing types contribute members to the same symbol; got \(symbols)"
        )
    }

    /// A property whose declared type is the enclosing type's generic parameter
    /// names no real type, so a call through it must not land on a repo type
    /// that happens to share the parameter's name.
    func testGenericParameterPropertyTypeDoesNotResolveToARealType() {
        let symbols = referenceSymbols([
            "A.swift": """
            struct Element { func foo() {} }
            struct Box<Element> {
                var item: Element
                func use() { self.item.foo() }
            }
            """
        ])
        XCTAssertFalse(
            symbols.contains("swift::Element.foo"),
            "a generic-parameter property type must not resolve to the real Element; got \(symbols)"
        )
    }

    /// A local `let` and a nested `func` inside a method body are not members of
    /// the enclosing type, so a bare call elsewhere in that type still names the
    /// free function.
    func testBareCallToFreeFunctionIsNotCapturedByALocalOfTheSameName() {
        let symbols = referenceSymbols([
            "A.swift": """
            func tick() {}
            func tock() {}
            struct Runner {
                func setup() {
                    let tick = 0
                    _ = tick
                    func tock() {}
                    tock()
                }
                func go() { tick() }
            }
            """
        ])
        XCTAssertTrue(
            symbols.contains("swift::tick"),
            "a local named tick must not make the free function a member; got \(symbols)"
        )
        XCTAssertFalse(
            symbols.contains("swift::Runner.tick"),
            "a local must never be recorded as a member of its enclosing type; got \(symbols)"
        )
        XCTAssertFalse(
            symbols.contains("swift::Runner.tock"),
            "a nested function must never be recorded as a member; got \(symbols)"
        )
    }
}
