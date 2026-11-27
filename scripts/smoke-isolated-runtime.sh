#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
real_config="${HOME}/.config/ai-usage-gauge/config.json"
real_state="${HOME}/.local/ai-usage-gauge/state.json"

fingerprint() {
  if [[ ! -e "$1" ]]; then
    echo absent
    return
  fi
  if stat -f '%p:%z:%m' "$1" >/dev/null 2>&1; then
    stat -f '%p:%z:%m' "$1"
  else
    stat -c '%a:%s:%Y' "$1"
  fi
  shasum -a 256 "$1"
}

before_config="$(fingerprint "$real_config")"
before_state="$(fingerprint "$real_state")"
root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT INT TERM

if [[ -x "$project_root/target/debug/ai-usage-gauge" ]]; then
  binary="$project_root/target/debug/ai-usage-gauge"
elif [[ -x "$project_root/target/release/ai-usage-gauge" ]]; then
  binary="$project_root/target/release/ai-usage-gauge"
else
  cargo build --manifest-path "$project_root/Cargo.toml" -p ai-usage-cli >/dev/null
  binary="$project_root/target/debug/ai-usage-gauge"
fi

mkdir -p "$root/bin" "$root/claude/projects" "$root/codex/sessions"
marker="$root/path-fallback-invoked"
cat >"$root/bin/ccusage" <<FAKE
#!/usr/bin/env bash
touch "$marker"
exit 99
FAKE
chmod +x "$root/bin/ccusage"

now_ts="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
cat >"$root/claude/projects/fixture.jsonl" <<JSONL
{"type":"assistant","timestamp":"$now_ts","sessionId":"s1","requestId":"r1","cwd":"/tmp/demo","message":{"id":"m1","role":"assistant","model":"claude-sonnet-4-5","usage":{"input_tokens":100,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}
JSONL

export PATH="$root/bin:$PATH"
export AI_USAGE_GAUGE_CONFIG_HOME="$root/config"
export AI_USAGE_GAUGE_STATE_HOME="$root/state"
export AI_USAGE_GAUGE_CACHE_HOME="$root/cache"
export CLAUDE_CONFIG_DIR="$root/claude"
export CODEX_HOME="$root/codex"
export AI_USAGE_GAUGE_ANTIGRAVITY_DIR=""

# 1. Default ccusagePath: null succeeds without invoking sentinel ccusage on PATH.
"$binary" config-check >/dev/null
test -f "$root/config/ai-usage-gauge/config.json"
"$binary" usage-snapshot --json | grep -q 'costSinceResetUSD'
test ! -e "$marker"

# 2. Nonexistent absolute ccusagePath succeeds locally without invoking sentinel ccusage.
cat >"$root/config/ai-usage-gauge/config.json" <<'JSON'
{"ccusagePath":"/definitely/missing/ccusage","defaultResetTerm":"daily","dashboardPort":18081,"dashboardAutostart":false,"pollIntervalSeconds":60}
JSON
"$binary" config-check >/dev/null
"$binary" usage-snapshot --json | grep -q 'costSinceResetUSD'
test ! -e "$marker"

# 3. Relative ccusagePath is rejected with exit 1 without mutating config.
cat >"$root/config/ai-usage-gauge/config.json" <<'JSON'
{"ccusagePath":"relative/ccusage","defaultResetTerm":"daily","dashboardPort":18081,"dashboardAutostart":false,"pollIntervalSeconds":60}
JSON
before_bad="$(fingerprint "$root/config/ai-usage-gauge/config.json")"
set +e
"$binary" config-check >/dev/null 2>&1; config_status=$?
"$binary" usage-snapshot --json >/dev/null 2>&1; snapshot_status=$?
set -e
test "$config_status" -eq 1
test "$snapshot_status" -eq 1
test "$(fingerprint "$root/config/ai-usage-gauge/config.json")" = "$before_bad"
test ! -e "$marker"
test "$(fingerprint "$real_config")" = "$before_config"
test "$(fingerprint "$real_state")" = "$before_state"

echo "isolated runtime smoke passed"
