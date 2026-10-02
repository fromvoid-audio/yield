#!/usr/bin/env bash
# The canonical facade invokes current workspace checks and propagates gate failures.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/scripts/internal/agent" "$fixture/scripts/internal/check"
cp "$root/scripts/internal/agent/run_agent_ci_checks.sh" "$fixture/scripts/internal/agent/"
cp "$root/scripts/internal/agent/run_agent_request.sh" "$fixture/scripts/internal/agent/"
export AGENT_ROUTING_LOG="$fixture/log"
cat > "$fixture/scripts/internal/check/check_workflow_toolchain_pinning.sh" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
echo pinning >> "$AGENT_ROUTING_LOG"
exit "${AGENT_ROUTING_GATE_STATUS:-0}"
SCRIPT
cat > "$fixture/scripts/gpui-development-checks.sh" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
echo "gpui:$*" >> "$AGENT_ROUTING_LOG"
exit "${AGENT_ROUTING_CHECK_STATUS:-0}"
SCRIPT
cat > "$fixture/scripts/internal/agent/run_agent_preflight.sh" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
echo preflight >> "$AGENT_ROUTING_LOG"
exit "${AGENT_ROUTING_CHECK_STATUS:-0}"
SCRIPT
cat > "$fixture/scripts/gpui-local-ci.sh" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
echo "native:$*" >> "$AGENT_ROUTING_LOG"
exit 9
SCRIPT
chmod +x "$fixture/scripts/"*.sh "$fixture/scripts/internal/agent/"*.sh "$fixture/scripts/internal/check/"*.sh
"$fixture/scripts/internal/agent/run_agent_ci_checks.sh"
[[ "$(cat "$AGENT_ROUTING_LOG")" == $'pinning\ngpui:preflight' ]]
: > "$AGENT_ROUTING_LOG"
if AGENT_ROUTING_GATE_STATUS=7 "$fixture/scripts/internal/agent/run_agent_ci_checks.sh"; then exit 1; else [[ "$?" == 7 ]]; fi
[[ "$(cat "$AGENT_ROUTING_LOG")" == pinning ]]
: > "$AGENT_ROUTING_LOG"
"$fixture/scripts/internal/agent/run_agent_request.sh"
[[ "$(cat "$AGENT_ROUTING_LOG")" == preflight ]]
: > "$AGENT_ROUTING_LOG"
"$fixture/scripts/internal/agent/run_agent_request.sh" --quick-ci
[[ "$(cat "$AGENT_ROUTING_LOG")" == $'preflight\ngpui:test' ]]
: > "$AGENT_ROUTING_LOG"
if "$fixture/scripts/internal/agent/run_agent_request.sh" --full-ci; then exit 1; else [[ "$?" == 9 ]]; fi
[[ "$(cat "$AGENT_ROUTING_LOG")" == $'preflight\nnative:full' ]]
: > "$AGENT_ROUTING_LOG"
if AGENT_ROUTING_CHECK_STATUS=5 "$fixture/scripts/internal/agent/run_agent_request.sh" --quick-ci; then exit 1; else [[ "$?" == 5 ]]; fi
[[ "$(cat "$AGENT_ROUTING_LOG")" == preflight ]]
: > "$AGENT_ROUTING_LOG"
if "$fixture/scripts/internal/agent/run_agent_request.sh" --quick-ci --full-ci >/dev/null 2>&1; then exit 1; else [[ "$?" == 2 ]]; fi
[[ ! -s "$AGENT_ROUTING_LOG" ]]
echo '[agent_routing_test] OK'
