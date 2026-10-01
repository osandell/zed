# ghostty_embed

Raw bindings to libghostty, the terminal core the `ghostty_terminal` crate
embeds. The library is not built by cargo: it is a prebuilt GhosttyKit
xcframework, kept outside the repo next to the worktrees:

```
<project>/ghostty-kit/
  GhosttyKit.xcframework/   linked by build.rs (override: GHOSTTY_KIT_DIR)
  share/                    terminfo, shell integration, themes; copied into
                            the app bundle by script/bundle-mac (override:
                            GHOSTTY_SHARE_DIR)
```

The current kit (macOS app sources are not needed, only `src/`) was built from upstream Ghostty
`4749c4e93731067049bfbf2e4572061cef2bdd17` with the patches in `patches/`
applied.

## Rebuilding the kit

Needs Zig at the version upstream's `build.zig.zon` asks for (`brew install zig@0.15` for
the commit above; it is keg-only, so call `/opt/homebrew/opt/zig@0.15/bin/zig`).

```bash
git clone https://github.com/ghostty-org/ghostty.git /tmp/ghostty
cd /tmp/ghostty
git checkout <upstream commit>
git am <this repo>/crates/ghostty_embed/patches/*.patch
zig build -Doptimize=ReleaseFast -Demit-xcframework -Demit-macos-app=false
rm -rf <project>/ghostty-kit/GhosttyKit.xcframework <project>/ghostty-kit/share
cp -R macos/GhosttyKit.xcframework zig-out/share <project>/ghostty-kit/
```

Update the commit above when you do.

## Patches

- `0001-fix-termio-deliver-SIGHUP-to-child-on-Darwin-killpg-.patch`: closing a
  terminal hangs up its child even when `killpg` returns EPERM on Darwin.
- `0002-renderer-adapt-dark-colors-reflect-program-colors-th.patch`: the
  `adapt-dark-colors` option. On a light background it darkens the text colors
  a program sets that would not read there (reflecting OKLab lightness above
  0.65) and turns dark backgrounds pale, keeping hue and chroma. Mid-tone
  accents and pale backgrounds are left as they are.
- `0003-config-parse-xNN-escapes-as-bytes-not-codepoints.patch`: `\xNN` in a
  config string is a byte. The embedded `initial_input` is escaped with
  `std.zig.stringEscape` and parsed back, and encoding each `\xNN` as a
  codepoint turned every non-ASCII character typed into a new tab into
  Latin-1 mojibake.
