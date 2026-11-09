# ワーカーモデル（マルチコア / F-Stack native-mt）

## 1. コンセプト

```
CPU Core 0                                  CPU Core 1
┌──────────────────────────────────┐        ┌──────────────────────────────────┐
│ F-Stack event loop / reactor     │        │ F-Stack event loop / reactor     │
│  ├─ RX/TX queue 0                │        │  ├─ RX/TX queue 1                │
│  ├─ TCP connections (stack 0)    │        │  ├─ TCP connections (stack 1)    │
│  └─ readiness → Waker            │        │  └─ readiness → Waker            │
│                                  │        │                                  │
│ Tokio current_thread runtime     │        │ Tokio current_thread runtime     │
│  ├─ accept loop                  │        │  ├─ accept loop                  │
│  ├─ sessions / relay             │        │  ├─ sessions / relay             │
│  └─ bus consumer                 │        │  └─ bus consumer                 │
│        ▲                         │        │        ▲                         │
│        │  ring buffer (SPSC)     │◀──────▶│        │  ring buffer (SPSC)     │
└────────┴─────────────────────────┘        └────────┴─────────────────────────┘
```

`tokio-fstack` の **worker** は「1 コア = 1 リアクター + 1 Tokio ランタイム」の
単位です。

- `WorkerConfig { id, core, eal_args }` で起動（`tokio_fstack::spawn_worker`）。
- 各 worker は**独立したリアクター**を持ちます。
  - 標準バックエンド: 専用の epoll インスタンス + driver スレッド。
  - F-Stack バックエンド: 専用のスタックスレッド（`ff_init` + `ff_run`）。
- worker のスレッドでは `current_thread` ランタイムが動き、そのスレッドの
  スレッドローカルにリアクターが束縛されます。`TcpListener::bind` などの
  ソケット生成は**その worker のリアクター**に登録されます。
- worker スレッドは `core` にピン留めできます（best effort）。

### ソケットのアフィニティ

ソケットは**生成した worker に属します**。その worker のタスクから
`read`/`write` する限り、syscall はその worker のスタックで実行されます。
別 worker へソケットを移動すると、I/O が誤ったスタックに送られるため
サポートしません（Minecraft プロキシでは接続ごとに worker を固定します）。

## 2. 負荷分散

各 worker は同じ `bind` アドレスに対して **`SO_REUSEPORT`** でリスナーを
作成します（`TcpListener::bind_reuseport`）。

- カーネル / F-Stack（FreeBSD の `SO_REUSEPORT` 実装）が 4-tuple のハッシュで
  接続を各リスナーに分配します。
- 1 接続のライフサイクル（クライアント → バックエンド → リレー）は
  同じ worker 内で完結するため、データプレーンに worker 間コピーはありません。
- バックエンド接続も同じ worker のスタックから張るため、f-stack の
  fd テーブル（VNET）も worker 内で閉じます。

## 3. Worker 間通信（RingBuffer）

```rust
let (bus, receivers) = WorkerBus::<ClusterMessage>::new(workers, 4096);
let sender = bus.sender();
sender.broadcast_from(worker_id, ClusterMessage::Joined { .. });
```

- 実装は `crossbeam_queue::ArrayQueue`（ロックフリーの MPMC リングバッファ）。
  1 worker につき 1 つの受信リングを持ち、送信側は対象リングに push して
  `Waker` で受信タスクを起こします。
- 受信側は `WorkerBusReceiver::recv().await` で待ち受けます。
- リングが満杯の場合は `send` がメッセージを返します（呼び出し側で
  ドロップ/リトライを決定）。制御系メッセージのみを流す前提です。
- プロキシでは次のイベントを複製しています。
  - `ClusterMessage::Joined { worker, name, uuid }`
  - `ClusterMessage::Left { worker, uuid }`
- 各 worker は自分が受け持つプレイヤーをローカルに即時反映し、他 worker へ
  ブロードキャストします。受信側は worker ローカルのレプリカ
  （`Shared::players` / `online`）に適用するため、どの worker が応答する
  サーバーリスト ping でもクラスタ全体の人数・サンプルを返せます。
- UUID で重複排除するので、同一プレイヤーのイベントが二重に届いても
  カウントは増えません。

