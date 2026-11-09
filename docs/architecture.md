# アーキテクチャ

## 1. 全体像

```
                         tokio worker threads                 reactor thread
┌──────────────┐   submit Op   ┌──────────────┐   execute    ┌──────────────────┐
│ async task   │ ────────────▶ │ Platform     │ ───────────▶ │ epoll_wait       │
│ poll_read()  │               │ (queue or    │              │ ff_epoll_wait    │
│              │ ◀──────────── │  inline)     │ ◀─────────── │ dispatch Wake    │
└──────────────┘   Waker wake  └──────────────┘  readiness   └──────────────────┘
```

`tokio-fstack` は Tokio のプライベート API に依存しません。`Waker` と
`AsyncRead`/`AsyncWrite` だけを使うため、任意の Tokio ランタイム
（current_thread / multi_thread）で動作します。

### モジュール

| ファイル | 役割 |
|---|---|
| `sys.rs` | 非ブロッキング syscall の薄いラッパー。標準 backend は libc、F-Stack backend は `ff_*` |
| `ops.rs` | `Op`（syscall 記述）と `OpFuture`（完了スロット）、キャンセル安全プロトコル |
| `reactor.rs` | `Platform`、`ScheduledIo`、`ReadinessFuture`、ドライバスレッド |
| `poll_io.rs` | `EAGAIN` → readiness → 再実行 の共通ループ |
| `stream.rs` / `listener.rs` / `udp.rs` | 公開 API |

## 2. 2 つのバックエンド

### 標準バックエンド（既定）

- `epoll` を待つ専用スレッド（`tokio-fstack-reactor`）を 1 本起動する。
- syscall（`read`/`write`/`accept`/`connect` など）は**呼び出し元の
  ワーカースレッドで直接実行**する。ソケットはすべて非ブロッキング。
- readiness の登録・解除は `epoll_ctl` を直接呼ぶ（スレッドセーフ）。

### F-Stack バックエンド（`--features fstack`）

F-Stack の API は「`ff_init` と `ff_run` を呼んだスレッド」でしか安全に
使えません（FreeBSD 由来の `curthread`/per-CPU 状態に依存するため）。
Tokio のワーカースレッドから `ff_*` を呼ぶと、スタックの状態が壊れるか
フェイルファストします。

さらに `ff_init`/`ff_run` は**プロセスのメインスレッド**で実行する必要が
あります（`ff_freebsd_init` が per-thread の FreeBSD 状態を初期化し、
リンカーセットの SYSINIT を実行するため）。

そこで F-Stack backend では、**メインスレッド**が

1. `ff_init(argv)` を実行し、
2. `ff_run(tick, ctx)` に入り、
3. `tick` の中で「キューに溜まった全 syscall」と「全 epoll 操作」を実行し、
4. `ff_epoll_wait(epfd, events, 512, 0)` で readiness を取得して `Waker` を起こす

という構造にしています。アプリケーション（Tokio ランタイムとプロキシ）は
別スレッドで動き、`Op` をキューに積んで完了を待つだけです。
`tokio_fstack::run_with_stack` がこのセットアップをカプセル化しています。

```
app thread ── push(Op) ──▶ queue ──▶ ff_run tick (main) ──▶ ff_recv/ff_send/...
     ▲                                      │
     └──────────── Waker ◀──────────────────┘
```

トレードオフは **I/O 1 回につき 1 回のクロススレッド・ハンドオフ**です。
Minecraft プロキシのパケット処理コストに比べれば十分小さく、
「F-Stack のスレッド制約を型と設計で守る」ことを優先しています。

> F-Stack の `ff_epoll_wait` は `ff_kevent` を非ブロッキングで呼ぶ実装になっており、
> タイムアウト引数は無視されます（`ff_kevent_do_each` が `timespec{0,0}` を
> 固定で使う）。したがって `ff_run` のループを止めずに readiness をポーリングできます。

## 3. readiness の扱い

