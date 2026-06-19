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
