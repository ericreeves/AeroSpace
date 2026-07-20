import AppKit
import Common
import Foundation

// MARK: - tree-persistence (local-patch/tree-persistence)
//
// Persists the full intra-workspace window tree to disk so that it can be restored across an
// intentional `aero restart` (the AeroSpace process is killed and relaunched, which normally
// rebuilds the tree from scratch and scrambles a hand-arranged desktop).
//
// This deliberately REUSES the lock-screen recovery machinery in this directory
// (`FrozenWorld` / `restoreClosedWindowsCacheIfNeeded`). At startup we decode a persisted
// `FrozenWorld` into the existing `closedWindowsCache`; the existing per-window hook in
// `MacWindow.getOrRegister` then re-homes each window as it (re)registers, which dodges the
// async AX-registration race and re-tiles orphaned windows via the existing `potentialOrphans`
// relayout.

/// Wrapper persisted to disk. Wraps a `FrozenWorld` plus the focus so focus can be restored too.
/// `schemaVersion` guards against decoding an incompatible snapshot after a format change.
struct PersistedTree: Codable {
    static let currentSchemaVersion = 1

    let schemaVersion: Int
    let world: FrozenWorld
    let focusedWindowId: UInt32?
    let focusedWorkspace: String
}

/// Focus to re-apply once startup restore has run. Populated by `loadPersistedTreeIfPending()`,
/// consumed in `initAppBundle`. Non-nil implies a restore snapshot was loaded this boot.
@MainActor var pendingTreeRestore: (focusedWindowId: UInt32?, focusedWorkspace: String)? = nil

@MainActor private var stateDir: URL {
    FileManager.default.homeDirectoryForCurrentUser
        .appending(path: ".local/state/aerospace", directoryHint: .isDirectory)
}
@MainActor private var snapshotUrl: URL { stateDir.appending(path: "tree-snapshot.json", directoryHint: .notDirectory) }
@MainActor private var sentinelUrl: URL { stateDir.appending(path: "tree-restore.pending", directoryHint: .notDirectory) }

/// Only restore if the snapshot was written this recently. Prevents restoring a stale desktop
/// long after the fact if a sentinel somehow lingers.
private let maxSnapshotAgeSeconds: TimeInterval = 5 * 60

/// Snapshot the current world to disk, UNCONDITIONALLY (always overwrite), and drop the
/// `tree-restore.pending` sentinel so the *next* startup performs a restore. Called by the
/// `persist-tree` CLI command right before an `aero restart`.
@MainActor func persistTreeToDisk() throws {
    let allWs = Workspace.all
    let allWindowIds = allWs.flatMap { collectAllWindowIdsRecursive($0) }.toSet()
    let world = FrozenWorld(
        workspaces: allWs.map { FrozenWorkspace($0) },
        monitors: monitors.map(FrozenMonitor.init),
        windowIds: allWindowIds,
    )
    let payload = PersistedTree(
        schemaVersion: PersistedTree.currentSchemaVersion,
        world: world,
        focusedWindowId: focus.windowOrNil?.windowId,
        focusedWorkspace: focus.workspace.name,
    )

    let encoder = JSONEncoder()
    encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
    let data = try encoder.encode(payload)

    try FileManager.default.createDirectory(at: stateDir, withIntermediateDirectories: true)
    try data.write(to: snapshotUrl, options: .atomic)
    // Sentinel is opt-in per restart: written here, consumed+deleted at startup.
    FileManager.default.createFile(atPath: sentinelUrl.path, contents: Data())
}

/// At startup: if the sentinel exists AND the snapshot is fresh, decode it into
/// `closedWindowsCache` and stash the focus for later restore. The sentinel is deleted
/// unconditionally (self-clearing / opt-in per restart) so a normal boot never restores.
/// Any read/decode error is logged and treated as a no-op — never crashes startup.
@MainActor func loadPersistedTreeIfPending() {
    let fm = FileManager.default
    guard fm.fileExists(atPath: sentinelUrl.path) else { return } // normal boot: nothing to do

    // Self-clear the sentinel immediately so a decode failure (or crash loop) can't repeat a restore.
    try? fm.removeItem(at: sentinelUrl)

    do {
        let attrs = try fm.attributesOfItem(atPath: snapshotUrl.path)
        if let mtime = attrs[.modificationDate] as? Date, Date().timeIntervalSince(mtime) > maxSnapshotAgeSeconds {
            eprint("tree-persistence: snapshot is stale (\(Int(Date().timeIntervalSince(mtime)))s old); skipping restore")
            return
        }
        let data = try Data(contentsOf: snapshotUrl)
        let payload = try JSONDecoder().decode(PersistedTree.self, from: data)
        guard payload.schemaVersion == PersistedTree.currentSchemaVersion else {
            eprint("tree-persistence: snapshot schemaVersion \(payload.schemaVersion) != \(PersistedTree.currentSchemaVersion); skipping restore")
            return
        }
        setClosedWindowsCache(payload.world)
        pendingTreeRestore = (payload.focusedWindowId, payload.focusedWorkspace)
        eprint("tree-persistence: loaded snapshot with \(payload.world.windowIds.count) window(s); restore armed")
    } catch {
        // Never let a bad snapshot crash startup — restore is best-effort.
        eprint("tree-persistence: failed to load snapshot: \(error); skipping restore")
    }
}

/// Re-apply the persisted focus after startup refresh has run and windows have been re-homed.
/// Best-effort: if the previously focused window no longer exists, fall back to focusing the
/// previously focused workspace.
@MainActor func applyPendingTreeRestoreFocus() {
    guard let pending = pendingTreeRestore else { return }
    pendingTreeRestore = nil
    let workspace = Workspace.get(byName: pending.focusedWorkspace)
    if let id = pending.focusedWindowId, let window = Window.get(byId: id) {
        _ = setFocus(to: window.toLiveFocusOrNil() ?? workspace.toLiveFocus())
    } else {
        _ = setFocus(to: workspace.toLiveFocus())
    }
}
