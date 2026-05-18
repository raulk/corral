set shell := ["bash", "-cu"]
release-target := "aarch64-apple-darwin"
release-target-app := "target/app/Corral.app"
release-target-dmg := "target/Corral.dmg"
release-target-dmg-root := "target/dmg-root"

default: run

# Quick type-check across the workspace.
check:
    cargo check --workspace --all-targets

fmt:
    cargo fmt --all

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Debug build of the app binary.
build:
    cargo build -p corral-app

# Release build of the app binary.
build-release:
    rustup target add {{release-target}}
    cargo build -p corral-app --release --target {{release-target}}

# Dev iteration: run the binary directly without bundling.
run:
    cargo run -p corral-app

# Assemble a runnable .app bundle in target/app/Corral.app.
app: build-release icon
    #!/usr/bin/env bash
    set -euo pipefail
    APP="{{release-target-app}}"
    rm -rf "$APP"
    mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
    cp crates/corral-app/resources/Info.plist "$APP/Contents/Info.plist"
    cp crates/corral-app/resources/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
    cp target/{{release-target}}/release/corral "$APP/Contents/MacOS/corral"
    chmod +x "$APP/Contents/MacOS/corral"
    echo "Built $APP"

# Build the bundle and launch it.
open: app
    open "{{release-target-app}}"

# Generate the .icns from the source SVG. Idempotent.
icon:
    @if [ ! -f crates/corral-app/resources/AppIcon.icns ] \
        || [ crates/corral-app/resources/icon.svg -nt crates/corral-app/resources/AppIcon.icns ]; then \
        scripts/make-icon.sh; \
    fi

# Sign the assembled .app with the developer's Developer ID Application cert.
# Pass the identity via env var; everything else (timestamp, hardened runtime,
# entitlements) is inlined.
sign: app
    #!/usr/bin/env bash
    set -euo pipefail
    : "${DEVELOPER_ID:?must export DEVELOPER_ID, e.g. 'Developer ID Application: Jane Doe (TEAMID)'}"
    APP="{{release-target-app}}"
    [ -d "$APP" ] || { echo "missing $APP; run 'just app' first" >&2; exit 1; }
    codesign --force --options runtime --timestamp \
        --sign "$DEVELOPER_ID" \
        --entitlements crates/corral-app/resources/entitlements.plist \
        "$APP"
    codesign --verify --deep --strict --verbose=2 "$APP"

# Package the signed bundle into a compressed UDZO DMG through dmgbuild.
dmg: sign
    #!/usr/bin/env bash
    set -euo pipefail
    APP="{{release-target-app}}"
    DMG="{{release-target-dmg}}"
    DMG_ROOT="{{release-target-dmg-root}}"
    DMG_SETTINGS="scripts/dmg-settings.py"
    trap 'rm -rf "$DMG_ROOT"' EXIT
    rm -rf "$DMG_ROOT"
    mkdir -p "$DMG_ROOT"
    ditto "$APP" "$DMG_ROOT/Corral.app"
    rm -f "$DMG"
    python3 -m dmgbuild -s "$DMG_SETTINGS" -D source_root="$DMG_ROOT" Corral "$DMG"
    echo "Wrote $DMG"

# Package an ad-hoc signed bundle into a compressed UDZO DMG.
# This is for pre-Developer-ID GitHub releases only. It is not a
# Gatekeeper-clean public release artifact, but the app bundle still needs a
# valid local signature so macOS does not treat it as damaged.
unsigned-dmg: app
    #!/usr/bin/env bash
    set -euo pipefail
    APP="{{release-target-app}}"
    DMG="{{release-target-dmg}}"
    DMG_ROOT="{{release-target-dmg-root}}"
    DMG_SETTINGS="scripts/dmg-settings.py"
    trap 'rm -rf "$DMG_ROOT"' EXIT
    codesign --force --deep --sign - "$APP"
    codesign --verify --deep --strict --verbose=2 "$APP"
    rm -rf "$DMG_ROOT"
    mkdir -p "$DMG_ROOT"
    ditto "$APP" "$DMG_ROOT/Corral.app"
    rm -f "$DMG"
    python3 -m dmgbuild -s "$DMG_SETTINGS" -D source_root="$DMG_ROOT" Corral "$DMG"
    echo "Wrote unsigned $DMG"

# Submit the DMG to Apple's notarisation service and staple the result.
# Requires an app-specific password generated at appleid.apple.com.
notarize: dmg
    #!/usr/bin/env bash
    set -euo pipefail
    : "${APPLE_ID:?must export APPLE_ID, your developer account email}"
    : "${TEAM_ID:?must export TEAM_ID, your 10-character team identifier}"
    : "${APP_PASSWORD:?must export APP_PASSWORD, an app-specific password}"
    DMG="{{release-target-dmg}}"
    [ -f "$DMG" ] || { echo "missing $DMG; run 'just dmg' first" >&2; exit 1; }
    xcrun notarytool submit "$DMG" \
        --apple-id "$APPLE_ID" \
        --team-id "$TEAM_ID" \
        --password "$APP_PASSWORD" \
        --wait
    xcrun stapler staple "$DMG"
    xcrun stapler validate "$DMG"

# Full pipeline: clean build, sign, package, notarise, staple.
release: notarize
    @echo "release artefacts ready in target/"

# Prepend the unreleased Conventional Commit changes to CHANGELOG.md.
changelog version:
    #!/usr/bin/env bash
    set -euo pipefail
    version="{{version}}"
    version="${version#v}"
    GIT_CLIFF_OFFLINE="${GIT_CLIFF_OFFLINE:-true}" \
        git cliff --unreleased --tag "v${version}" --prepend CHANGELOG.md

# Print the sha256 of the released DMG for pasting into the Homebrew Cask.
release-sha:
    @shasum -a 256 {{release-target-dmg}} | awk '{print $1}'
