# Switching to the unified app

The unified Zed Dev (branch `unified-terminal`) and winman's unified mode
(`[unified_app]` in `~/.config/winman/config.toml`) only work together: the
unified build puts a terminal column into every workspace, and winman in
unified mode no longer places a Ghostty Dev window beside Zed Dev. Switch both
at once.

## Before

- Quit Ghostty Dev (`Cmd+Q`). Its tabs are saved in
  `~/.config/ghostty/tab-sessions/`, which the unified app restores, each
  Claude tab with `claude --resume`. Do not run both: they would resume the
  same sessions twice, and whichever starts first owns
  `/tmp/ghostty-winman-control.sock`.
- `GhosttyKit.xcframework` in the Ghostty checkout is what gets linked
  (`zig build -Demit-xcframework -Doptimize=ReleaseFast` rebuilds it; set
  `GHOSTTY_KIT_DIR` to use another one).

## Switch

1. Install the unified Zed Dev, from `zed/worktrees/unified-terminal`:

   ```
   DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer ./script/bundle-mac -i
   ```

   The bundle carries Ghostty's resources (terminfo, shell integration,
   themes) from `ghostty/worktrees/main/zig-out/share`.

2. Give `/Applications/Zed Dev.app` Full Disk Access (System Settings >
   Privacy & Security). The terminals are Zed Dev's children now, and the
   1Password CLI needs it, as it did for Ghostty Dev.

3. Turn on winman's unified mode:

   ```toml
   # ~/.config/winman/config.toml
   [unified_app]
   enabled = true
   ```

   and deploy it from `winman-mac/worktrees/main`:

   ```
   ./scripts/build-app.sh && launchctl kickstart -k "gui/$(id -u)/com.olof.winman.daemon"
   ./scripts/deploy-gui.sh
   ```

4. `~/.config/winman/default.kbd:2610` focuses Ghostty before toggling
   fullscreen from Zed. In unified mode fullscreen belongs to the half with
   the keyboard, so make that line `(toggle-fullscreen)` like the one below
   it, then validate with `cargo run --release --example check_cfg` before
   restarting the daemon.

5. Start Zed Dev. It restores last session's workspaces into the one window;
   winman then opens the rest of its worktrees there.

## Back

Set `enabled = false`, reinstall Zed Dev from `zed/worktrees/main` the same
way, restart the daemon and start Ghostty Dev again.

## Not there yet

- Ghostty's command palette and terminal inspector. Its config commands are
  there: `cmd+,` in the terminal (Ghostty's `open_config`) opens the config in
  the editor beside it, and `ghostty_terminal: open config` / `reload config`
  are in Zed's command palette.
- Zed's keybindings do not apply while the terminal has the keyboard, the same
  as with the Ghostty app; winman's keys move the keyboard between the halves.
