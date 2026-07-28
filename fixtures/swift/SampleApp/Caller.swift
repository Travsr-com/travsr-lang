// Fixture for issue #449: bare member access `ClassC.shared` (no call parens)
// must emit a reference — previously only FunctionCallExprSyntax was visited.
import Foundation

func configure() {
    let c = ClassC.shared
    c.environments.append("staging")
    ClassC.registerEnvironments()
}
