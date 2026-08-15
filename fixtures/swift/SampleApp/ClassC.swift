// Fixture for issue #449: singleton with dotted static access (`ClassC.shared`)
// and an @objc static func called from Objective-C (`[ClassC registerEnvironments]`
// in the objc fixture's Bridge.m).
import Foundation

@objc class ClassC: NSObject {
    @objc static let shared = ClassC()

    var environments: [String] = []

    @objc static func registerEnvironments() {
        ClassC.shared.environments.append("default")
    }
}
