# Full Xcode is required for the Metal shader compiler (CommandLineTools lacks `metal`).
export DEVELOPER_DIR := "/Applications/Xcode.app/Contents/Developer"

# Build the dev-channel debug bundle and install it as /Applications/Zed Dev.app, then launch it.
deploy:
    #!/usr/bin/env bash
    set -euo pipefail
    osascript -e 'tell application "Zed Dev" to quit' >/dev/null 2>&1 || true
    # -d debug build, -i install into /Applications, -o launch the installed bundle.
    # bundle-mac's debug path exits 1 on a trailing remote_server gzip step (it reads
    # from release/ even for debug builds); the app is already installed+launched by
    # then, so swallow that and instead verify the bundle was actually refreshed.
    script/bundle-mac -d -i -o || true
    find '/Applications/Zed Dev.app/Contents/MacOS/zed' -mmin -10 | grep -q . \
        || { echo 'deploy failed: /Applications/Zed Dev.app was not updated'; exit 1; }
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
    codesign --force --deep --entitlements "$entitlements" --sign - "$app"
    rm -f "$entitlements"
    echo "Updated $app"

# iter-build, then restart Zed Dev. Its terminals restart with it (tabs and
# Claude sessions come back from tab-sessions).
iter: iter-build
    #!/usr/bin/env bash
    set -euo pipefail
    osascript -e 'tell application "Zed Dev" to quit' >/dev/null 2>&1 || true
    for _ in $(seq 1 40); do pgrep -f 'Zed Dev.app/Contents/MacOS/zed' >/dev/null || break; sleep 0.25; done
    open -a '/Applications/Zed Dev.app'
