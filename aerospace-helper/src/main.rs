use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use std::ptr::NonNull;

use block2::RcBlock;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::NSNotification;
use serde::Deserialize;
use notify::{Watcher, RecursiveMode, Event, EventKind};
use std::path::Path;

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/Users/eric".to_string())
}
fn current_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "user".to_string())
}
// Per-user names so multiple users on one host don't collide in /tmp.
fn socket_path() -> String { format!("/tmp/aerospace-helper-{}.sock", current_user()) }
fn pid_path() -> String { format!("/tmp/aerospace-helper-{}.pid", current_user()) }
fn icon_map_path() -> String { format!("{}/.config/aerospace/sketchybar/icon_map.toml", home_dir()) }
// Workspaces toggled to floating mode by workspace_float_toggle.sh (alt-shift-f)
fn floating_ws_path() -> String { format!("{}/.local/state/aero/floating-workspaces", home_dir()) }

// Catppuccin Mocha Mauve — floating state, matches the JankyBorders floating border
const MAUVE: &str = "0xffcba6f7";
const MAUVE_DIM: &str = "0x99cba6f7";

// Debounce: minimum ms between processing the same event type
const DEBOUNCE_MS: u64 = 150;
// Max concurrent spawned processes before we start dropping events
const MAX_CHILD_PROCS: usize = 20;
// Timeout for synchronous aerospace CLI calls. The CLI normally returns in ms;
// if it doesn't, the daemon is unresponsive and blocking the worker is worse
// than aborting the call. The worker recovers by returning None to callers,
// which already handle missing data via unwrap_or_default().
const AEROSPACE_CMD_TIMEOUT: Duration = Duration::from_secs(2);

// --- TOML deserialization structs ---

#[derive(Deserialize, Default)]
struct IconMapConfig {
    #[serde(default)]
    icons: HashMap<String, String>,
}

/// A single JSON line from `aerospace subscribe`. Only the fields we route on.
#[derive(Deserialize)]
struct SubscribeEvent {
    #[serde(rename = "_event")]
    event: String,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    binding: Option<String>,
}

/// Keybindings that mutate the tiling tree but emit no focus/workspace event,
/// so the helper must redraw when `binding-triggered` reports them. Mirrors the
/// bindings that previously appended `aero-notify workspace_changed` in
/// aerospace.toml (move / move-node-to-workspace / join-with / layout / flatten).
/// If you add a layout-affecting binding there, add its key here too.
const LAYOUT_BINDINGS: &[&str] = &[
    "alt-shift-h", "alt-shift-j", "alt-shift-k", "alt-shift-l",
    "alt-shift-comma", "alt-shift-period",
    "alt-shift-u", "alt-shift-i", "alt-o", "alt-r",
];

/// Workspaces currently in floating mode (one name per line, written by
/// workspace_float_toggle.sh). Read fresh on every update — the toggle script
/// fires workspace_changed after writing, so no watcher is needed.
fn load_floating_workspaces() -> std::collections::HashSet<String> {
    fs::read_to_string(floating_ws_path())
        .map(|c| {
            c.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn load_icon_map() -> HashMap<String, String> {
    let content = match fs::read_to_string(icon_map_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[helper] failed to read icon map: {}", e);
            return HashMap::new();
        }
    };
    let config: IconMapConfig = match toml::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[helper] failed to parse icon map: {}", e);
            return HashMap::new();
        }
    };
    config.icons
}

struct HelperState {
    last_event_times: HashMap<String, Instant>,
    child_count: usize,
    icon_map: HashMap<String, String>,
    sketchybar_items: HashMap<String, Vec<String>>,  // workspace -> dynamic item names
    sketchybar_enabled: bool,
}

impl HelperState {
    fn new() -> Self {
        let icon_map = load_icon_map();
        // Detect sketchybar by binary presence only — failed CLI calls when the
        // daemon isn't up yet are harmless no-ops, so we don't need to gate on
        // daemon reachability (which would race launchctl bootstrap order).
        let sketchybar_enabled = std::path::Path::new("/opt/homebrew/bin/sketchybar").exists();
        eprintln!("[helper] loaded {} icon mappings, sketchybar={}",
            icon_map.len(), sketchybar_enabled);
        Self {
            last_event_times: HashMap::new(),
            child_count: 0,
            icon_map,
            sketchybar_items: HashMap::new(),
            sketchybar_enabled,
        }
    }

