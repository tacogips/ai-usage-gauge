# Homebrew Packaging

This project ships two Homebrew release paths built from the Rust and Tauri v2 workspace:

- Formula: unsigned tarballs containing `bin/ai-usage-gauge`, `bin/ai-usage-gauge-app`, and `share/ai-usage-gauge/web`.
- Cask: signed, notarized, and stapled macOS DMGs containing `AIUsageGauge.app` (`Contents/MacOS/ai-usage-gauge-app`, `Contents/MacOS/ai-usage-gauge`, and `Contents/Resources/Web`, with no nested helper app bundle) and exposing the `ai-usage-gauge` CLI binary.

Formula archives are macOS-only by default (`darwin-arm64`, `darwin-x64`). Linux CLI/server binaries are built separately in CI.

## Formula

Build release archives:

```bash
scripts/build-homebrew-release.sh darwin-arm64 darwin-x64
```

The command writes archives and checksums under `dist/homebrew/`:

```text
dist/homebrew/ai-usage-gauge-<version>-darwin-arm64.tar.gz
dist/homebrew/ai-usage-gauge-<version>-darwin-arm64.tar.gz.sha256
dist/homebrew/ai-usage-gauge-<version>-darwin-x64.tar.gz
dist/homebrew/ai-usage-gauge-<version>-darwin-x64.tar.gz.sha256
```

Publish those assets to the GitHub release named `v<version>`, then render the
formula into a tap checkout:

```bash
scripts/render-homebrew-formula.sh <version> ../homebrew-tap/Formula/ai-usage-gauge.rb
```

## Cask

Build signed and notarized DMGs on macOS:

```bash
kinko exec --env APPLE_SIGNING_IDENTITY,APPLE_ID,APPLE_PASSWORD,APPLE_TEAM_ID -- \
  scripts/build-homebrew-cask-release.sh darwin-arm64 darwin-x64
```

This writes:

```text
dist/homebrew-cask/ai-usage-gauge-<version>-darwin-arm64.dmg
dist/homebrew-cask/ai-usage-gauge-<version>-darwin-arm64.dmg.sha256
dist/homebrew-cask/ai-usage-gauge-<version>-darwin-x64.dmg
dist/homebrew-cask/ai-usage-gauge-<version>-darwin-x64.dmg.sha256
```

Render the Cask:

```bash
scripts/render-homebrew-cask.sh <version> ../homebrew-tap/Casks/ai-usage-gauge.rb
```

For a tagged release, the local wrapper verifies the tag, builds DMGs, uploads
release assets, and renders the tap Cask:

```bash
kinko exec --env APPLE_SIGNING_IDENTITY,APPLE_ID,APPLE_PASSWORD,APPLE_TEAM_ID -- \
  scripts/release-homebrew-cask-local.sh v<version>
```

## Verification

From the tap checkout:

```bash
ruby -c Formula/ai-usage-gauge.rb
brew audit --strict ai-usage-gauge || brew audit --strict --formula ai-usage-gauge
brew fetch --cask user/tap/ai-usage-gauge
HOMEBREW_NO_GITHUB_API=1 brew audit --cask user/tap/ai-usage-gauge
```

If online audit fails due local GitHub credentials or rate limits, run the
non-online audit and record the limitation.
