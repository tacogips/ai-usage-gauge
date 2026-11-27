#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
app="${CCUSAGE_GAUGE_LOCAL_APP:-$project_root/target/CCUsageGauge.app}"
version="${CCUSAGE_GAUGE_VERSION:-$(tr -d '[:space:]' < "$project_root/VERSION")}"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/Web"

cargo build --manifest-path "$project_root/Cargo.toml" --release -p ccusage-cli -p ccusage-gauge-app >/dev/null
cp "$project_root/target/release/ccusage-gauge-app" "$app/Contents/MacOS/ccusage-gauge-app"
cp "$project_root/target/release/ccusage-gauge" "$app/Contents/MacOS/ccusage-gauge"
chmod 0755 "$app/Contents/MacOS/ccusage-gauge-app" "$app/Contents/MacOS/ccusage-gauge"
cp "$project_root/Resources/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
cp -R "$project_root/frontend/dist"/. "$app/Contents/Resources/Web"/

cp "$project_root/Resources/MenuBarInfo.plist" "$app/Contents/Info.plist"
plutil -replace CFBundleIdentifier -string "com.tacogips.ccusage-gauge.debug" "$app/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$version" "$app/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$version" "$app/Contents/Info.plist"

plutil -lint "$app/Contents/Info.plist" >/dev/null
codesign --force --sign - "$app" >/dev/null
printf 'built local app at %s\n' "$app"
