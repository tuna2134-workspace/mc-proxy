#!/usr/bin/env bash
#
# Build F-Stack (with DPDK) and link mc-proxy against it.
#
# Run as a normal user; the script uses sudo for the parts that install into
# /usr/local. Tested on Ubuntu 24.04 with F-Stack master (v2.0) and DPDK 24.11.
#
# Environment:
#   FSTACK_DIR   checkout directory           (default: /opt/f-stack)
#   FSTACK_REF   git ref to build             (default: master)
#   PREFIX       install prefix               (default: /usr/local)
#   JOBS         parallel build jobs          (default: nproc)
#
# After a successful run:
#   ./target/release/mc-proxy --config config.toml
# with [fstack] eal_args pointing at /etc/f-stack.conf.
set -euo pipefail

FSTACK_DIR=${FSTACK_DIR:-/opt/f-stack}
FSTACK_REF=${FSTACK_REF:-master}
PREFIX=${PREFIX:-/usr/local}
JOBS=${JOBS:-$(nproc)}
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

if [[ $EUID -eq 0 ]]; then
    SUDO=""
    echo "warning: running as root; build artifacts under target/ will be root-owned" >&2
else
    SUDO="sudo"
fi

echo "==> installing build dependencies"
$SUDO apt-get update
DEBIAN_FRONTEND=noninteractive $SUDO apt-get install -y \
    git gcc g++ make pkg-config libssl-dev libnuma-dev libnuma1 \
    libpcre3 libpcre3-dev zlib1g-dev python3 python3-pyelftools \
    meson ninja-build libpcap-dev libelf-dev libarchive-dev libbsd-dev bc \
    libsystemd-dev libmd-dev nettle-dev libxml2-dev libacl1-dev \
    liblzma-dev liblz4-dev libbz2-dev libzstd-dev libdbus-1-dev \
    libibverbs-dev libnl-3-dev libnl-route-3-dev ibverbs-providers

if [[ ! -d "$FSTACK_DIR/.git" ]]; then
    echo "==> cloning F-Stack ($FSTACK_REF) into $FSTACK_DIR"
    $SUDO mkdir -p "$FSTACK_DIR"
    $SUDO chown "$(id -u):$(id -g)" "$FSTACK_DIR"
    git clone --depth 1 --branch "$FSTACK_REF" \
        https://github.com/F-Stack/f-stack.git "$FSTACK_DIR"
fi

echo "==> building DPDK"
cd "$FSTACK_DIR/dpdk"
if [[ ! -d build ]]; then
    meson setup -Denable_kmods=false -Dprefix="$PREFIX" build
fi
ninja -C build
$SUDO ninja -C build install
$SUDO ldconfig

# Debian/Ubuntu install DPDK's .pc file under a multiarch directory.
export PKG_CONFIG_PATH="$PREFIX/lib/x86_64-linux-gnu/pkgconfig:$PREFIX/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
pkg-config --modversion libdpdk

echo "==> building libfstack.a"
cd "$FSTACK_DIR/lib"
make -j"$JOBS"
$SUDO make install PREFIX="$PREFIX"
$SUDO ldconfig

echo "==> building mc-proxy with the fstack backend"
cd "$ROOT"
FSTACK_LIB_DIR="$PREFIX/lib" \
    cargo build --release -p mc-proxy --features fstack

echo
echo "note: the mc-proxy/tokio-fstack build scripts add the DPDK PMD/timer"
echo "      libraries and --no-gc-sections (F-Stack keeps boot hooks in ELF"
echo "      linker-set sections)."
echo
echo "built: $ROOT/target/release/mc-proxy"
echo "check: nm $ROOT/target/release/mc-proxy | grep -E 'ff_init|ff_epoll_create'"
echo "run:   sudo $ROOT/target/release/mc-proxy --config config.toml"
echo "test:  sudo $ROOT/scripts/run-fstack-net-tap.sh"
