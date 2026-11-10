# パフォーマンス比較: カーネルソケット vs F-Stack

同一ホスト・同一負荷で、標準バックエンド（カーネルソケット）と
F-Stack バックエンド（`net_tap` PMD）を比較した結果です。

## 1. 環境と方法

| 項目 | 値 |
|---|---|
| CPU | 8 vCPU（VM） |
| OS | Ubuntu 24.04 / kernel 6.8 |
| DPDK | 24.11.8 |
| F-Stack | v2.0 (master, `thread_mode=0`) |
| F-Stack の NIC | `net_tap`（物理 NIC なし） |
| クライアント | `examples/bench`（カーネルソケット、8 スレッド、5 秒） |
| バックエンド | 同ホストのカーネル側 fake Paper（`bench backend`） |
| プロキシ | 1 コアまたは 2 コアに `taskset` で固定 |

負荷の種類:

- **status**: 接続ごとに handshake + status request → JSON 応答 → 切断。
  新規接続レート（conn/s）を測定。
- **login**: 接続 → Login Start → forwarding/HMAC → compression →
  Login Success/ack → 切断。ログイン数/s。
- **relay**: ログイン後、64B または 1KB のプレイパケットを送り、
  バックエンドがエコーしたパケットを受け取る ping-pong。
  msg/s（1 msg = クライアント送信 + エコー受信）と RTT。

クライアントとバックエンドはカーネル側、F-Stack プロキシは net_tap 経由で
接続します。F-Stack プロキシのバックエンド接続も net_tap を通るため、
1 回の relay で **4 回の tap 横断**（client→proxy、proxy→backend、
backend→proxy、proxy→client）が発生します。

## 2. 1 コア固定の結果

プロキシを 1 コアに固定（`taskset -c 0`）、Tokio ワーカー 1、
F-Stack は `idle_sleep=50`（50 µs）。

| ワークロード | カーネル | F-Stack | 比 | カーネル p50 | F-Stack p50 |
|---|---:|---:|---:|---:|---:|
| status | 3,150 conn/s | 2,918 conn/s | 0.93x | 1.51 ms | 2.50 ms |
| login | 1,055 /s | 728 /s | 0.69x | 7.67 ms | 10.82 ms |
| relay 64B | 18,077 msg/s | 5,800 msg/s | 0.32x | 0.43 ms | 1.35 ms |
| relay 1KB | 6,848 msg/s | 3,736 msg/s | 0.55x | 1.13 ms | 2.10 ms |

- relay 64B の実効スループット: カーネル 2.21 MiB/s、F-Stack 0.71 MiB/s
- relay 1KB の実効スループット: カーネル 13.4 MiB/s、F-Stack 7.3 MiB/s
- プロキシ CPU: カーネル約 1.0 コア、F-Stack 0.6〜0.9 コア

## 3. 2 コア固定の結果

プロキシを 2 コアに固定（`taskset -c 0-1`）。カーネルは Tokio ワーカー 2、
F-Stack は `idle_sleep=0`（ビジーポーリング）。F-Stack のスタック
スレッドは lcore 0、アプリはもう一方のコアを使える構成です。

| ワークロード | カーネル | F-Stack | 比 |
|---|---:|---:|---:|
| status | 7,407 conn/s（1.87 コア） | 3,059 conn/s（0.97 コア） | 0.41x |
| login | 2,151 /s（1.86 コア） | 517 /s（0.89 コア） | 0.24x |
| relay 64B | 23,239 msg/s（1.30 コア） | 5,560 msg/s（1.04 コア） | 0.24x |
| relay 1KB | 7,611 msg/s（1.31 コア） | 2,724 msg/s（0.96 コア） | 0.36x |

カーネル版は 2 コアで status が約 2.35 倍にスケールしますが、
F-Stack 版は **1 コア時とほぼ同じ**です。F-Stack の全ソケット操作が
単一のスタックスレッドに直列化されるため、スタック側がボトルネックに
なっています（アプリ側にもう 1 コア渡しても改善しない）。