- epoll は**レベルトリガー**で登録する（`EPOLLIN` / `EPOLLOUT`）。
- イベントを 1 方向に配送したら、その方向の interest を解除する
  （`EPOLL_CTL_MOD`）。待っていないソケットでイベントが鳴り続けるのを防ぐ。
- タスクは「まず syscall を投げ、`EAGAIN` なら readiness を待つ」方式。
  - データが既にある場合は余計な epoll 往復をしない。
  - `EAGAIN` 後は waker を登録してから interest を arm し、再度 readiness を
    確認する（lost wakeup を防ぐ）。
- `ReadinessFuture` は direction ごとのビットを `fetch_and` で消費する。
  消費しないと「`EAGAIN` → readiness 即 Ready → `EAGAIN`」のビジーループになる。

## 4. バッファ所有権とキャンセル

読み書きは呼び出し側のバッファに対して直接 syscall するため、
`poll_read`/`poll_write` が `Pending` を返した後にバッファが解放されると
use-after-free になります。これを防ぐため、

1. `Slot`（完了スロット）は `Mutex<SlotState>` を持ち、
2. syscall は **その Mutex を保持したまま** 実行され、
3. `OpFuture::drop` は同じ Mutex を取得してからキャンセルフラグを立てる。

したがって「in-flight の future を drop する」ことは「syscall の完了を待つ」
ことと等価で、バッファの寿命は常に syscall より長くなります。
`TcpStream`/`UdpSocket` はさらに内部バッファを持つため、
`poll_read` の呼び出し側バッファが `Pending` を跨いで保持されることは
ありません（`AsyncRead` の契約を満たす）。

## 5. fd のライフサイクル

- `Io::new(fd)` でリアクターに登録（`EPOLL_CTL_ADD`、interest 0）。
- `Io::drop` で
  1. レジストリからエントリを削除、
  2. `state.closed = true` を interest ロック下で設定、
  3. `EPOLL_CTL_DEL`、
  4. `close`（F-Stack では `ff_close`）をスタックスレッドにキューイング。

`closed` フラグと interest ロックにより、drop と同時に走っている
`deliver()` が閉じた fd（再利用された別ソケット）に対して
`epoll_ctl` する競合を防ぎます。

## 6. Tokio との互換性

- `TcpStream` は `AsyncRead + AsyncWrite + Unpin + Send + Sync`。
- `into_split()` は `Arc<StreamInner>` を共有する owned half を返す。
- `TcpListener::accept()` はキャンセル安全（`poll_accept` は同じ状態を
  再開する）。`UdpSocket::poll_recv_from` もキャンセル安全。
- `poll_send_to`/`poll_write` は「開始済みの送信」を完了させるため、
  キャンセル後に別バッファで再呼び出しすると、先行したデータが送られる
  ことがあります（Tokio の `poll_send_to` もキャンセル安全を保証しない）。
- `TcpListener::incoming()` は `tokio-stream` 非依存の `async fn next`。

## 7. ワーカー

`tokio-fstack` は「1 コア = 1 リアクター + 1 Tokio ランタイム」の worker を
提供します。各 worker はスレッドローカルに自分のリアクターを束縛し、
`SO_REUSEPORT` で接続を分配します。worker 間の制御イベントはロックフリーの
リングバッファ（`WorkerBus`）で複製されます。F-Stack の native-mt
（`thread_mode=1`）との対応、マルチプロセス構成、トレードオフは
[workers.md](workers.md) を参照してください。

## 8. 既知の設計上の制約

- ソケット操作ごとに `Arc<Slot>` を 1 つ確保する（std backend）。
  ホットパス最適化の余地として、スロットのプール化や
  `readiness` + 直接 syscall の fast path がある。
- プロキシのリレーは方向ごとに「1 パケット読む → 1 パケット書く」を
  繰り返す。TCP のバックプレッシャーで両方向が同時に詰まる可能性は
  理論上あるが、Minecraft のパケットサイズでは実用上問題にならない。
- プロキシはプレイ状態のパケットを解釈しない（同一バージョン同士の
  リレーであることを利用）。サーバー切り替えを実装する場合は、
  バージョンごとの KeepAlive / Respawn / Join Game の解析が必要になる。
