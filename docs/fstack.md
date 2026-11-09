# F-Stack / DPDK デプロイ・検証ガイド

このドキュメントは `mc-proxy` を F-Stack（DPDK）上でビルドし、実行する
手順です。2026-11 時点の Ubuntu 24.04 + DPDK 24.11.8 + F-Stack master
（v2.0、`thread_mode=0`）で、**ビルド・リンク・net_tap 経由の実際の
Minecraft status 応答まで確認済み**です。

## 1. 検証環境と結果

| 項目 | 値 |
|---|---|
| OS | Ubuntu 24.04 (kernel 6.8) |
| コンパイラ | GCC 13.3 |
| DPDK | 24.11.8（f-stack 同梱、`-Denable_kmods=false`） |
| F-Stack | master（v2.0） |
| NIC | `net_tap` PMD（物理 NIC 不要） |
| 結果 | `backend="f-stack"` で起動、`ff_socket`/`ff_epoll_*` 経由で status ping 応答、ping RTT 0.14〜0.5 ms |

実際の起動ログ:

```text
f-stack-0: Successed to register dpdk interface
INFO mc-proxy started backend="f-stack" listen=192.168.100.2:25599 ...
```

```text
$ ping -c 1 192.168.100.2
64 bytes from 192.168.100.2: icmp_seq=1 ttl=64 time=0.145 ms
$ python3 scripts/...  # status request
F-STACK STATUS OK: §aF-Stack backend | max 42 | proto 770
```

## 2. 依存パッケージ

```bash
sudo apt-get update
sudo apt-get install -y \
  git gcc g++ make pkg-config python3 python3-pyelftools \
  meson ninja-build bc \
  libssl-dev libnuma-dev libnuma1 libpcre3 libpcre3-dev zlib1g-dev \
  libpcap-dev libelf-dev libarchive-dev libbsd-dev \
  libsystemd-dev libmd-dev nettle-dev libxml2-dev libacl1-dev \
  liblzma-dev liblz4-dev libbz2-dev libzstd-dev libdbus-1-dev \
  libibverbs-dev libnl-3-dev libnl-route-3-dev ibverbs-providers
```

後半のライブラリは、DPDK の `libdpdk.pc` が静的リンク時に要求するものです
（`-lsystemd`、`-lmd`、`-larchive`、`-libverbs` など）。不足すると
`rust-lld: unable to find library -lsystemd` のようなリンクエラーになります。

## 3. F-Stack + DPDK のビルド

```bash
git clone --depth 1 https://github.com/F-Stack/f-stack.git /tmp/f-stack
cd /tmp/f-stack/dpdk

# カーネルモジュールは不要（vfio-pci / net_tap を使う場合は false でよい）
meson setup -Denable_kmods=false -Dprefix=/usr/local build
ninja -C build
sudo ninja -C build install
sudo ldconfig

export PKG_CONFIG_PATH=/usr/local/lib/x86_64-linux-gnu/pkgconfig:\
/usr/local/lib/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/pkgconfig
pkg-config --modversion libdpdk   # => 24.11.8

cd /tmp/f-stack/lib
make -j"$(nproc)"
sudo make install PREFIX=/usr/local
sudo ldconfig
```

`make install` は `libfstack.a`、`ff_*.h`、`ff_start` をインストールし、
`/etc/f-stack.conf` がなければ `config.ini` をコピーします。

## 4. プロキシのビルド

```bash
cd /path/to/mc-proxy
FSTACK_LIB_DIR=/usr/local/lib \
PKG_CONFIG_PATH=/usr/local/lib/x86_64-linux-gnu/pkgconfig:/usr/local/lib/pkgconfig \
cargo build --release -p mc-proxy --features fstack
```

`build.rs` が以下を行います。

- `pkg-config --static --libs libdpdk` で DPDK をリンク
- `libfstack.a` を `--whole-archive` でリンク
- F-Stack が直接参照する **bonding PMD**（`rte_net_bond`）と
  **静的 timer**（`rte_timer_meta_init` は共有ライブラリに存在しない）を追加
- `--no-gc-sections` を付与

### 4.1 `--no-gc-sections` が必要な理由

F-Stack は FreeBSD の SYSINIT / VNET / PCPU を **ELF リンカーセット**
（`set_sysinit_set` などのセクション）で実行します。Rust のリンクは既定で
`--gc-sections` を付けるため、`__start_/__stop_` が weak なこれらの
セクションが破棄され、`mi_startup()` が何も実行しません。その結果
`vnet0` が未初期化のまま `ff_freebsd_init()` が `V_tcp_do_ecn` で
セグメンテーションフォルトします（`ff_freebsd_init.c:352`）。

`crates/mc-proxy/build.rs`（と `tokio-fstack/build.rs`）が
`-Wl,--no-gc-sections` を付与することで解決しています。
`nm target/release/mc-proxy | grep __start_set_sysinit_set` で
シンボルが `D`（定義済み）になっていることを確認できます。

## 5. 起動モデル（重要）