    /// Returns true if this event type should be processed (not debounced).
    fn should_process(&mut self, event_key: &str) -> bool {
        let now = Instant::now();
        if let Some(last) = self.last_event_times.get(event_key) {
            if now.duration_since(*last) < Duration::from_millis(DEBOUNCE_MS) {
                return false;
            }
        }
        self.last_event_times.insert(event_key.to_string(), now);
        true
    }
}

// Global flag for signal-driven shutdown
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

// --- Aerospace CLI ---

fn aerospace_cmd(args: &[&str]) -> Option<String> {
    let child = Command::new("/opt/homebrew/bin/aerospace")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let pid = child.id();

    // wait_with_output() consumes the child; run it on a thread so we can bound
    // the wait. On timeout we SIGKILL the pid we captured above, which lets the
    // background thread complete its read and exit naturally.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(AEROSPACE_CMD_TIMEOUT) {
        Ok(Ok(output)) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(_) => None,
        Err(_) => {
            eprintln!(
                "[helper] WARNING: aerospace_cmd timed out after {:?} (pid {}): {:?}",
                AEROSPACE_CMD_TIMEOUT, pid, args
            );
            unsafe { libc::kill(pid as i32, libc::SIGKILL); }
            None
        }
    }
}

fn get_hidden_bundle_ids() -> Vec<String> {
    let workspace = NSWorkspace::sharedWorkspace();
    let apps = workspace.runningApplications();
    let mut hidden = Vec::new();
    for app in apps.iter() {
        if app.isHidden() {
            if let Some(bid) = app.bundleIdentifier() {
                hidden.push(bid.to_string());
            }
        }
    }
    hidden
}

// --- Spawn external commands with cleanup ---

/// Spawn a fire-and-forget command, but reap the child to prevent zombies.
/// Returns false if we're at the child process limit.
fn spawn_and_reap(cmd: &str, args: &[&str], state: &Arc<Mutex<HelperState>>) -> bool {
    {
        let s = state.lock().unwrap();
        if s.child_count >= MAX_CHILD_PROCS {
            eprintln!("[helper] WARNING: child process limit ({}) reached, dropping spawn of {}", MAX_CHILD_PROCS, cmd);
            return false;
        }
    }

    let child = Command::new(cmd).args(args).spawn();
    match child {
        Ok(mut child) => {
            let state = Arc::clone(state);
            {
                let mut s = state.lock().unwrap();
                s.child_count += 1;
            }
            thread::spawn(move || {
                let _ = child.wait();
                let mut s = state.lock().unwrap();
                s.child_count = s.child_count.saturating_sub(1);
            });
            true
        }
        Err(e) => {
            eprintln!("[helper] failed to spawn {}: {}", cmd, e);
            false
        }
    }
}

fn spawn_bash_and_reap(script: &str, state: &Arc<Mutex<HelperState>>) -> bool {
    spawn_and_reap("/bin/bash", &["-c", script], state)
}

// --- Event handling ---

fn update_borders(state: &Arc<Mutex<HelperState>>) {
    // Query focused window state for dynamic border color
    let state_info = aerospace_cmd(&[
        "list-windows", "--focused",
        "--format", "%{window-layout}|%{window-is-fullscreen}",
    ]).unwrap_or_default();

    let parts: Vec<&str> = state_info.lines().next().unwrap_or("").split('|').collect();
    let layout = parts.first().unwrap_or(&"");
    let is_fullscreen = parts.get(1).unwrap_or(&"false") == &"true";

    // Catppuccin Mocha border colors by state
    let active_color = if is_fullscreen {
        "0xff89b4fa"  // Blue — fullscreen
    } else if layout.contains("accordion") {
        "0xffa6e3a1"  // Green — accordion/stacked
    } else if *layout == "floating" {
        "0xffcba6f7"  // Mauve — floating
    } else {
        "glow(0xffb4befe)"  // Lavender glow — normal tiling
    };

    let cmd = format!(
        "/opt/homebrew/bin/borders active_color=\"{}\" inactive_color=\"0xff11111b\"",
        active_color
    );
    spawn_bash_and_reap(&cmd, state);
}

