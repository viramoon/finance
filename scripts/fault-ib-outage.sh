#!/usr/bin/env bash
# Block only the local finance client's IPv4 traffic to the configured paper port.
set -euo pipefail
duration=${1:-120}
if (( $# > 1 )) || [[ ! $duration =~ ^[1-9][0-9]{0,3}$ ]] || (( duration > 1800 )); then
    echo 'Usage: fault-ib-outage.sh [duration_seconds: 1..1800]' >&2
    exit 2
fi
endpoint=${IB_GATEWAY_ADDRESS:-127.0.0.1:4002}
if [[ ${endpoint##*:} != 4002 ]]; then
    echo 'This drill requires paper Gateway TCP port 4002.' >&2
    exit 2
fi
ip=$(python3 - "${endpoint%:*}" <<'PY_IP'
import ipaddress
import sys

try:
    addr = ipaddress.IPv4Address(sys.argv[1])
except ValueError:
    sys.exit("This drill requires a numeric IPv4 loopback address.")
if not addr.is_loopback:
    sys.exit("This drill requires a loopback paper Gateway.")
print(addr)
PY_IP
)
# Run the bounded sleep and both cleanup paths under the same sudo session.
sudo bash -s -- "$duration" "$ip" <<'ROOT'
set -euo pipefail
duration=$1
ip=$2
label="finance-ib-outage-$$"
rule=(-p tcp -d "$ip" --dport 4002 -m comment --comment "$label" -j DROP)
cleanup() {
    if iptables -w -C OUTPUT "${rule[@]}" 2>/dev/null; then
        iptables -w -D OUTPUT "${rule[@]}"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP
iptables -w -I OUTPUT 1 "${rule[@]}"
printf 'Blocked TCP %s:4002 for %s seconds (rule %s)\n' "$ip" "$duration" "$label"
# An orphaned watchdog also removes our own uniquely identified rule if the
# foreground shell is killed before its EXIT trap can run.
(sleep "$duration"; cleanup) &
guard=$!
wait "$guard"
cleanup
printf 'Restored TCP %s:4002\n' "$ip"
ROOT
