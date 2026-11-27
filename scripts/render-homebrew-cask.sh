#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
artifact_name="ai-usage-gauge"
product="ai-usage-gauge"

usage() {
  cat <<EOF
Usage:
  scripts/render-homebrew-cask.sh <version> [output-file]

Reads archive checksums from:
  dist/homebrew-cask/$artifact_name-<version>-<target>.dmg.sha256
EOF
}

sha_for_target() {
  local version="$1" target="$2" release_dir="$3"
  local sha_file="$release_dir/$artifact_name-$version-$target.dmg.sha256"
  if [[ ! -f "$sha_file" ]]; then
    printf 'missing checksum file: %s\n' "$sha_file" >&2
    return 1
  fi
  awk '{print $1}' "$sha_file"
}

main() {
  if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then usage; return; fi
  if [[ "${1:-}" == "" ]]; then usage; return 2; fi

  local version="$1"
  local output="${2:-$repo_root/Casks/$artifact_name.rb}"
  local release_dir="${CASK_RELEASE_DIR:-$repo_root/dist/homebrew-cask}"
  local release_base_url="${CASK_RELEASE_BASE_URL:-https://github.com/tacogips/ai-usage-gauge/releases/download/v$version}"

  local darwin_arm64_sha darwin_x64_sha
  darwin_arm64_sha="$(sha_for_target "$version" darwin-arm64 "$release_dir")"
  darwin_x64_sha="$(sha_for_target "$version" darwin-x64 "$release_dir")"

  mkdir -p "$(dirname "$output")"
  cat > "$output" <<EOF
cask "ai-usage-gauge" do
  arch arm: "darwin-arm64", intel: "darwin-x64"

  version "$version"
  sha256 arm:   "$darwin_arm64_sha",
         intel: "$darwin_x64_sha"

  url "$release_base_url/$artifact_name-#{version}-#{arch}.dmg"
  name "AI Usage Gauge"
  desc "Menu bar gauge for Claude Code, Codex, and Antigravity subscription limits, credits, and costs"
  homepage "https://github.com/tacogips/ai-usage-gauge"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: :sonoma

  app "AIUsageGauge.app"
  binary "#{appdir}/AIUsageGauge.app/Contents/MacOS/$product", target: "$product"

  caveats do
    <<~EOS
      The app and native dashboard are signed and notarized with Apple Developer ID.
      Local usage is computed natively in Rust without requiring a local ccusage CLI.
    EOS
  end
end
EOF

  printf 'rendered %s\n' "$output"
}

main "$@"