## 4. チューニングの効果

F-Stack の既定値は高スループット向けで、Minecraft のような小さな
パケットの往復には不利な設定がいくつかあります。以下を変更しました。

```ini
[dpdk]
pkt_tx_delay=0          ; 32 パケット未満送信時の 100us 遅延を無効化
idle_loop_interval=0    ; アイドル時の tick 間隔 100us を無効化
idle_sleep=0            ; ビジーポーリング

[freebsd.sysctl]
net.inet.tcp.delayed_ack=0
net.inet.tcp.sendspace=65536
net.inet.tcp.recvspace=65536
```

2 コア固定（スタック用 + アプリ用）での比較（同一条件、8 クライアント）:

| ワークロード | チューニング前 | チューニング後 |
|---|---:|---:|
| login | 517 /s | **938 /s** |
| relay 64B | 5,560 msg/s | 4,845 msg/s |
| relay 1KB | 2,724 msg/s | 2,753 msg/s |
| status | 3,059 conn/s | 2,379 conn/s |

- **login は約 1.8 倍**。バックエンド接続・forwarding・圧縮の往復で
  `pkt_tx_delay`（最大 100us）と delayed ACK の影響が大きかったためです。
- relay / status はスタックスレッドが 1 コアを使い切っており、
  設定変更では伸びません（数値は ±15% 程度のばらつきを含みます）。
- 大きなバッファ（64KB）は 64B/1KB の往復主体では効果がありませんでした。

## 5. マルチコア（マルチプロセス）

現行の F-Stack 統合は **1 プロセス 1 スタック**です。マルチコアは
「1 プロセス 1 コア」で複数プロセス起動するのが現実的です。
2 プロセス（lcore 0/1、別タップ・別サブネット・別 IP）で、
8 スレッドの status クライアントを 2 本同時に実行した結果:

| 構成 | status | 合計 | 備考 |
|---|---:|---:|---|
| 1 プロセス（1 コア） | 2,936 /s | 2,936 /s | p50 2.2 ms |
| 2 プロセス（2 コア） | 2,936 + 2,989 /s | **5,925 /s** | ほぼ 2 倍、エラー 0 |

- 各プロセスは独立した vnet / ソケットテーブル / スタックを持ち、
  別 IP・別タップ・別 `lcore_mask`（`1`, `2`）で動作します。
- スタックが 1 コアで飽和するため、**プロセス数を増やすほど
  ほぼ線形にスケール**します（クライアントとバックエンドのコアが
  別途必要）。
- ただし現状は **プロセス間でプレイヤー状態を共有していません**。
  status の人数は各プロセスで独立です。共有するには
  共有 hugepage 上の `rte_ring` / `ff_ipc` による制御メッセージ
  （[workers.md](workers.md) の `WorkerBus` のプロセス版）が必要です。
- 1 アドレスで受けるには `SO_REUSEPORT` と RSS の分散が必要ですが、
  net_tap では検証できていません。native-mt（`thread_mode=1`）で
  1 プロセス N スタックにするのが本来の形で、`ff_run` の per-lcore
  tick ディスパッチが今後の課題です。

## 6. 考察

- **status はほぼ互角（1 コア）**。接続あたりのプロキシ CPU（JSON 生成など）
  が支配的で、tap の往復コストが相対的に小さいためです。
- **login は F-Stack が 0.7 倍**。バックエンド接続、forwarding/HMAC、
  圧縮の往復が tap を余分に通過します。
- **relay は F-Stack が 0.3〜0.5 倍**。ping-pong 1 回ごとに
  4 回の tap 横断と、スタックスレッドへのキュー往復が入ります。
  1KB ではスループットが上がり、差が縮まります。
- **F-Stack は 1 コアで CPU を使い切らない**（0.6〜0.9 コア）。
  スタックとアプリの間のキュー往復・待ち合わせが律速で、
  コアを増やしてもスタックスレッドが直列のままでは伸びません。
