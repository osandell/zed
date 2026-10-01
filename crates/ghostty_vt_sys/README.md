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