fn update_sketchybar(focused_workspace: &str, state_arc: &Arc<Mutex<HelperState>>) {
    // Snapshot what we need from state, then drop the lock immediately
    let (icon_map, prev_items) = {
        let state = state_arc.lock().unwrap();
        if !state.sketchybar_enabled {
            return;
        }
        (state.icon_map.clone(), state.sketchybar_items.clone())
    };

    let hidden = get_hidden_bundle_ids();
    let floating_workspaces = load_floating_workspaces();

    // Query ALL windows with layout, container, and tree index
    let all_windows = aerospace_cmd(&[
        "list-windows", "--all",
        "--format", "%{workspace}|%{app-name}|%{app-bundle-id}|%{window-layout}|%{window-parent-container-id}|%{window-tree-index}",
    ]).unwrap_or_default();

    // Query focused window's app name to highlight its icon
    let focused_app = aerospace_cmd(&[
        "list-windows", "--focused", "--format", "%{app-name}",
    ]).unwrap_or_default();

    // Query workspace-to-monitor mapping (only if multi-monitor)
    let monitor_count: usize = aerospace_cmd(&["list-monitors", "--count"])
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1);

    let ws_monitors: HashMap<String, String> = if monitor_count > 1 {
        aerospace_cmd(&["list-workspaces", "--monitor", "all", "--format", "%{workspace}|%{monitor-id}"])
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() == 2 { Some((parts[0].to_string(), parts[1].to_string())) } else { None }
            })
            .collect()
    } else {
        HashMap::new()
    };

    // Helper closures using the snapshots
    let get_icon = |app_name: &str| -> &str {
        icon_map.get(app_name)
            .or_else(|| icon_map.get("_default"))
            .map(|s| s.as_str())
            .unwrap_or(":default:")
    };

    // Parse windows into per-workspace ordered lists
    struct WindowInfo {
        app_name: String,
        layout: String,
        container_id: String,
        tree_index: i32,
    }
    let mut ws_windows: HashMap<String, Vec<WindowInfo>> = HashMap::new();
    for line in all_windows.lines() {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() != 6 { continue; }
        let ws = parts[0].trim();
        let app_name = parts[1].trim();
        let bid = parts[2].trim();
        let layout = parts[3].trim();
        let container_id = parts[4].trim();
        let tree_index: i32 = parts[5].trim().parse().unwrap_or(999);
        if app_name.is_empty() { continue; }
        if hidden.contains(&bid.to_string()) { continue; }
        ws_windows.entry(ws.to_string()).or_default().push(WindowInfo {
            app_name: app_name.to_string(),
            layout: layout.to_string(),
            container_id: container_id.to_string(),
            tree_index,
        });
    }

    // Sort each workspace's windows by tree index (visual order)
    for windows in ws_windows.values_mut() {
        windows.sort_by_key(|w| w.tree_index);
    }

    // Build groups per workspace, preserving tree order
    struct IconGroup {
        icons: Vec<String>,
        is_accordion: bool,
        is_floating: bool,
        has_focused: bool,
    }
    let mut ws_groups: HashMap<String, Vec<IconGroup>> = HashMap::new();
    for (ws, windows) in &ws_windows {
        let mut groups: Vec<IconGroup> = Vec::new();
        let mut seen_containers: std::collections::HashSet<String> = std::collections::HashSet::new();
        for win in windows {
            let is_accordion = win.layout.contains("accordion");
            if is_accordion {
                if seen_containers.contains(&win.container_id) {
                    continue; // Already emitted this accordion group
                }
                seen_containers.insert(win.container_id.clone());
                // Collect ALL windows with this container_id
                let container_windows: Vec<&WindowInfo> = windows.iter()
                    .filter(|w| w.container_id == win.container_id)
                    .collect();
                let has_focused = container_windows.iter().any(|w| w.app_name == focused_app);
                let group_icons: Vec<String> = container_windows.iter()
                    .map(|w| get_icon(&w.app_name).to_string())
                    .collect();
                groups.push(IconGroup { icons: group_icons, is_accordion: true, is_floating: false, has_focused });
            } else {
                let has_focused = win.app_name == focused_app;
                groups.push(IconGroup {
                    icons: vec![get_icon(&win.app_name).to_string()],
                    is_accordion: false,
                    is_floating: win.layout == "floating",
                    has_focused,
                });
            }
        }
        ws_groups.insert(ws.clone(), groups);
    }

    // Build batched sketchybar command args
    let mut args: Vec<String> = Vec::new();
    let mut new_items: HashMap<String, Vec<String>> = HashMap::new();

    for sid in 1..=9 {
        let sid_str = sid.to_string();
        let is_focused = sid_str == focused_workspace;
        // Floating-mode workspace gets a mauve outline (matching JankyBorders)
        let is_floating_ws = floating_workspaces.contains(&sid_str);
        let bg_color = if is_focused { "0xff313244" } else { "0x00000000" };
        let highlight = if is_focused { "on" } else { "off" };
        let (border_color, border_width) = if is_floating_ws { (MAUVE, 2) } else { ("0x00000000", 0) };

        // Update the static space.{sid} item — clear label, set highlight
        // Background now comes from the bracket, so disable it on the item itself
        args.extend([
            "--set".to_string(), format!("space.{}", sid),
            "label=".to_string(),
            format!("icon.highlight={}", highlight),
            format!("label.highlight={}", highlight),
            "background.drawing=off".to_string(),
            "icon.padding_right=5".to_string(),
            "label.padding_left=0".to_string(),
            "label.padding_right=0".to_string(),
        ]);

        if monitor_count > 1 {
            if let Some(mid) = ws_monitors.get(&sid_str) {
                args.push(format!("associated_display={}", mid));
            }
        } else {
            args.push("associated_display=1".to_string());
        }

        // Create dynamic items for each icon group
        let groups = ws_groups.get(&sid_str);
        let mut item_names: Vec<String> = Vec::new();
        if let Some(groups) = groups {
            // Track the last placed item so we can chain --move after it
            let mut move_after = format!("space.{}", sid);
            for (idx, group) in groups.iter().enumerate() {
                let item_name = format!("ws{}.g{}", sid, idx);
                let label = group.icons.join(" ");
                // Floating windows render mauve (matching the JankyBorders floating
                // border); otherwise focused gets bright color, others dim
                let item_color = if group.is_floating {
                    if group.has_focused { MAUVE } else { MAUVE_DIM }
                } else if group.has_focused { "0xffcdd6f4" } else { "0xff6c7086" };

                // Add item and set properties (left segment, same as the space.N items)
                args.extend([
                    "--add".to_string(), "item".to_string(), item_name.clone(), "left".to_string(),
                    "--set".to_string(), item_name.clone(),
                    format!("label={}", label),
                    format!("label.color={}", item_color),
                    "label.font=sketchybar-app-font:Regular:13.0".to_string(),
                    "label.y_offset=0".to_string(),
                    "icon.drawing=off".to_string(),
                    "label.padding_left=4".to_string(),
                    "label.padding_right=4".to_string(),
                ]);

                if group.is_accordion {
                    args.extend([
                        "background.drawing=on".to_string(),
                        "background.color=0xff45475a".to_string(),
                        "background.corner_radius=8".to_string(),
                        "background.border_width=0".to_string(),
                        "background.height=26".to_string(),
                        "background.padding_left=2".to_string(),
                        "background.padding_right=2".to_string(),
                    ]);
                } else {
                    args.push("background.drawing=off".to_string());
                }

                if monitor_count > 1 {
                    if let Some(mid) = ws_monitors.get(&sid_str) {
                        args.push(format!("associated_display={}", mid));
                    }
                } else {
                    args.push("associated_display=1".to_string());
                }

                // Position after the previous item to maintain tree order
                args.extend([
                    "--move".to_string(), item_name.clone(),
                    "after".to_string(), move_after,
                ]);

                move_after = item_name.clone();
                item_names.push(item_name);
            }
        }
        // Create bracket around workspace number + all icon items
        let bracket_name = format!("ws{}.bracket", sid);
        if !item_names.is_empty() {
            let mut bracket_members: Vec<String> = vec![format!("space.{}", sid)];
            bracket_members.extend(item_names.iter().cloned());
            args.push("--add".to_string());
            args.push("bracket".to_string());
            args.push(bracket_name.clone());
            args.extend(bracket_members);
            args.extend([
                "--set".to_string(), bracket_name.clone(),
                format!("background.color={}", bg_color),
                format!("background.border_color={}", border_color),
                format!("background.border_width={}", border_width),
                "background.corner_radius=8".to_string(),
                "background.height=26".to_string(),
                "background.drawing=on".to_string(),
            ]);
        } else {
            // No icons — just show workspace number with its own background
            args.extend([
                "--set".to_string(), format!("space.{}", sid),
                "background.drawing=on".to_string(),
                format!("background.color={}", bg_color),
                format!("background.border_color={}", border_color),
                format!("background.border_width={}", border_width),
            ]);
            // Remove bracket if it existed
            args.extend(["--remove".to_string(), bracket_name.clone()]);
        }

        item_names.push(bracket_name);
        new_items.insert(sid_str, item_names);
    }

    // Remove stale items from previous update
    for (ws, old_names) in &prev_items {
        let current_names = new_items.get(ws).cloned().unwrap_or_default();
        for name in old_names {
            if !current_names.contains(name) {
                args.extend(["--remove".to_string(), name.clone()]);
            }
        }
    }

    // Store new item names in state
    {
        let mut state = state_arc.lock().unwrap();
        state.sketchybar_items = new_items;
    }

    let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    spawn_and_reap("/opt/homebrew/bin/sketchybar", &args_refs, state_arc);
}

