#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
port=18081
assets="$project_root/frontend/dist"
binary=""
while (($#)); do
  case "$1" in
    --port) port="$2"; shift 2 ;;
    --assets) assets="$2"; shift 2 ;;
    --binary) binary="$2"; shift 2 ;;
    --installed-assets) assets=""; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
if [[ -n "$assets" && "$assets" != /* ]]; then assets="$project_root/$assets"; fi
if [[ -n "$binary" && "$binary" != /* ]]; then binary="$project_root/$binary"; fi

if [[ -z "$binary" ]]; then
  if [[ -x "$project_root/target/debug/ccusage-gauge" ]]; then
    binary="$project_root/target/debug/ccusage-gauge"
  elif [[ -x "$project_root/target/release/ccusage-gauge" ]]; then
    binary="$project_root/target/release/ccusage-gauge"
  else
    cargo build --manifest-path "$project_root/Cargo.toml" -p ccusage-cli >/dev/null
    binary="$project_root/target/debug/ccusage-gauge"
  fi
fi

root="$(mktemp -d)"
pid=""
cleanup() {
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then kill -TERM "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  rm -rf "$root"
}
trap cleanup EXIT INT TERM

mkdir -p "$root/config/ccusage-gauge" "$root/state/ccusage-gauge" "$root/cache/ccusage-gauge" "$root/claude/projects" "$root/codex/sessions"
chmod 0700 "$root/config/ccusage-gauge" "$root/state/ccusage-gauge" "$root/cache/ccusage-gauge"
cat >"$root/config/ccusage-gauge/ccusage-config.json" <<JSON
{"ccusagePath":null,"defaultResetTerm":"daily","dashboardPort":$port,"dashboardAutostart":false,"pollIntervalSeconds":60}
JSON

now_ts="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
today="$(date '+%Y-%m-%d')"
cat >"$root/claude/projects/smoke.jsonl" <<JSONL
{"type":"assistant","timestamp":"$now_ts","sessionId":"s1","requestId":"r1","cwd":"/tmp/demo","message":{"id":"m1","role":"assistant","model":"claude-sonnet-4-5","usage":{"input_tokens":1000,"output_tokens":200,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}
JSONL

export CCUSAGE_GAUGE_CONFIG_HOME="$root/config"
export CCUSAGE_GAUGE_STATE_HOME="$root/state"
export CCUSAGE_GAUGE_CACHE_HOME="$root/cache"
export CLAUDE_CONFIG_DIR="$root/claude"
export CODEX_HOME="$root/codex"
export CCUSAGE_GAUGE_ANTIGRAVITY_DIR=""

require_free_port() {
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
    echo "port $port is already serving; stop it or pass --port <free port>" >&2
    return 1
  fi
  return 0
}

wait_ready() {
  for _ in {1..80}; do
    if [[ -n "$pid" ]] && ! kill -0 "$pid" 2>/dev/null; then
      echo "dashboard exited before becoming ready" >&2
      cat "$root/server.log" >&2
      return 1
    fi
    curl -fsS "http://127.0.0.1:$port/api/health" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "dashboard readiness timed out" >&2
  cat "$root/server.log" >&2
  return 1
}

run_once() {
  local arguments=(serve --port "$port")
  if [[ -n "$assets" ]]; then arguments+=(--assets "$assets"); fi
  "$binary" "${arguments[@]}" >"$root/server.log" 2>&1 &
  pid=$!
  wait_ready
  curl -fsS "http://127.0.0.1:$port/" | grep -q 'ccusage-gauge'
  curl -fsS "http://127.0.0.1:$port/api/recent" | grep -q 'points'
  curl -fsS "http://127.0.0.1:$port/api/day?date=$today" | grep -q 'points'
  curl -fsS "http://127.0.0.1:$port/api/period?range=today" | grep -q 'points'
  curl -fsS "http://127.0.0.1:$port/api/metrics?range=today" | grep -q 'claude-sonnet-4-5'
  curl -fsS "http://127.0.0.1:$port/api/cost-series?range=today&granularity=hourly" | grep -q 'claude-sonnet-4-5'
  curl -fsS "http://127.0.0.1:$port/api/budget" | grep -q 'spentUSD'
  cache_clear_response="$(curl -fsS \
    -H 'X-CCUsage-Gauge-Mutation: 1' \
    -H "Origin: http://127.0.0.1:$port" \
    -H 'Sec-Fetch-Site: same-origin' \
    -X DELETE "http://127.0.0.1:$port/api/cache")"
  grep -q '"local"' <<<"$cache_clear_response"
  kill -TERM "$pid"
  wait "$pid" || true
  pid=""
  ! curl -fsS "http://127.0.0.1:$port/api/health" >/dev/null 2>&1
}

require_free_port
run_once
run_once
echo "dashboard smoke passed on 127.0.0.1:$port"
