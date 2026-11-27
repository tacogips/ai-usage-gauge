#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
product="ai-usage-gauge"
artifact_name="ai-usage-gauge"

usage() {
  cat <<EOF
Usage:
  scripts/build-homebrew-cask-release.sh [--dry-run] [target ...]

Targets:
  darwin-arm64  darwin-x64

This builder stages signed, notarized, and stapled macOS .dmg artifacts for a
pure Rust + Tauri v2 Homebrew Cask.
EOF
}

require_command() { command -v "$1" >/dev/null 2>&1 || { printf 'missing required command: %s\n' "$1" >&2; return 1; }; }
require_env() { [[ -n "${!1:-}" ]] || { printf 'missing required environment variable: %s\n' "$1" >&2; return 1; }; }

detect_target() {
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64) printf '%s\n' "darwin-arm64" ;;
    Darwin:x86_64) printf '%s\n' "darwin-x64" ;;
    *) printf 'unsupported cask host platform\n' >&2; return 1 ;;
  esac
}

validate_target() {
  case "$1" in
    darwin-arm64 | darwin-x64) ;;
    *) printf 'unsupported cask target: %s\n' "$1" >&2; return 1 ;;
  esac
}

validate_version() {
  local version="$1"
  if [[ "$version" == *..* || ! "$version" =~ ^[0-9]+[.][0-9]+[.][0-9]+([-+][0-9A-Za-z][0-9A-Za-z.+-]*)?$ ]]; then
    printf 'unsafe cask version: %s\n' "$version" >&2
    return 1
  fi
}

absolute_path() {
  case "$1" in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s/%s\n' "$repo_root" "$1" ;;
  esac
}

rust_target_for() {
  case "$1" in
    darwin-arm64) printf '%s\n' "aarch64-apple-darwin" ;;
    darwin-x64) printf '%s\n' "x86_64-apple-darwin" ;;
  esac
}

expected_arch_for_target() {
  case "$1" in
    darwin-arm64) printf '%s\n' "arm64" ;;
    darwin-x64) printf '%s\n' "x86_64" ;;
  esac
}

verify_staged_binary() {
  local binary="$1" target="$2" expected_version="$3"
  if [[ "$(lipo -archs "$binary")" != "$(expected_arch_for_target "$target")" ]]; then
    printf 'staged binary is not %s: %s\n' "$target" "$binary" >&2
    return 1
  fi
  if [[ "$target" == "$(detect_target)" ]]; then
    "$binary" --version | grep -Fq "$expected_version" || return 1
  else
    grep -Fq "version = \"$expected_version\"" "$repo_root/Cargo.toml" || return 1
  fi
}

write_sha256() {
  local file="$1"
  ( cd "$(dirname "$file")" && shasum -a 256 "$(basename "$file")" )
}

package_version() {
  if [[ -n "${RELEASE_VERSION:-}" ]]; then
    printf '%s\n' "$RELEASE_VERSION"
  else
    tr -d '[:space:]' < "$repo_root/VERSION"
  fi
}

print_plan() {
  local version="$1" target="$2" release_dir="$3"
  local triple work_dir dmg_path staged_app
  triple="$(rust_target_for "$target")"
  work_dir="$release_dir/work/$artifact_name-$version-$target"
  dmg_path="$release_dir/$artifact_name-$version-$target.dmg"
  staged_app="$work_dir/AIUsageGauge.app"

  printf 'Rust Homebrew Cask DMG plan\n'
  printf '  product: %s\n' "$product"
  printf '  target: %s\n' "$target"
  printf '  rust target: %s\n' "$triple"
  printf '  staged app bundle: %s\n' "$staged_app"
  printf '  notarized DMG: %s\n' "$dmg_path"
  printf '  checksum: %s.sha256\n' "$dmg_path"
  printf '  publish side effects: false\n'
}