/// Persistent `aerospace subscribe` reader. Replaces the old aero-notify Unix
/// socket: AeroSpace pushes JSON-line events directly, which we route to the
/// workspace worker. Reconnects if the stream ends (e.g. AeroSpace restart).
///
/// Routing (matching the previous aero-notify semantics):
///   focused-workspace-changed -> redraw for that workspace (never debounced)
///   focus-changed / focused-monitor-changed / window-detected -> "__focus__"
///   binding-triggered (only LAYOUT_BINDINGS) -> "__focus__" (covers move/join/
///     layout/flatten, which emit no focus/workspace event)
/// The "__focus__" path is debounced under a single key, same as before.
fn subscribe_loop(ws_tx: mpsc::Sender<String>, state_arc: Arc<Mutex<HelperState>>) {
    let events = [
        "subscribe",
        "focused-workspace-changed",
        "focus-changed",
        "focused-monitor-changed",
        "binding-triggered",
        "window-detected",
    ];
    loop {
        if SHUTDOWN.load(Ordering::Relaxed) { return; }
        let mut child = match Command::new("/opt/homebrew/bin/aerospace")
            .args(events)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[helper] failed to spawn `aerospace subscribe`: {}; retrying", e);
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => { let _ = child.wait(); thread::sleep(Duration::from_secs(2)); continue; }
        };
        eprintln!("[helper] subscribe stream connected");

        let debounce = |state_arc: &Arc<Mutex<HelperState>>| -> bool {
            let mut s = state_arc.lock().unwrap();
            s.should_process("focus")
        };

        for line in BufReader::new(stdout).lines() {
            let line = match line { Ok(l) => l, Err(_) => break };
            let line = line.trim();
            if line.is_empty() { continue; }
            let ev: SubscribeEvent = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(e) => { eprintln!("[helper] subscribe parse error: {} ({})", e, line); continue; }
            };
            match ev.event.as_str() {
                "focused-workspace-changed" => {
                    match ev.workspace {
                        Some(ws) => { let _ = ws_tx.send(ws); }
                        None => { if debounce(&state_arc) { let _ = ws_tx.send("__focus__".to_string()); } }
                    }
                }
                "focus-changed" | "focused-monitor-changed" | "window-detected" => {
                    if debounce(&state_arc) { let _ = ws_tx.send("__focus__".to_string()); }
                }
                "binding-triggered" => {
                    if let Some(b) = ev.binding.as_deref() {
                        if LAYOUT_BINDINGS.contains(&b) && debounce(&state_arc) {
                            let _ = ws_tx.send("__focus__".to_string());
                        }
                    }
                }
                _ => {}
            }
        }

        let _ = child.wait();
        eprintln!("[helper] subscribe stream ended; reconnecting");
        if SHUTDOWN.load(Ordering::Relaxed) { return; }
        thread::sleep(Duration::from_secs(1));
    }
}

