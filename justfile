# Full Xcode is required for the Metal shader compiler (CommandLineTools lacks `metal`).
export DEVELOPER_DIR := "/Applications/Xcode.app/Contents/Developer"

# Linux: Zed Dev is installed under this prefix (binary, Ghostty's resources)
# and linked into ~/.local/bin as `zed-dev`. libghostty-vt and the resources
# come from `<project>/ghostty-vt` (see crates/ghostty_vt_sys/README.md).
linux_prefix := env("HOME", "") / ".local/opt/zed-dev"
ghostty_vt_commit := "33da6848d63b3bba2b4f31ab1531d618f2795192"

# Build the dev-channel debug bundle and install it as /Applications/Zed Dev.app, then launch it.
[macos]
deploy:
    #!/usr/bin/env bash
    set -euo pipefail
    # -d debug build, -i install into /Applications; WinMan restores the terminals.
    # bundle-mac's debug path exits 1 on a trailing remote_server gzip step (it reads
    # from release/ even for debug builds); the app is already installed by
    # then, so swallow that and instead verify the bundle was actually refreshed.
    script/bundle-mac -d -i || true
    find '/Applications/Zed Dev.app/Contents/MacOS/zed' -mmin -10 | grep -q . \
        || { echo 'deploy failed: /Applications/Zed Dev.app was not updated'; exit 1; }
    printf '%s\n' restart-zed | nc -U /tmp/winman.sock
    echo 'Deployed /Applications/Zed Dev.app'

# Same as deploy but without launching afterwards.
[macos]
bundle:
    #!/usr/bin/env bash
    set -euo pipefail
    script/bundle-mac -d -i || true
    find '/Applications/Zed Dev.app/Contents/MacOS/zed' -mmin -10 | grep -q . \
        || { echo 'deploy failed: /Applications/Zed Dev.app was not updated'; exit 1; }
    echo 'Bundled /Applications/Zed Dev.app'

# Rebuild Zed Dev with the incremental release-iter profile and put the binary
# into the installed /Applications/Zed Dev.app, without re-bundling.
[macos]
iter-build:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --profile release-iter --package zed
    app='/Applications/Zed Dev.app'
    entitlements=$(mktemp)
    sed '/com.apple.developer.associated-domains/,+1d' crates/zed/resources/zed.entitlements > "$entitlements"
    cp target/release-iter/zed "$app/Contents/MacOS/zed.new"
    mv "$app/Contents/MacOS/zed.new" "$app/Contents/MacOS/zed"
    bash script/sign-dev-app "$app" "$entitlements"
    rm -f "$entitlements"
    echo "Updated $app"

# iter-build, then restart Zed Dev. Its terminals restart with it (tabs and
# Claude sessions come back from tab-sessions).
[macos]
iter: iter-build
    #!/usr/bin/env bash
    set -euo pipefail
    printf '%s\n' restart-zed | nc -U /tmp/winman.sock

# Build libghostty-vt from the pinned upstream commit into <project>/ghostty-vt
# (needs zig at the version upstream's build.zig.zon asks for). Ghostty's
# resources come from an installed Ghostty (the snap).
[linux]
ghostty-vt:
    #!/usr/bin/env bash
    set -euo pipefail
    kit=$(cd "{{justfile_directory()}}/../.." && pwd)/ghostty-vt
    [ -d "{{justfile_directory()}}/../../worktrees" ] || kit=$(cd "{{justfile_directory()}}/.." && pwd)/ghostty-vt
    src=$(mktemp -d)
    git -C "$src" init -q
    git -C "$src" fetch -q --depth 1 https://github.com/ghostty-org/ghostty.git {{ghostty_vt_commit}}
    git -C "$src" checkout -q FETCH_HEAD
    (cd "$src" && zig build -Demit-lib-vt -Doptimize=ReleaseFast)
    rm -rf "$kit/include" "$kit/lib"
    mkdir -p "$kit/share"
    cp -R "$src/zig-out/include" "$src/zig-out/lib" "$kit/"
    echo {{ghostty_vt_commit}} > "$kit/COMMIT"
    for dir in ghostty terminfo; do
        [ -d "$kit/share/$dir" ] || cp -R "/snap/ghostty/current/share/$dir" "$kit/share/"
    done
    rm -rf "$src"
    echo "libghostty-vt in $kit"

# Release build of Zed Dev, installed under linux_prefix with Ghostty's
# resources beside it, as `zed-dev` on PATH and a desktop entry for its
# app id (dev.zed.Zed-Dev, what winman-linux places the window by).
[linux]
bundle:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release --package zed
    kit=$(cd "{{justfile_directory()}}/../.." && pwd)/ghostty-vt
    [ -d "$kit" ] || kit=$(cd "{{justfile_directory()}}/.." && pwd)/ghostty-vt
    mkdir -p "{{linux_prefix}}/bin" "{{linux_prefix}}/share" "$HOME/.local/bin" "$HOME/.local/share/applications"
    install -m755 target/release/zed "{{linux_prefix}}/bin/zed-dev.new"
    mv "{{linux_prefix}}/bin/zed-dev.new" "{{linux_prefix}}/bin/zed-dev"
    rm -rf "{{linux_prefix}}/share/ghostty" "{{linux_prefix}}/share/terminfo"
    cp -R "$kit/share/ghostty" "$kit/share/terminfo" "{{linux_prefix}}/share/"
    ln -sfn "{{linux_prefix}}/bin/zed-dev" "$HOME/.local/bin/zed-dev"
    cat > "$HOME/.local/share/applications/dev.zed.Zed-Dev.desktop" <<DESKTOP
    [Desktop Entry]
    Type=Application
    Name=Zed Dev
    Exec={{linux_prefix}}/bin/zed-dev %U
    Icon=zed
    StartupWMClass=dev.zed.Zed-Dev
    Categories=Development;TextEditor;
    MimeType=text/plain;x-scheme-handler/zed;
    DESKTOP
    echo "Installed {{linux_prefix}}/bin/zed-dev"

# bundle, then restart Zed Dev (winman-linux brings its window back).
[linux]
deploy: bundle
    #!/usr/bin/env bash
    set -euo pipefail
    pkill -x zed-dev || true
    sleep 1
    if [ -n "${SWAYSOCK:-}" ] || ls /run/user/$(id -u)/sway-ipc.*.sock >/dev/null 2>&1; then
        export SWAYSOCK=${SWAYSOCK:-$(ls /run/user/$(id -u)/sway-ipc.*.sock | head -1)}
        swaymsg exec "{{linux_prefix}}/bin/zed-dev" >/dev/null
    else
        setsid "{{linux_prefix}}/bin/zed-dev" >/dev/null 2>&1 &
    fi
    echo 'Deployed Zed Dev'
