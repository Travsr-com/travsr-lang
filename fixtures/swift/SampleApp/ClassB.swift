// Fixture for issue #449: instantiates ClassA. find_references(ClassA) must
// report this file, not "0 reference(s)".
import Foundation

class ClassB {
    func makeA() -> ClassA {
        let controller = Controller()
        let a = ClassA(controller: controller)
        a.start()
        return a
    }
}
