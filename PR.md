# feat: add `window-parent-container-id` and `window-tree-index` format variables

## Summary

Adds two new `--format` interpolation variables to `list-windows`:

- `%{window-parent-container-id}` — the identity of the window's parent container
- `%{window-tree-index}` — the window's DFS index in the workspace tiling tree

Both are opt-in format fields. Default output, `--json` output, and existing format variables are unchanged.

## Motivation

`list-windows` currently provides `%{window-layout}` to determine whether a window is in tiles or accordion mode, but there's no way to determine **which** windows share the same container. If a workspace has two separate accordion stacks, all accordion windows report `v_accordion` with no way to distinguish which stack they belong to.

This makes it impossible for external tools (status bars, workspace indicators) to visually group windows by container — for example, showing which apps are stacked together in an accordion.

Similarly, `list-windows` sorts output alphabetically by app name, which discards the tree's spatial order. External tools that want to display windows in their visual left-to-right arrangement have no way to recover this from the CLI output. The `focus --dfs-index` command already uses DFS ordering internally, but it isn't exposed as a format variable.

**Use case:** We use these fields in a status bar helper to visually group accordion-stacked windows. The helper queries `list-windows --all` with both fields, groups windows by `container-id`, sorts by `tree-index`, and renders each accordion group as a distinct pill in sketchybar. Without these fields, there was no way to distinguish which windows share a container or recover their visual order.

## Changes

**`window-parent-container-id`** — Returns `ObjectIdentifier` of the parent `NonLeafTreeNodeObject` as a `UInt` string. Windows sharing the same container produce the same ID. IDs are stable within a session but change across restarts (they are memory addresses). This is intentional — the field is designed for grouping windows within a single `list-windows` call, not for persistent storage.

**`window-tree-index`** — Returns the window's 0-based index in `workspace.rootTilingContainer.allLeafWindowsRecursive`, which is a DFS traversal matching the visual layout order (left-to-right, top-to-bottom). Returns `-1` for windows not in the tiling tree (floating, minimized, hidden app windows). The `-1` fallback is used instead of a format error because `list-windows --all` includes windows across all states, and a format error on any single window would abort the entire output — making `--all` unusable with this field.

## Example

```
$ aerospace list-windows --workspace 1 \
    --format '%{window-tree-index}|%{app-name}|%{window-layout}|%{window-parent-container-id}'
0|Code|v_accordion|6124895232
1|cmux|v_accordion|6124895232        <- same container as Code
2|Chrome|v_accordion|6124901488
3|Vivaldi|v_accordion|6124901488     <- same container as Chrome
4|Discord|h_tiles|6124893120
-1|System Settings|floating|6124902400
```