/// Minimal external-nudge IPC. `aerospace subscribe` covers all AeroSpace
/// events, but non-AeroSpace triggers still need a way to request a redraw —
/// currently sketchybar's `display_change` event (monitor connect/disconnect,
/// which has no subscribe equivalent) via `aero-notify`. Any line received
/// triggers a full refresh for the currently-focused workspace.
fn socket_nudge_loop(ws_tx: mpsc::Sender<String>) {
    let _ = fs::remove_file(socket_path());
    let path = socket_path();
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => { eprintln!("[helper] failed to bind nudge socket: {}", e); return; }
    };
    eprintln!("[helper] nudge socket ready on {}", path);
    for stream in listener.incoming() {
        if SHUTDOWN.load(Ordering::Relaxed) { break; }
        match stream {
            Ok(stream) => {
                for line in BufReader::new(stream).lines() {
                    if matches!(line, Ok(ref l) if !l.trim().is_empty()) {
                        let _ = ws_tx.send("__visibility__".to_string());
                    }
                }
            }
            Err(e) => eprintln!("[helper] nudge socket error: {}", e),
        }
    }
}

fn cleanup_and_exit() {
    let _ = fs::remove_file(socket_path());
    let _ = fs::remove_file(pid_path());
    eprintln!("[helper] cleaned up, exiting");
    std::process::exit(0);
}