F-Stack の `ff_init`/`ff_run` は **プロセスのメインスレッド**で実行する
必要があります。`ff_freebsd_init` が per-thread の FreeBSD 状態を作り、
リンカーセットの SYSINIT を実行するためです。専用スレッドで `ff_init` を
呼ぶと `ff_freebsd_init` で落ちます。

`tokio_fstack::run_with_stack` がこの制約をカプセル化します。

1. メインスレッドで `ff_init` を実行
2. プロキシ（Tokio ランタイム）を別スレッドで起動
3. メインスレッドで `ff_run(tick)` を実行
4. アプリのソケット操作（`ff_socket`/`ff_bind`/`ff_accept`/`ff_recv`/
   `ff_send`/`ff_epoll_*`）はキュー経由で tick に渡され、メインスレッドで実行
5. アプリが終了すると `ff_stop_run()` でループを止めてプロセス終了

したがって、F-Stack バックエンドでは `workers = 1`（1 スタック）です。
マルチコアは「1 プロセス = 1 コア」で複数プロセス起動してください
（[workers.md](workers.md)）。

## 6. net_tap での動作確認

物理 NIC がなくても `net_tap` PMD で検証できます。付属の
[`scripts/run-fstack-net-tap.sh`](../scripts/run-fstack-net-tap.sh) が
hugepage 確保、設定生成、起動、tap 設定、status ping まで行います。

```bash
sudo ./scripts/run-fstack-net-tap.sh
```

手動で行う場合:

```bash
# 1. hugepages（2 MiB × 1024）
echo 1024 | sudo tee /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
sudo mkdir -p /dev/hugepages
mountpoint -q /dev/hugepages || sudo mount -t hugetlbfs nodev /dev/hugepages

# 2. f-stack 設定（/etc/f-stack.conf をコピーして編集）
#    [dpdk]
#    lcore_mask=1
#    channel=1
#    numa_on=0
#    file_prefix=mcproxy
#    extra_eal_args=--vdev=net_tap0,iface=ftap0 --no-pci
#    [port0]
#    addr=192.168.100.2
#    netmask=255.255.255.0
#    broadcast=192.168.100.255
#    gateway=192.168.100.1
#    lcore_list=0

# 3. プロキシ設定
#    bind = "192.168.100.2:25599"
#    [fstack]
#    eal_args = ["--conf", "/tmp/fstack-net-tap.conf", "--proc-type=primary"]

sudo ./target/release/mc-proxy --config /tmp/mcproxy-net-tap.toml

# 4. カーネル側 tap
sudo ip link set ftap0 down
sudo ip link set ftap0 address 02:11:22:33:44:55   # ← 重要
sudo ip link set ftap0 up
sudo ip addr add 192.168.100.1/24 dev ftap0
ping 192.168.100.2
```

### 6.1 tap の MAC アドレス

`net_tap` は作成した tap インターフェースに **DPDK ポートと同じ MAC** を
設定します。F-Stack の veth も同じ MAC を持つため、両端が同一 MAC になり
ARP の送受信が成立しません（ARP request は届くが reply が処理されない、
という症状になります）。カーネル側 tap の MAC を別の値に変更すると
ARP が通り、TCP も正常に動作します。

## 7. 実機検証で見つかった問題と対処

| 症状 | 原因 | 対処 |
|---|---|---|
| `ff_freebsd_init` で SIGSEGV（`V_tcp_do_ecn`） | `set_sysinit_set` が `--gc-sections` で破棄され `vnet0` 未初期化 | `--no-gc-sections` を付与（build.rs 済み） |
| 専用スレッドで `ff_init` すると SIGSEGV | `ff_init`/`ff_run` はメインスレッド専用 | `run_with_stack` を使う |
| `unable to find library -lsystemd` | DPDK 静的リンクの依存パッケージ不足 | 上記 apt パッケージを導入 |
| `unrecognized option '--file-prefix'` | f-stack の CLI は `--conf`/`--proc-type`/`--proc-id` のみ | `extra_eal_args`（config）に書く |
| net_tap で ping/ARP が通らない | tap と F-Stack が同一 MAC | tap の MAC を変更 |
| `ff_epoll_wait` がタイムアウトしない | f-stack 実装が常に非ブロッキング | 問題なし（tick 内でポーリング） |

## 8. マルチコア

- **現行（`thread_mode=0`）**: 1 プロセス 1 スタック。N コアは
  `workers = 1` のプロセスを N 個起動し、`SO_REUSEPORT` で待ち受ける。
  プロセス間の状態共有は未実装（`rte_ring`/`ff_ipc` が候補）。
- **native-mt（`thread_mode=1`）**: F-Stack v2.0 の 1 プロセス N スタック
  モード。現在の `tokio-fstack` は 1 スタック前提のため、lcore ごとの
  tick ディスパッチ（lcore ごとのキュー・epoll・Waker）を追加すると
  1 プロセスで N スタックを扱えます。詳細は [workers.md](workers.md)。

## 9. 参考

- [F-Stack](https://f-stack.org/) /
  [F-Stack GitHub](https://github.com/F-Stack/f-stack)
- [F-Stack Build Guide](https://github.com/F-Stack/f-stack/blob/master/doc/F-Stack_Build_Guide.md)
- [DPDK](https://doc.dpdk.org/)
