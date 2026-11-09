# F-Stack / DPDK デプロイガイド

このドキュメントは `mc-proxy` を F-Stack（DPDK）上で動かすための手順と
注意点をまとめたものです。カーネルソケットで試す場合は README の
クイックスタートを参照してください。

## 1. 前提

- Linux（x86_64）
- DPDK がサポートする NIC（または `net_tap` / `net_af_packet` などの仮想 PMD）
- F-Stack（`ff_epoll_create` / `ff_epoll_ctl` / `ff_epoll_wait` を提供するバージョン）
- root または `CAP_SYS_RAWIO`・`CAP_IPC_LOCK`・`CAP_NET_ADMIN` 相当の権限
- hugepage（2 MiB ページを 1024 枚程度）

## 2. F-Stack のビルド

```bash
sudo apt install -y build-essential pkg-config libnuma-dev libpcap-dev \
                    libssl-dev zlib1g-dev python3
git clone https://github.com/F-Stack/f-stack.git
cd f-stack
make
sudo make install
```

`/usr/local/lib/libfstack.a` と `libdpdk.pc` がインストールされます。

## 3. hugepage と NIC

```bash
# 2 MiB ページ × 1024
echo 1024 | sudo tee /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
sudo mkdir -p /mnt/huge
sudo mount -t hugetlbfs nodev /mnt/huge

# vfio-pci で NIC を DPDK にバインド
sudo modprobe vfio-pci enable_unsafe_noiommu_mode=1
sudo /usr/local/bin/dpdk-devbind.py --status
sudo /usr/local/bin/dpdk-devbind.py --bind=vfio-pci 0000:00:06.0
```

> 検証用途では `net_tap` PMD（`--vdev=net_tap0,iface=dtap0`）を使うと
> カーネル側から `dtap0` 経由で通信でき、デバッグが容易です。

## 4. `/etc/f-stack.conf`

最低限、DPDK の lcore とポート、スタックのアドレスを設定します。

```ini
[dpdk]
lcore_mask=1
channel=1
prompt=1
nb_mem_chunks=8192

[port0]
addr=192.168.1.10
netmask=255.255.255.0
broadcast=192.168.1.255
gateway=192.168.1.1

[stack]
# F-Stack の TCP スタック設定（必要に応じて）
```

詳細は [F-Stack の設定](https://github.com/F-Stack/f-stack/blob/master/doc/F-Stack-User-Guide.md)
を参照してください。

## 5. ビルド

```bash
FSTACK_LIB_DIR=/usr/local/lib \
PKG_CONFIG_PATH=/usr/local/lib/pkgconfig \
cargo build --release -p mc-proxy --features fstack
```

`build.rs` は

1. `pkg-config --static --libs libdpdk` の結果をリンクし、
2. `libfstack.a` を `--whole-archive` でリンクし、
3. `rt m dl crypto z pthread numa` を追加します。

共有ライブラリ版の F-Stack を使う場合:

```bash
FSTACK_LIBS="fstack dpdk" cargo build --release -p mc-proxy --features fstack
```

リンクだけ確認したい場合:

```bash
cargo check -p mc-proxy --features fstack
```

## 6. 設定と起動

```toml
bind = "0.0.0.0:25565"
forwarding_secret = "<secret>"
motd = "&aF-Stack Proxy"

[fstack]
eal_args = ["--conf", "/etc/f-stack.conf", "--proc-type=primary", "--file-prefix=mc-proxy"]

[[servers]]
name = "lobby"
address = "192.168.1.20:25566"   # IP リテラル推奨

[try]
servers = ["lobby"]
```

```bash
sudo ./target/release/mc-proxy --config config.toml
```

起動ログに `backend="f-stack"` と表示されれば F-Stack バックエンドで動作しています。

## 7. 動作の仕組み

F-Stack は `ff_init`/`ff_run` を実行したスレッドに強く結び付いた
ユーザー空間スタックです。`tokio-fstack` は

- `ff_init` → `ff_run` を実行する専用スレッドを 1 本作り、
- すべての `ff_*`（ソケット作成、bind/listen/accept、read/write、epoll、
  close）をそのスレッド上のキューで実行し、
- DPDK のパケット処理ループ（`ff_run` のコールバック）内で
  `ff_epoll_wait` を非ブロッキングでポーリングして `Waker` を起こします。

Tokio のワーカースレッドが `ff_*` を直接呼ぶことはありません。

## 8. トラブルシューティング

| 症状 | 確認ポイント |
|---|---|
| `ff_init` が失敗する | hugepage の枚数、NIC のバインド、`/etc/f-stack.conf`、`--proc-type`、権限 |
| 起動直後に落ちる | 別プロセスが同じ `--file-prefix` を使っていないか |
| クライアントが接続できない | `bind` のアドレスが F-Stack 側のポートと一致しているか、ARP/ルート設定 |
| Mojang 認証に失敗する | F-Stack 側からインターネットに出られるか（`online_mode = false` で切り分け） |
| バックエンドに接続できない | アドレスを IP リテラルにする、F-Stack 側のルートを確認 |

## 9. 性能に関するメモ

- I/O 1 回ごとに 1 回のクロススレッド・ハンドオフが発生します。
  スループット重視の場合は、将来 `readiness` を共有メモリで通知する
  バッチ実行モデルに置き換える余地があります。
- Minecraft のプロトコルはパケットが小さく、接続数あたりの帯域も
  小さいため、このオーバーヘッドは通常ボトルネックになりません。
- `ff_epoll_wait` は毎ループ呼ばれるため、待機中のソケットが多い場合は
  kqueue のイベント数に比例したコストになります。接続数を増やす場合は
  複数プロセス（`--proc-type=secondary` + RSS）での分割も検討してください。