DPDK/F-Stack のマルチプロセス構成では、このリングを共有 hugepage 上の
`rte_ring`（`rte_ring_create` / `rte_ring_lookup` / `rte_ring_enqueue`）に
置き換えるのが自然です。現状の `WorkerBus` は同一プロセス内のワーカー間を
対象としています（[8. 制限](#8-制限と今後の課題)）。

## 4. F-Stack でのマッピング

### 4.1 native-mt（`thread_mode=1`, F-Stack v2.0）

F-Stack v2.0 の native-mt は、**1 プロセス・N lcore スレッド・N スタック
インスタンス**（VNET 分離）を提供します。

```ini
[dpdk]
lcore_mask=0xf        # 4 コア = 4 スレッド
thread_mode=1
```

`ff_init` は 1 回だけ呼び、`ff_run` が全 lcore で `main_loop` を起動します。
各 lcore スレッドは `ff_stack_thread_init(cpuid)` で自分のスタックを初期化し、
RX/TX キューと fd テーブルを持ちます。まさにこのドキュメントの図の形です。

**現状の `tokio-fstack` の F-Stack バックエンド**は、ワーカーごとに
`ff_init`/`ff_run` を呼ぶ 1 スタック構成のため、native-mt の N スタックを
そのままは利用できません。native-mt 対応には、`ff_run` の tick コールバックを
lcore ごとに識別し、lcore ごとのキュー・epoll・Waker をディスパッチする
拡張が必要です（[8. 制限](#8-制限と今後の課題)）。

### 4.2 マルチプロセス（`thread_mode=0`、現行 F-Stack）

現行の F-Stack でマルチコアにスケールする標準的な方法は、
**1 プロセス = 1 コア = 1 ワーカー**です。

- 各プロセスは自分の `--proc-type`（primary/secondary）と lcore を持ち、
  それぞれが `workers = 1` で起動します。
- すべてのプロセスが同じアドレスに `SO_REUSEPORT` でバインドします
  （`workers = 1` のプロキシも `SO_REUSEPORT` で bind します）。
- プロセス間のプレイヤー状態の共有は、共有 hugepage 上の `rte_ring` や
  F-Stack の IPC（`ff_ipc`）で行う設計になります（未実装）。

```bash
# 例: 4 プロセス（それぞれ別の config.ini / lcore）
for i in 0 1 2 3; do
  ./mc-proxy --config /etc/mc-proxy/worker-$i.toml &
done
```

## 5. 使い方

```toml
workers = 4       # 4 worker（それぞれ独立したリアクター + ランタイム）
pin_cores = true  # 先頭からコアにピン留め（best effort）
```

単一プロセスで `workers > 1` を使えるのは標準バックエンドのみです
（F-Stack バックエンドは 1 プロセス 1 スタックのため、`workers > 1` を
指定すると起動時にエラーになります）。

ライブラリとして直接使う場合:

```rust
use tokio_fstack::{spawn_worker, WorkerConfig};

let handle = spawn_worker(WorkerConfig::new(0).with_core(0), || async {
    let listener = tokio_fstack::TcpListener::bind("0.0.0.0:25565").await?;
    // この worker のリアクター上で accept / セッションを処理
    Ok::<(), std::io::Error>(())
})?;
handle.join()?;
```

## 6. 状態のレプリケーション

| データ | 保持場所 | 更新方法 |
|---|---|---|
| 設定・サーバー表・鍵・シークレット | `Arc<Immutable>`（全 worker 共有） | 起動時に不変 |
| メトリクス（接続数・転送量） | `Arc<Immutable>` 内の atomic | 直接更新（同一プロセス） |
| プレイヤー一覧・人数 | worker ローカル（`Shared`） | ローカル更新 + RingBuffer で複製 |
| オンライン UUID | 同上 | 同上 |
| レート制限・BAN 等（将来） | 共有 or 複製 | 制御メッセージでブロードキャスト |

同一プロセス内ではメトリクスのように atomic を共有できますが、
プレイヤー一覧は「worker ごとのレプリカ + リングバッファでのイベント複製」と
することで、マルチプロセス構成にもそのまま移行できる形にしています。

## 7. スレッド構成とトレードオフ

現状の 1 worker あたりのスレッド:

```
worker スレッド（runtime, core にピン留め）
  └─ Tokio current_thread runtime
        ├─ accept loop
        ├─ セッションタスク
        └─ bus consumer
reactor / stack スレッド（同じ core にピン留め可能）
  ├─ epoll_wait（標準） / ff_run（F-Stack）
  └─ ソケット syscall の実行（F-Stack の場合）
```

- 1 コアあたり 2 スレッド（ポーリング + ランタイム）です。F-Stack の
  DPDK ループはビジーポーリングなので、OS のスケジューラがランタイム側に
  CPU 時間を配分します。`idle_sleep` を設定するとアイドル時の干渉を
  減らせます。
- 完全な「1 コア 1 スレッド」にしたい場合は、F-Stack の tick 内で
  非同期タスクを直接ポーリングするカスタムエグゼキューター
  （または F-Stack の micro_thread 連携）が必要です。現状は
  Tokio 互換性を優先して 2 スレッド構成にしています。
- データプレーンは share-nothing です。worker 間でコピーされるのは
  ログイン/ログアウトなどの制御イベントだけです。
- 1 接続は 1 worker に固定されます（work stealing なし）。
  バックエンド接続も同じ worker から張るため、fd の所有権が単純になります。

## 8. 制限と今後の課題

- **F-Stack native-mt の直接対応**: 現在は 1 プロセス 1 スタック。
  `ff_run` の per-lcore tick ディスパッチを実装すると、1 プロセスで
  N スタックを扱えます。
- **マルチプロセス間の WorkerBus**: 現在のリングバッファは同一プロセス内。
  DPDK `rte_ring`（共有 hugepage）版を追加すると、マルチプロセス
  デプロイでもプレイヤー状態を共有できます。
- **共有メモリの状態**: マルチプロセスでは `Arc` を共有できないため、
  設定・メトリクス・プレイヤー一覧の共有方法をプロセス間 IPC に置き換える
  必要があります。
- **ワーカー間の作業移譲**: セッションのマイグレーションや `/server`
  切り替えを実装する場合は、接続の所有権移動プロトコルが必要です。

## 9. テスト

- `tokio-fstack`:
  - `worker_runs_sockets_and_tasks` — worker 上での bind/accept/read/write
  - `bus_delivers_and_wakes` — リングバッファと Waker
  - `bus_across_two_workers_delivers_everything` — worker ランタイム越しの配送
  - `bus_broadcast_reaches_every_worker` / `full_inbox_returns_the_message`
- `mc-proxy`（`tests/e2e.rs`）:
  - `multi_worker_replicates_player_state` — 2 worker で join/leave が
    両方のビューに複製され、status が全体人数を返す
  - `multi_worker_serves_parallel_connections` — 3 worker / 9 並列ログイン、
    全 worker が 9 人を観測し、切断後に 0 に戻る
