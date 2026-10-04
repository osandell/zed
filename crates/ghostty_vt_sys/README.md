# ghostty_vt_sys

Raw bindings to libghostty-vt, the terminal core of upstream Ghostty, used by
the Linux backend of `ghostty_terminal` (macOS embeds the whole of libghostty
through `ghostty_embed`). The library is not built by cargo: it is prebuilt
and kept outside the repo next to the worktrees:

```
<project>/ghostty-vt/
  include/ghostty/   vt.h and vt/*.h          (override: GHOSTTY_VT_DIR)
  lib/               libghostty-vt.a
  share/             terminfo, shell integration, themes (Ghostty's resources)
  COMMIT             the upstream commit it was built from
```

Current build: upstream Ghostty `33da6848d63b3bba2b4f31ab1531d618f2795192`,
no patches. The C API is marked unstable upstream, so the commit is pinned
and moved on purpose.

## Rebuilding

Needs Zig at the version upstream's `build.zig.zon` asks for (0.16.0 for the
commit above). `just ghostty-vt` does all of this:

```bash
git clone https://github.com/ghostty-org/ghostty.git /tmp/ghostty
cd /tmp/ghostty && git checkout <commit>
zig build -Demit-lib-vt -Doptimize=ReleaseFast
cp -R zig-out/include zig-out/lib <project>/ghostty-vt/
```

`share/` comes from an installed Ghostty (`/snap/ghostty/current/share`,
`ghostty/` and `terminfo/` only), since the lib-vt build does not emit it.

## Packaging (Nix)

What a package of Zed Dev for Linux needs (the homelab flake builds one):

- **libghostty-vt**: upstream Ghostty at the commit above, built with Zig
  0.16.0 as `zig build -Demit-lib-vt -Doptimize=ReleaseFast`. The build
  fetches Ghostty's Zig dependencies from `build.zig.zon`, so a sandboxed
  build needs them prefetched (zon2nix or nixpkgs' `ghostty` deps for that
  commit). It installs `include/ghostty/vt.h`, `include/ghostty/vt/*.h` and
  `lib/libghostty-vt.a`.
- **The Zed build**: `cargo build --release --package zed` with
  `GHOSTTY_VT_DIR` pointing at that install (`include/` and `lib/` under
  it). The crate's build script runs bindgen on `vt.h`, so libclang is
  needed (`LIBCLANG_PATH`).
- **Resources at run time**: Ghostty's `share/ghostty` (shell integration,
  themes) and `share/terminfo`. The lib-vt build does not emit them; the
  `ghostty` package of the same Ghostty version has them. The terminal finds
  them through `GHOSTTY_RESOURCES_DIR`, or next to the binary at
  `<bin>/../share/ghostty` (with `terminfo` beside it); `just bundle`
  copies them to `~/.local/opt/zed-dev/share/`.
- **Identity**: the window's app id is `dev.zed.Zed-Dev` (winman places it
  by that), and the binary is installed as `zed-dev`.
