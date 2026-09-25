# Unified Zed + Ghostty: plan

Goal: Zed and Ghostty become one process with one window. All winman
workspaces live in that window as views. Look and behavior stay exactly as
with today's two apps (Ghostty Dev + Zed Dev arranged by winman).

## Starting points

- Terminal engine: full libghostty (GhosttyKit from the Ghostty fork). Ghostty
  renders with its own Metal renderer into an IOSurface that is composited into
  GPUI's scene. See `crates/ghostty_terminal`. Rejected alternative:
  libghostty-vt with GPUI rendering (tiborvass/zed `libghostty`,
  xipeng-jin/zed `migration/libghostty2`), because it does not look like
  Ghostty and does not honour the Ghostty config.
- The ported code lives on top of this fork, not on top of tiborvass's
  branch: that branch is 6 commits on an upstream base older than ours.

## Reference behavior

- Ghostty fork: `ghostty/worktrees/main/macos/Sources/Features/Terminal/`
  (`ZedTabBar.swift`, `ClaudeTabStatus.swift`, `WorktreePicker.swift`,
  `Winman*.swift`, `PixelArt.swift`, `SpinningGear.swift`,
  `ClaudeTabSessions.swift`) plus `BaseTerminalController.swift`.
- winman: `winman-mac/worktrees/main/src/window_manager.rs`,
  `socket_server.rs`, `keyboard.rs`, `winman-gui-swift/`.

## Target layout inside the one window

The window covers winman's content area (today: `frame_x, frame_y`, width
`TERMINAL_WIDTH + cursor_width`, height `fullscreen_height`), below the winman
bar.

```
+--------------------------+------------------------------------------+
| Ghostty tab bar (40 pt)  | Zed tab bar (40 pt)                      |
| terminal split tree      | Zed workspace (panes, docks)             |
|                          |                                          |
| bottom strip (10 pt)     | bottom strip (10 pt)                     |
+--------------------------+------------------------------------------+
  800 pt (650 at 50 %)       the rest
```

- One `MultiWorkspace` per window. Each winman worktree path is one Zed
  `Workspace` with its own terminal column (a list of Ghostty tabs, each tab a
  split tree of surfaces).
- Fullscreen per worktree: q+f makes the side you are on (terminal or editor)
  take the whole width and keeps focus there; the other side is hidden.
  Switching side while fullscreen shows that side full width. This replaces
  `default.kbd:2610`, which today jumps from Zed to Ghostty before toggling.
- winman drives the window through a new view protocol (show worktree,
  fullscreen on/off, focus side) instead of window frames and stacking.
- Switching workspace = activating another `Workspace` in the `MultiWorkspace`.
  Terminals of inactive workspaces keep running; their surfaces are marked
  occluded (`ghostty_surface_set_occlusion`).

## Milestones

1. [x] libghostty embedded, rendering composited in GPUI, keyboard (dead keys,
   IME, option-as-alt), mouse, clipboard. `ghostty_terminal::NewGhosttyTerminal`.
2. [x] Terminal column per workspace: Ghostty tab bar (flat + amiga theme,
   palette, Claude lamps, spinning gear, blocked lamp), tabs, splits,
   new/close/goto tab, bottom strip, 1 px edge lines.
   Left over: middle truncation of worktree names.
3. [x] Claude tab status poll, tab session persistence (`tab-sessions/*.json`,
   the Ghostty app's own files; a `--user-data-dir` instance keeps its own),
   worktree line + worktree picker, blocked-note sheet.
4. [x] Unified window: `MultiWorkspace` holding every worktree (every open
   joins the one window, `workspace::UnifiedWindow`), layout above,
   fullscreen per worktree, focus terminal/editor. winman drives it with
   `zed://winman/focus?path=..&terminal=1|&editor=1` and
   `zed://winman/fullscreen?path=..`.
5. [x] winman interfaces served in-process (`ghostty_terminal/src/winman.rs`):
   the Ghostty control socket with every verb (a window title is a worktree's
   terminal column), the mailbox (inject/read, kqueue), `focused-tab`,
   `blocked-tabs`, tab strips + `tab-strips-changed`, `show-editor`.
   The window shows the *current terminal* (`focus-window`) beside the active
   editor workspace (`zed://winman/raise`), so the editor can follow the work
   into another worktree while the terminal stays (`columns.rs`).
   A `--user-data-dir` instance keeps all sockets and files in its data dir.
6. [x] winman-mac (`src/unified.rs`, behind `[unified_app] enabled = true` in
   `~/.config/winman/config.toml`): focus/switch/fullscreen/show-editor go to
   the app; the app reports `focus-side terminal|editor` to the daemon, which
   the virtual keys and `active_app` follow; tab hints via `tab-hints` /
   `select-tab`; closing a worktree never closes the window. The GUI reads
   the same key.
7. [x] Bundling: `script/bundle-mac` copies Ghostty's resources into the app;
   GhosttyKit comes from the fork. How to switch over (and back):
   `switch-over.md`.
