#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
layout=all
missing=false
while (($#)); do
  case "$1" in
    --layout) layout="$2"; shift 2 ;;
    --expect-missing-diagnostics) missing=true; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
case "$layout" in formula|cask|all) ;; *) echo "invalid layout: $layout" >&2; exit 2 ;; esac

source_assets="$project_root/frontend/dist"
test -f "$source_assets/index.html"
if [[ -x "$project_root/target/debug/ai-usage-gauge" ]]; then
  source_binary="$project_root/target/debug/ai-usage-gauge"
elif [[ -x "$project_root/target/release/ai-usage-gauge" ]]; then
  source_binary="$project_root/target/release/ai-usage-gauge"
else
  cargo build --manifest-path "$project_root/Cargo.toml" -p ai-usage-cli >/dev/null
  source_binary="$project_root/target/debug/ai-usage-gauge"
fi

root="$(mktemp -d)"
pid=""
cleanup() { if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then kill -TERM "$pid" || true; wait "$pid" || true; fi; rm -rf "$root"; }
trap cleanup EXIT INT TERM

mkdir -p "$root/config/ai-usage-gauge" "$root/state" "$root/cache" "$root/claude" "$root/codex"
chmod 0700 "$root" "$root/config" "$root/config/ai-usage-gauge"
cat >"$root/config/ai-usage-gauge/config.json" <<'JSON'
{"ccusagePath":null,"defaultResetTerm":"daily","dashboardPort":18082,"dashboardAutostart":false,"pollIntervalSeconds":60}
JSON
chmod 0600 "$root/config/ai-usage-gauge/config.json"
export AI_USAGE_GAUGE_CONFIG_HOME="$root/config"
export AI_USAGE_GAUGE_STATE_HOME="$root/state"
export AI_USAGE_GAUGE_CACHE_HOME="$root/cache"
export CLAUDE_CONFIG_DIR="$root/claude"
export CODEX_HOME="$root/codex"
export AI_USAGE_GAUGE_ANTIGRAVITY_DIR=""

if (exec 3<>/dev/tcp/127.0.0.1/18082) 2>/dev/null; then
  echo "port 18082 is already serving; stop that process before running this smoke" >&2
  exit 1
fi

probe() {
  local name="$1" binary="$2" asset_root="$3"
  if ! $missing; then mkdir -p "$asset_root"; cp -R "$source_assets"/. "$asset_root"/; else rm -rf "$asset_root"; fi
  "$binary" serve --port 18082 >"$root/$name.log" 2>&1 & pid=$!
  for _ in {1..80}; do curl -fsS http://127.0.0.1:18082/api/health >/dev/null 2>&1 && break; sleep .1; done
  if $missing; then
    code="$(curl -sS -o "$root/body" -w '%{http_code}' http://127.0.0.1:18082/)"
    test "$code" = 503; grep -q assets_missing "$root/body"
  else
    curl -fsS http://127.0.0.1:18082/ | grep -q ai-usage-gauge
  fi
  kill -TERM "$pid"; wait "$pid" || true; pid=""
  ! curl -fsS http://127.0.0.1:18082/api/health >/dev/null 2>&1
}

run_layout() {
  local name="$1" stage="$root/$1"
  case "$name" in
    formula)
      mkdir -p "$stage/bin"; cp "$source_binary" "$stage/bin/ai-usage-gauge"
      probe "$name" "$stage/bin/ai-usage-gauge" "$stage/share/ai-usage-gauge/web" ;;
    cask)
      mkdir -p "$stage/AIUsageGauge.app/Contents/MacOS"; cp "$source_binary" "$stage/AIUsageGauge.app/Contents/MacOS/ai-usage-gauge"
      probe "$name" "$stage/AIUsageGauge.app/Contents/MacOS/ai-usage-gauge" "$stage/AIUsageGauge.app/Contents/Resources/Web" ;;
  esac
}

if [[ "$layout" = all ]]; then for value in formula cask; do run_layout "$value"; done; else run_layout "$layout"; fi
echo "packaged asset smoke passed: layout=$layout missing=$missing"
