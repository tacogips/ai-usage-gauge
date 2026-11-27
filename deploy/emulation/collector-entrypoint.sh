#!/bin/sh
set -eu

is_tmpfs() {
  awk -v path="$1" '$2 == path && $3 == "tmpfs" { found=1 } END { exit !found }' /proc/mounts
}

is_tmpfs /run/ccusage-secrets
while test ! -s /run/ccusage-secrets/id_ed25519; do sleep 0.1; done
test "$(stat -c %a /run/ccusage-secrets/id_ed25519)" = 400
chown collector:collector /run/ccusage-secrets/id_ed25519

mkdir -p /runtime/config/ai-usage-gauge /runtime/state/ai-usage-gauge /runtime/cache/ai-usage-gauge
chmod 0700 /runtime/config/ai-usage-gauge /runtime/state/ai-usage-gauge /runtime/cache/ai-usage-gauge
if test ! -e /runtime/config/ai-usage-gauge/config.json; then
  umask 077
  printf '%s\n' '{"ccusagePath":"/usr/local/bin/ccusage","defaultResetTerm":"daily","dashboardPort":18081,"dashboardAutostart":true,"pollIntervalSeconds":60,"cacheRetentionDays":365}' > /runtime/config/ai-usage-gauge/config.json
fi
chown -R collector:collector /runtime

export AI_USAGE_GAUGE_CONFIG_HOME=/runtime/config
export AI_USAGE_GAUGE_STATE_HOME=/runtime/state
export AI_USAGE_GAUGE_CACHE_HOME=/runtime/cache
export HOME=/home/collector
export TZ=UTC
exec setpriv --reuid=collector --regid=collector --init-groups \
  /usr/local/bin/ai-usage-gauge serve --port 18081
