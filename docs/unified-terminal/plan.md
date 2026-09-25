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
- Fullscreen per worktree: the focused side takes the whole width, the other
  side is hidden (today the terminal and editor windows both go full width and
  stacking decides which one shows; this is the same result without stacking).
- Switching workspace = activating another `Workspace` in the `MultiWorkspace`.
  Terminals of inactive workspaces keep running; their surfaces are marked
  occluded (`ghostty_surface_set_occlusion`).

## Milestones

1. [x] libghostty embedded, rendering composited in GPUI, keyboard (dead keys,
   IME, option-as-alt), mouse, clipboard. `ghostty_terminal::NewGhosttyTerminal`.
2. [ ] Terminal column per workspace: Ghostty tab bar (flat + amiga theme,
   palette, Claude lamps, spinning gear, blocked lamp and note), tabs,
   splits, new/close/goto tab, bottom strip, 1 px edge lines.
3. [ ] Claude tab status poll, tab session persistence (`tab-sessions/*.json`),
   worktree line + worktree picker.
4. [ ] Unified window: `MultiWorkspace` holding every worktree, layout above,
   fullscreen per worktree, focus terminal/editor.
5. [ ] winman interfaces served in-process: control socket verbs, mailbox
   (inject/read), `focused-tab`/`blocked-tabs`/`tab-strips-changed`,
   `show-editor`, `zed://winman/*`.
6. [ ] winman-mac: replace window frame/stacking logic with a view protocol
   against the one window (show worktree, fullscreen, focus side); virtual keys
   follow the focused side instead of the front process; tab hints from
   in-process geometry instead of AX.
7. [ ] Bundling: one app, GhosttyKit build from the fork, resources, signing.
