import AppKit
import Common

/// tree-persistence (local-patch): snapshot the full window tree to disk and arm a restore for
/// the next startup. Intended to be run right before an intentional `aero restart`.
struct PersistTreeCommand: Command {
    let args: PersistTreeCmdArgs
    // Reading/serializing the tree does not mutate it, so the closed-windows cache stays valid.
    /*conforms*/ let shouldResetClosedWindowsCache = false

    func run(_ env: CmdEnv, _ io: CmdIo) -> BinaryExitCode {
        do {
            try persistTreeToDisk()
            io.out("Persisted window tree snapshot; restore armed for next startup")
            return .succ
        } catch {
            io.err("Failed to persist window tree snapshot: \(error)")
            return .fail
        }
    }
}
