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
                typePropertyTypes: collector.typePropertyTypes,
                typePaths: collector.typePaths
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

    /// Symbols are emitted on a type's simple name, so two enclosing types that
    /// declare the same nested name share one symbol. The member tables are
    /// keyed on the nesting path and are no longer shared, but a receiver
    /// written as the bare name could mean either type, so the symbol falls back
    /// to the name as written and both members still land on it.
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
            "symbols are emitted on the simple name; got \(symbols)"
        )
        XCTAssertTrue(
            symbols.contains("swift::Inner.pong"),
            "an ambiguous receiver keeps the name as written; got \(symbols)"
        )
    }

    /// Two enclosing types declaring the same nested name must not share one
    /// property table: `self.svc` in `Outer2.Inner` used to read the `svc` that
    /// only `Outer1.Inner` declares and emit a call on a type `Outer2.Inner`
    /// never names. The type that does declare it still resolves.
    func testNestedTypesDoNotShareAPropertyTable() {
        let symbols = referenceSymbols([
            "A.swift": """
            struct Outer1 { struct Inner {
                var svc: Svc
                func use() { self.svc.ping() }
            } }
            struct Outer2 { struct Inner {
                func use() { self.svc.ping() }
            } }
            struct Svc { func ping() {} }
            """
        ])
        XCTAssertEqual(
            symbols.filter { $0 == "swift::Svc.ping" }.count, 1,
            "only the Inner that declares svc may resolve the call; got \(symbols)"
        )
    }

    /// The property is declared on a BASE class, whose generic parameter is what
    /// `command` names. The guard used to ask the generic parameters of the type
    /// being visited, which is `Sub`, so the call resolved to whatever real type
    /// happens to be called `Command`.
    func testInheritedGenericParameterPropertyTypeDoesNotResolveToARealType() {
        let symbols = referenceSymbols([
            "A.swift": """
            protocol Command { func run() }
            class Base<Command> {
                var command: Command
                init(command: Command) { self.command = command }
            }
            class Sub: Base<Int> { func go() { self.command.run() } }
            struct RealThing: Command { func run() {} }
            """
        ])
        XCTAssertFalse(
            symbols.contains("swift::Command.run"),
            "a base class's generic parameter must not resolve to the real Command; got \(symbols)"
        )
    }

    /// A local whose written type is a generic parameter names no real type, so
    /// the call resolves through the parameter's constraint or not at all. The
    /// metatype form is the one that regressed: unwrapping `T.Type` to `T` bound
    /// the local, and the branch had no generic guard, so it emitted `swift::T`.
    func testGenericParameterLocalReceiverDoesNotNameTheParameter() {
        let symbols = referenceSymbols([
            "A.swift": """
            protocol Command { func run() }
            struct T { func run() {} }
            func viaMetatype<T: Command>(_ t: T.Type) { t.run() }
            func viaLocal<T: Command>(_ t: T) { t.run() }
            func viaUnconstrained<U>(_ u: U) { u.run() }
            func viaWhere<W>(_ w: W.Type) where W: Command { w.run() }
            """
        ])
        XCTAssertFalse(
            symbols.contains("swift::T.run"),
            "a generic parameter receiver must not name the real T; got \(symbols)"
        )
        XCTAssertFalse(
            symbols.contains("swift::U.run"),
            "an unconstrained parameter names no type at all; got \(symbols)"
        )
        XCTAssertEqual(
            symbols.filter { $0 == "swift::Command.run" }.count, 3,
            "every constrained receiver resolves through the constraint, and a where clause "
                + "constrains exactly as an inline one does; got \(symbols)"
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

    /// The three receiver shapes whose type the source states without writing a
    /// type annotation. Only the annotated local used to resolve, so a call
    /// through a stored property, through `let e = Engine()`, or on a fresh
    /// `Engine()` was dropped: 1 of 4 instance calls on the commonest shapes.
    func testReceiverTypeStatedWithoutAnAnnotationResolves() {
        let symbols = referenceSymbols([
            "A.swift": """
            class Engine { func start() {} }
            class Uses {
                var prop: Engine = Engine()
                func viaProp() { prop.start() }
                func viaLocalInferred() { let e = Engine(); e.start() }
                func viaLocalAnnotated() { let e: Engine = Engine(); e.start() }
                func viaFresh() { Engine().start() }
            }
            """
        ])
        XCTAssertEqual(
            symbols.filter { $0 == "swift::Engine.start" }.count, 4,
            "a stored property, an inferred local, an annotated local and a fresh "
                + "instance all name Engine; got \(symbols)"
        )
    }

    /// A receiver whose type the source never states must still resolve to
    /// nothing. A factory return type is not knowable at parse level, and a
    /// property with no annotation says nothing about its own type either, so
    /// neither may be read off the name of the thing that produced it.
    func testReceiverTypeTheSourceNeverStatesStaysUnresolved() {
        let symbols = referenceSymbols([
            "A.swift": """
            class Engine { func start() {} }
            func makeEngine() -> Engine { Engine() }
            class Uses {
                var inferred = makeEngine()
                func viaFactoryLocal() { let e = makeEngine(); e.start() }
                func viaInferredProperty() { inferred.start() }
            }
            """
        ])
        XCTAssertFalse(
            symbols.contains("swift::Engine.start"),
            "a factory result and an unannotated property state no type; got \(symbols)"
        )
    }

    /// `T()` inside a generic constructs whatever T is bound to, not a type
    /// called `T`. The constructor-initializer binding must apply the same guard
    /// every other receiver path does, or a repo type sharing the name collects
    /// the edge.
    func testGenericParameterConstructorDoesNotBindTheRealType() {
        let symbols = referenceSymbols([
            "A.swift": """
            struct T { func run() {} }
            func make<T>() { let t = T(); t.run() }
            """
        ])
        XCTAssertFalse(
            symbols.contains("swift::T.run"),
            "a generic parameter constructor must not name the real T; got \(symbols)"
        )
    }

    /// A `guard let` or `for … in` binds a local that shadows a property of the
    /// enclosing type. Only `let`/`var` declarations and parameters were noted as
    /// in-scope names, so once a bare receiver could resolve through the property
    /// table these bindings resolved to the PROPERTY's type: a wrong edge, on a
    /// receiver the source says is something else entirely.
    func testABindingThatShadowsAPropertyDoesNotResolveToThePropertyType() {
        let symbols = referenceSymbols([
            "A.swift": """
            class Engine { func start() {} }
            class Other { func start() {} }
            class Holder {
                var item: Engine = Engine()
                func viaForIn(_ others: [Other]) { for item in others { item.start() } }
                func viaGuardLet(_ o: Other?) {
                    guard let item = o else { return }
                    item.start()
                }
            }
            """
        ])
        XCTAssertFalse(
            symbols.contains("swift::Engine.start"),
            "a shadowing binding must not take the property's type; got \(symbols)"
        )
    }
}
