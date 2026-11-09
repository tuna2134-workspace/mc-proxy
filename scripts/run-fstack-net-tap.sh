#!/usr/bin/env bash
#
# Run mc-proxy on F-Stack using the net_tap PMD, then verify a Minecraft
# server-list ping end to end through the F-Stack stack.
#
# Prerequisites:
#   * scripts/build-fstack.sh has been run (F-Stack + DPDK installed)
#   * cargo build --release -p mc-proxy --features fstack
#   * root (hugepages, TUN/TAP, network namespace changes)
#
# Environment:
#   FSTACK_IP   F-Stack interface address   (default: 192.168.100.2)
#   KERNEL_IP   kernel tap address          (default: 192.168.100.1)
#   TAP         tap interface name          (default: ftap0)
#   PORT        proxy port                  (default: 25599)
#   BIN         proxy binary                (default: target/release/mc-proxy)
#   KEEP=1      leave the proxy/tap running on exit
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
FSTACK_IP=${FSTACK_IP:-192.168.100.2}
KERNEL_IP=${KERNEL_IP:-192.168.100.1}
TAP=${TAP:-ftap0}
PORT=${PORT:-25599}
BIN=${BIN:-$ROOT/target/release/mc-proxy}
CONF=/tmp/fstack-net-tap.conf
PROXY_CONF=/tmp/mcproxy-net-tap.toml
LOG=/tmp/mcproxy-net-tap.log
PID=""

if [[ $EUID -ne 0 ]]; then
    echo "run as root (hugepages/TUN): sudo $0" >&2
    exit 1
fi
if ! nm "$BIN" 2>/dev/null | grep -q "T ff_init"; then
    echo "$BIN is not an F-Stack build; rebuild with:" >&2
    echo "  cargo build --release -p mc-proxy --features fstack" >&2
    exit 1
fi

cleanup() {
    if [[ "${KEEP:-0}" != "1" ]]; then
        if [[ -n "$PID" ]]; then
            kill "$PID" 2>/dev/null || true
            for _ in $(seq 1 20); do
                kill -0 "$PID" 2>/dev/null || break
                sleep 0.1
            done
            kill -9 "$PID" 2>/dev/null || true
        fi
        ip link del "$TAP" 2>/dev/null || true
    fi
}
trap cleanup EXIT

echo "==> hugepages"
echo 1024 > /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
mountpoint -q /dev/hugepages || mount -t hugetlbfs nodev /dev/hugepages

echo "==> F-Stack config ($CONF)"
cp /etc/f-stack.conf "$CONF"
python3 - "$CONF" "$FSTACK_IP" "$KERNEL_IP" "$TAP" <<'PY'
import re, sys
path, fstack_ip, kernel_ip, tap = sys.argv[1:]
s = open(path).read()
s = re.sub(r'(?m)^channel=.*$', 'channel=1', s)
# Minecraft-style small-packet proxy tuning.
for key, value in {
    'pkt_tx_delay': '0',
    'idle_loop_interval': '0',
    'idle_sleep': '0',
    'net.inet.tcp.delayed_ack': '0',
    'net.inet.tcp.sendspace': '65536',
    'net.inet.tcp.recvspace': '65536',
}.items():
    s = re.sub(rf'(?m)^{re.escape(key)}=.*$', f'{key}={value}', s)
s = re.sub(r'(?m)^numa_on=.*$', 'numa_on=0', s)
s = s.replace('#file_prefix=dpdk',
              f'file_prefix=mcproxy\nextra_eal_args=--vdev=net_tap0,iface={tap} --no-pci')
# [port0] address / peer
s = re.sub(r'(?ms)^\[port0\]\n(.*?)(?=^\[|\Z)', lambda m: '[port0]\n' + m.group(1)
           .replace('addr=192.168.1.2', f'addr={fstack_ip}')
           .replace('broadcast=192.168.1.255', f'broadcast={fstack_ip.rsplit(".", 1)[0]}.255')
           .replace('gateway=192.168.1.1', f'gateway={kernel_ip}')
           .replace('#lcore_list=0', 'lcore_list=0'), s)
open(path, 'w').write(s)
PY

echo "==> proxy config ($PROXY_CONF)"
cat > "$PROXY_CONF" <<EOF
bind = "$FSTACK_IP:$PORT"
motd = "&aF-Stack net_tap test"
max_players = 42
forwarding_secret = "net-tap-secret"

[fstack]
eal_args = ["--conf", "$CONF", "--proc-type=primary"]

[[servers]]
name = "lobby"
address = "127.0.0.1:25566"

[try]
servers = ["lobby"]
EOF

echo "==> starting $BIN"
rm -rf /var/run/dpdk/container-mcproxy
RUST_LOG=info "$BIN" --config "$PROXY_CONF" > "$LOG" 2>&1 &
PID=$!
for _ in $(seq 1 100); do
    grep -q "mc-proxy started" "$LOG" 2>/dev/null && break
    kill -0 "$PID" 2>/dev/null || { echo "proxy died:"; tail -30 "$LOG"; exit 1; }
    sleep 0.2
done
grep -q "mc-proxy started" "$LOG" || { echo "proxy did not start:"; tail -30 "$LOG"; exit 1; }

echo "==> configuring kernel tap $TAP"
ip link set "$TAP" down
# net_tap gives the tap the same MAC as the DPDK port; distinct MACs make ARP work.
ip link set "$TAP" address 02:11:22:33:44:55
ip link set "$TAP" up
ip addr flush dev "$TAP"
ip addr add "$KERNEL_IP/24" dev "$TAP"
ip neigh flush dev "$TAP"
sleep 0.5

ping -c 1 -W 2 "$FSTACK_IP" > /dev/null && echo "==> ping $FSTACK_IP: OK"

echo "==> Minecraft status request through F-Stack"
python3 - "$FSTACK_IP" "$PORT" <<'PY'
import json, socket, struct, sys

host, port = sys.argv[1], int(sys.argv[2])

def vi(n):
    n &= 0xffffffff
    out = b''
    while True:
        if n & ~0x7f == 0:
            return out + bytes([n])
        out += bytes([(n & 0x7f) | 0x80])
        n >>= 7

def read_varint(sock):
    num = shift = 0
    while True:
        b = sock.recv(1)[0]
        num |= (b & 0x7f) << shift
        shift += 7
        if not b & 0x80:
            return num

s = socket.create_connection((host, port), timeout=5)
addr = b'localhost'
handshake = vi(0) + vi(770) + vi(len(addr)) + addr + struct.pack('>H', 25565) + vi(1)
s.sendall(vi(len(handshake)) + handshake)
s.sendall(vi(1) + b'\x00')
length = read_varint(s)
body = b''
while len(body) < length:
    chunk = s.recv(length - len(body))
    if not chunk:
        raise SystemExit('connection closed')
    body += chunk
i = 1
n = shift = 0
while True:
    b = body[i]
    i += 1
    n |= (b & 0x7f) << shift
    shift += 7
    if not b & 0x80:
        break
status = json.loads(body[i:i + n])
print('    version:', status['version'])
print('    players:', status['players']['max'])
print('    motd:   ', status['description']['text'])
print('F-STACK STATUS OK')
PY

echo
echo "proxy log: $LOG"
echo "KEEP=1 to leave the proxy and $TAP running."
