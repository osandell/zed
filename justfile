# Full Xcode is required for the Metal shader compiler (CommandLineTools lacks `metal`).
export DEVELOPER_DIR := "/Applications/Xcode.app/Contents/Developer"

# Build the dev-channel debug bundle and install it as /Applications/Zed Dev.app, then launch it.
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
bundle:
    #!/usr/bin/env bash
    set -euo pipefail
    script/bundle-mac -d -i || true
    find '/Applications/Zed Dev.app/Contents/MacOS/zed' -mmin -10 | grep -q . \
        || { echo 'deploy failed: /Applications/Zed Dev.app was not updated'; exit 1; }
    echo 'Bundled /Applications/Zed Dev.app'

# Rebuild Zed Dev with the incremental release-iter profile and put the binary
# into the installed /Applications/Zed Dev.app, without re-bundling.
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
iter: iter-build
    #!/usr/bin/env bash
    set -euo pipefail
    printf '%s\n' restart-zed | nc -U /tmp/winman.sock