fn write_pid_file() -> bool {
    let my_pid = std::process::id();

    // Check for existing PID file
    if let Ok(contents) = fs::read_to_string(pid_path()) {
        if let Ok(old_pid) = contents.trim().parse::<u32>() {
            // Check if that process is still alive
            let alive = unsafe { libc::kill(old_pid as i32, 0) == 0 };
            if alive && old_pid != my_pid {
                eprintln!("[helper] another instance already running (pid {})", old_pid);
                return false;
            }
        }
    }

    if let Err(e) = fs::write(pid_path(), format!("{}", my_pid)) {
        eprintln!("[helper] failed to write PID file: {}", e);
        return false;
    }
    true
}

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-v") {
        println!("aerospace-helper {} ({})", env!("CARGO_PKG_VERSION"), env!("HELPER_GIT_HASH"));
        return;
    }

    // Single-instance guard
    if !write_pid_file() {
        eprintln!("[helper] exiting to avoid duplicate instance");
        std::process::exit(1);
    }

    let _ = fs::remove_file(socket_path());
    eprintln!("aerospace-helper {} ({}) starting (pid {})",
        env!("CARGO_PKG_VERSION"), env!("HELPER_GIT_HASH"), std::process::id());

    // Register signal handlers for cleanup
    unsafe {
        libc::signal(libc::SIGTERM, cleanup_and_exit as *const () as usize);
        libc::signal(libc::SIGINT, cleanup_and_exit as *const () as usize);
    }

    let state = Arc::new(Mutex::new(HelperState::new()));
    update_borders(&state);

    // File watcher thread for config hot-reload
    let state_watcher = Arc::clone(&state);
    thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, _>| {
            if let Ok(event) = res { let _ = tx.send(event); }
        }).expect("Failed to create file watcher");

        watcher.watch(Path::new(&icon_map_path()), RecursiveMode::NonRecursive).ok();
        eprintln!("[helper] file watcher started");

        let mut last_reload = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(event) => {
                    if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) { continue; }
                    if Instant::now().duration_since(last_reload) < Duration::from_millis(500) { continue; }
                    thread::sleep(Duration::from_millis(500));
                    while rx.try_recv().is_ok() {} // Drain
                    last_reload = Instant::now();

                    let mut s = state_watcher.lock().unwrap();
                    for path in &event.paths {
                        let p = path.to_str().unwrap_or("");
                        if p.contains("icon_map.toml") {
                            s.icon_map = load_icon_map();
                            eprintln!("[helper] reloaded {} icon mappings", s.icon_map.len());
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }
    });

    // Workspace worker thread — processes events sequentially to avoid concurrent
    // aerospace_cmd calls (AeroSpace is single-threaded and can deadlock).
    // workspace_changed:N carries the workspace; __focus__ / __visibility__ are
    // sentinels meaning "refresh for whatever is currently focused".
    let (ws_tx, ws_rx) = mpsc::channel::<String>();
    let state_ws_worker = Arc::clone(&state);
    thread::spawn(move || {
        for workspace in ws_rx {
            let focused_ws = if workspace.starts_with("__") {
                aerospace_cmd(&["list-workspaces", "--focused"]).unwrap_or_default()
            } else {
                workspace
            };
            update_borders(&state_ws_worker);
            update_sketchybar(&focused_ws, &state_ws_worker);
        }
    });

    // aerospace subscribe reader on a background thread — AeroSpace pushes
    // JSON-line events directly (replaces the aero-notify Unix socket).
    let state_sub = Arc::clone(&state);
    let ws_tx_sub = ws_tx.clone();
    thread::spawn(move || subscribe_loop(ws_tx_sub, state_sub));

    // Minimal external-nudge socket (sketchybar display_change -> aero-notify).
    let ws_tx_socket = ws_tx.clone();
    thread::spawn(move || socket_nudge_loop(ws_tx_socket));

    // Main thread: register workspace notifications and run NSRunLoop
    // NSWorkspace notifications require the main thread's run loop
    let workspace = NSWorkspace::sharedWorkspace();
    let center = workspace.notificationCenter();

    // NSWorkspace app hide/unhide/launch/terminate isn't an `aerospace subscribe`
    // event, so we keep these observers and push "__visibility__" to the worker
    // directly (no socket round-trip).
    let tx_hide = ws_tx.clone();
    let hide_block = RcBlock::new(move |_notif: NonNull<NSNotification>| {
        eprintln!("[helper] NSWorkspace: app hidden");
        let _ = tx_hide.send("__visibility__".to_string());
    });

    let tx_unhide = ws_tx.clone();
    let unhide_block = RcBlock::new(move |_notif: NonNull<NSNotification>| {
        eprintln!("[helper] NSWorkspace: app unhidden");
        let _ = tx_unhide.send("__visibility__".to_string());
    });

    let tx_terminate = ws_tx.clone();
    let terminate_block = RcBlock::new(move |_notif: NonNull<NSNotification>| {
        eprintln!("[helper] NSWorkspace: app terminated");
        let _ = tx_terminate.send("__visibility__".to_string());
    });

    let tx_launch = ws_tx.clone();
    let launch_block = RcBlock::new(move |_notif: NonNull<NSNotification>| {
        eprintln!("[helper] NSWorkspace: app launched");
        let _ = tx_launch.send("__visibility__".to_string());
    });

    let hide_name = objc2_foundation::NSNotificationName::from_str("NSWorkspaceDidHideApplicationNotification");
    let unhide_name = objc2_foundation::NSNotificationName::from_str("NSWorkspaceDidUnhideApplicationNotification");
    let terminate_name = objc2_foundation::NSNotificationName::from_str("NSWorkspaceDidTerminateApplicationNotification");
    let launch_name = objc2_foundation::NSNotificationName::from_str("NSWorkspaceDidLaunchApplicationNotification");

    unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(&hide_name), None, None, &hide_block,
        );
        center.addObserverForName_object_queue_usingBlock(
            Some(&unhide_name), None, None, &unhide_block,
        );
        center.addObserverForName_object_queue_usingBlock(
            Some(&terminate_name), None, None, &terminate_block,
        );
        center.addObserverForName_object_queue_usingBlock(
            Some(&launch_name), None, None, &launch_block,
        );
    }

    eprintln!("[helper] workspace observer registered on main thread");

    // Run the main thread's run loop forever to receive notifications
    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            cleanup_and_exit();
        }
        objc2_foundation::NSRunLoop::currentRunLoop()
            .runUntilDate(&objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(1.0));
    }
}
