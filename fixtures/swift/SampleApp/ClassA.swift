// Fixture for issue #449: class with an explicit init. `ClassA(controller:)`
// call sites must produce RefCall edges + edge_sites.
import Foundation

class Controller {
    var name: String = ""
}

class ClassA {
    let controller: Controller

    init(controller: Controller) {
        self.controller = controller
    }

    func start() {
        print("started for \(controller.name)")
    }
}
