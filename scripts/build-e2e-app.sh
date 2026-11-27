#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output_root="${AI_USAGE_GAUGE_E2E_DIR:-$project_root/target/e2e}"
app="$output_root/AIUsageGaugeE2E.app"
version="${AI_USAGE_GAUGE_VERSION:-$(tr -d '[:space:]' < "$project_root/VERSION")}"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/Web"

cargo build --manifest-path "$project_root/Cargo.toml" --release -p ai-usage-cli -p ai-usage-gauge-app >/dev/null
cp "$project_root/target/release/ai-usage-gauge-app" "$app/Contents/MacOS/ai-usage-gauge-app"
cp "$project_root/target/release/ai-usage-gauge" "$app/Contents/MacOS/ai-usage-gauge"
chmod 0755 "$app/Contents/MacOS/ai-usage-gauge-app" "$app/Contents/MacOS/ai-usage-gauge"
cp "$project_root/Resources/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
cp -R "$project_root/frontend/dist"/. "$app/Contents/Resources/Web"/

cp "$project_root/Resources/MenuBarInfo.plist" "$app/Contents/Info.plist"
plutil -replace CFBundleIdentifier -string "com.tacogips.ai-usage-gauge.e2e" "$app/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$version" "$app/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$version" "$app/Contents/Info.plist"

plutil -lint "$app/Contents/Info.plist" >/dev/null
codesign --force --sign - "$app" >/dev/null
printf 'built e2e app at %s\n' "$app"
