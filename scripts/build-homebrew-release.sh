#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
product="ai-usage-gauge"
artifact_name="ai-usage-gauge"

usage() {
  cat <<EOF
Usage:
  scripts/build-homebrew-release.sh [--dry-run] [target ...]

Targets:
  darwin-arm64  darwin-x64

Environment:
  RELEASE_VERSION       Override package version used in archive names.
  RELEASE_DIR           Output directory. Defaults to dist/homebrew.

This builder stages Rust + Tauri v2 macOS archives for a Homebrew formula.
EOF
}

detect_target() {
  local kernel arch
  kernel="$(uname -s)"
  arch="$(uname -m)"
  case "$kernel:$arch" in
    Darwin:arm64) printf '%s\n' "darwin-arm64" ;;
    Darwin:x86_64) printf '%s\n' "darwin-x64" ;;
    *) printf 'unsupported Homebrew host platform: %s/%s\n' "$kernel" "$arch" >&2; return 1 ;;
  esac
}

validate_target() {
  case "$1" in
    darwin-arm64 | darwin-x64) ;;
    *) printf 'unsupported Homebrew target: %s\n' "$1" >&2; usage >&2; return 1 ;;
  esac
}

validate_version() {
  local version="$1"
  if [[ "$version" == *..* || ! "$version" =~ ^[0-9]+[.][0-9]+[.][0-9]+([-+][0-9A-Za-z][0-9A-Za-z.+-]*)?$ ]]; then
    printf 'unsafe release version: %s\n' "$version" >&2
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
  local file="$1" dir base
  dir="$(dirname "$file")"
  base="$(basename "$file")"
  ( cd "$dir" && shasum -a 256 "$base" )
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
  local triple work_dir archive
  triple="$(rust_target_for "$target")"
  work_dir="$release_dir/work/$artifact_name-$version-$target"
  archive="$release_dir/$artifact_name-$version-$target.tar.gz"

  printf 'Rust Homebrew archive plan\n'
  printf '  product: %s\n' "$product"
  printf '  target: %s\n' "$target"
  printf '  rust target: %s\n' "$triple"
  printf '  build command: cargo build --locked --release -p ai-usage-cli -p ai-usage-gauge-app --target %s\n' "$triple"
  printf '  staged binaries: %s/bin/%s, %s/bin/ai-usage-gauge-app\n' "$work_dir" "$product" "$work_dir"
  printf '  archive: %s\n' "$archive"
  printf '  checksum: %s.sha256\n' "$archive"
  printf '  publish side effects: false\n'
}

build_target() {
  local version="$1" target="$2" release_dir="$3"
  local triple work_dir archive bin_dir
  triple="$(rust_target_for "$target")"
  work_dir="$release_dir/work/$artifact_name-$version-$target"
  archive="$release_dir/$artifact_name-$version-$target.tar.gz"
  bin_dir="$repo_root/target/$triple/release"

  rm -rf "$work_dir" "$archive" "$archive.sha256"
  mkdir -p "$work_dir/bin" "$work_dir/share/ai-usage-gauge/web"

  (
    cd "$repo_root"
    cargo build --locked --release -p ai-usage-cli -p ai-usage-gauge-app --target "$triple"
  )

  cp "$bin_dir/$product" "$work_dir/bin/$product"
  cp "$bin_dir/ai-usage-gauge-app" "$work_dir/bin/ai-usage-gauge-app"
  chmod 0755 "$work_dir/bin/$product" "$work_dir/bin/ai-usage-gauge-app"
  verify_staged_binary "$work_dir/bin/$product" "$target" "$version"
  cp -R "$repo_root/frontend/dist/." "$work_dir/share/ai-usage-gauge/web/"
  cp "$repo_root/README.md" "$repo_root/LICENSE" "$work_dir/"

  tar -C "$work_dir" -czf "$archive" .
  write_sha256 "$archive" > "$archive.sha256"

  printf 'built %s\n' "$archive"
  cat "$archive.sha256"
}

main() {
  local dry_run=false
  if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then usage; return; fi
  if [[ "${1:-}" == "--dry-run" ]]; then dry_run=true; shift; fi

  local version release_dir
  version="$(package_version)"
  validate_version "$version"
  release_dir="$(absolute_path "${RELEASE_DIR:-dist/homebrew}")"

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