- これは **net_tap と現行の「1 スタック + アプリ別スレッド」構成**の
  結果です。物理 NIC + DPDK（RSS / zero-copy、カーネルバイパス）や、
  native-mt で 1 コア 1 スレッドに統合した構成では、特に高負荷・
  多接続時の特性が変わります。**F-Stack 一般の性能を表すものではありません。**

### `idle_sleep` のトレードオフ

F-Stack のメインループは既定でビジーポーリングします。アプリを同じコアに
固定すると CPU を奪い合うため、`idle_sleep` を調整します。

| idle_sleep | proxy CPU | status | relay 64B | relay p50 |
|---|---:|---:|---:|---:|
| 0（busy poll） | 約 1.0 コア | 3,235/s | 5,691/s | 1.09 ms |
| 50 µs | 0.6〜0.9 コア | 2,918/s | 5,800/s | 1.35 ms |
| 1000 µs | 0.14〜0.38 コア | 1,184/s | 1,160/s | 7.06 ms |

1 ms の sleep は I/O ごとの待ち時間として効いてしまい、レイテンシ重視の
Minecraft プロキシには不向きです。**50 µs 前後**がバランス点で、
スタック専用コアを割り当てられるなら `idle_sleep=0` が最良です。

## 7. 再現方法

```bash
# ベンチマークツール
cargo build --release --example bench -p mc-proxy

# バックエンド（カーネル）
./target/release/examples/bench backend 0.0.0.0:25566 bench-secret &

# カーネル版プロキシ（1 コア固定）
TOKIO_WORKER_THREADS=1 taskset -c 0 /tmp/mc-proxy-std \
  --config /tmp/bench-std.toml &

./target/release/examples/bench status 127.0.0.1:25599 5 8
./target/release/examples/bench login  127.0.0.1:25599 5 8
./target/release/examples/bench relay  127.0.0.1:25599 5 8 64
./target/release/examples/bench relay  127.0.0.1:25599 5 8 1024
```

F-Stack 版は `scripts/run-fstack-net-tap.sh` と同じ手順で net_tap を設定し、
`/tmp/mc-proxy-fstack` を起動して `192.168.100.2:25599` に対して同じ
ベンチマークを実行します（`[servers]` のアドレスはカーネル側
`192.168.100.1:25566`）。

`idle_sleep` は `/etc/f-stack.conf`（またはコピー）の `[dpdk]` セクションで
変更します。

## 8. F-Stack native-mt（実験的）

`thread_mode=1`、`lcore_mask=0x3`、`lcore_list=0,1`、2 ワーカー、
net_tap `queues=2` で、1 lcore = 1 スレッドのインライン実行を検証:

- 2 ワーカーの起動と SO_REUSEPORT bind は成功（worker 0/1）。
- status を 4 スレッド以上で流すと **segfault**。
- login/relay は `backend timed out`（F-Stack 起点の接続応答が別 RX
  キューに配送されるため）。
- キュー方式で先に試した構成でも 4 スレッド 2,615〜2,778/s で
  単一スタック（2,918〜3,150/s）を超えず、8/16 スレッドで低下・エラー。

**net_tap は native-mt の検証・運用環境として不適切**です。物理 NIC +
DPDK RSS で、双方向フローが同一キューに届く構成が必要です。net_tap/
本番では 1 プロセス 1 コア + `SO_REUSEPORT` を推奨します。

## 9. 注意

- 8 vCPU の VM、CPU アイソレーションなし、クライアントとバックエンドが
  同一ホストという条件です。絶対値は環境依存です。
- F-Stack の優位性が見えるのは、物理 NIC + DPDK、複数 RSS キュー、
  native-mt、カーネルバイパスが揃った構成です。本結果は
  **現行実装を net_tap で動かした場合の相対比較**として扱ってください。
- 計測は各 5 秒、8 クライアントスレッド。エラーは 0 件（表の実行時）。
- マルチプロセスの計測は 2 プロセス・2 コア（クライアントは別コア）。
  4 プロセス以上でも同様にスケールする見込みですが、同一ホストの
  コア数・クライアント生成能力に依存します。