build_target() {
  local version="$1" target="$2" release_dir="$3"
  local triple work_dir dmg_path staged_app bin_dir notarytool stapler
  triple="$(rust_target_for "$target")"
  work_dir="$release_dir/work/$artifact_name-$version-$target"
  dmg_path="$release_dir/$artifact_name-$version-$target.dmg"
  staged_app="$work_dir/AIUsageGauge.app"
  bin_dir="$repo_root/target/$triple/release"
  notarytool="${NOTARYTOOL:-/Applications/Xcode.app/Contents/Developer/usr/bin/notarytool}"
  stapler="${STAPLER:-/Applications/Xcode.app/Contents/Developer/usr/bin/stapler}"

  require_env APPLE_SIGNING_IDENTITY
  require_env APPLE_ID
  require_env APPLE_PASSWORD
  require_env APPLE_TEAM_ID
  require_command codesign
  require_command hdiutil
  require_command security
  require_command spctl
  test -x "$notarytool"
  test -x "$stapler"

  rm -rf "$work_dir" "$dmg_path" "$dmg_path.sha256"
  mkdir -p "$staged_app/Contents/MacOS" "$staged_app/Contents/Resources/Web"

  (
    cd "$repo_root"
    cargo build --locked --release -p ai-usage-cli -p ai-usage-gauge-app --target "$triple"
  )

  cp "$bin_dir/$product" "$bin_dir/ai-usage-gauge-app" "$staged_app/Contents/MacOS/"
  chmod 0755 "$staged_app/Contents/MacOS/$product" "$staged_app/Contents/MacOS/ai-usage-gauge-app"
  verify_staged_binary "$staged_app/Contents/MacOS/$product" "$target" "$version"
  cp "$repo_root/Resources/AppIcon.icns" "$staged_app/Contents/Resources/"
  cp -R "$repo_root/frontend/dist/." "$staged_app/Contents/Resources/Web/"
  cp "$repo_root/Resources/MenuBarInfo.plist" "$staged_app/Contents/Info.plist"
  plutil -replace CFBundleShortVersionString -string "$version" "$staged_app/Contents/Info.plist"
  plutil -replace CFBundleVersion -string "$version" "$staged_app/Contents/Info.plist"

  codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$staged_app/Contents/MacOS/$product"
  codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$staged_app/Contents/MacOS/ai-usage-gauge-app"
  codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$staged_app"
  codesign --verify --deep --strict --verbose=2 "$staged_app"

  hdiutil create -quiet -fs HFS+ -format UDZO -volname "$product" -srcfolder "$work_dir" "$dmg_path"
  codesign --force --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$dmg_path"
  codesign --verify --strict --verbose=2 "$dmg_path"
  "$notarytool" submit "$dmg_path" --apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID" --wait
  "$stapler" staple "$dmg_path"
  "$stapler" validate "$dmg_path"
  spctl --assess --type open --context context:primary-signature --verbose=4 "$dmg_path"
  write_sha256 "$dmg_path" > "$dmg_path.sha256"

  printf 'built %s\n' "$dmg_path"
  cat "$dmg_path.sha256"
}

main() {
  local dry_run=false
  if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then usage; return; fi
  if [[ "${1:-}" == "--dry-run" ]]; then dry_run=true; shift; fi
  if [[ "$(uname -s)" != "Darwin" ]]; then printf 'Homebrew Cask DMG builds must run on macOS.\n' >&2; return 1; fi

  local version release_dir
  version="$(package_version)"
  validate_version "$version"
  release_dir="$(absolute_path "${CASK_RELEASE_DIR:-dist/homebrew-cask}")"

  local -a targets
  if [[ "$#" -eq 0 ]]; then targets=("$(detect_target)"); else targets=("$@"); fi

  local target
  for target in "${targets[@]}"; do
    validate_target "$target"
    if [[ "$dry_run" == true ]]; then
      print_plan "$version" "$target" "$release_dir"
    else
      mkdir -p "$release_dir"
      build_target "$version" "$target" "$release_dir"
    fi
  done
}

main "$@"
