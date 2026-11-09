# mc-proxy — F-Stack/DPDK 対応・Velocity 互換 Minecraft リバースプロキシ

DPDK 上のユーザー空間 TCP/IP スタック **F-Stack** で動く、Tokio 互換の
`TcpListener` / `TcpStream` / `UdpSocket` ライブラリ **`tokio-fstack`** と、
それを用いて実装した **Velocity 互換** の Minecraft Java Edition リバースプロキシ
**`mc-proxy`** のワークスペースです。

```
┌──────────────┐    ┌──────────────────────┐    ┌──────────────────────┐    ┌────────────────┐
│ Minecraft    │    │ mc-proxy (Tokio)     │    │ tokio-fstack         │    │ F-Stack / DPDK │
│ client       │◀──▶│ login / forwarding   │◀──▶│ reactor + sockets    │◀──▶│ ff_socket ...  │
│ (1.13–1.21+) │    │ status / relay       │    │ (readiness, wakers)  │    │ ff_epoll_*     │
└──────────────┘    └──────────────────────┘    └──────────────────────┘    └────────────────┘
                                                        │
                                                        ▼
                                                ┌──────────────────┐
                                                │ Paper backend    │
                                                │ velocity.enabled │
                                                └──────────────────┘
```

## 目次

1. [できること](#できること)
2. [リポジトリ構成](#リポジトリ構成)
3. [クイックスタート（カーネルソケット）](#クイックスタートカーネルソケット)
4. [Velocity モダンフォワーディング](#velocity-モダンフォワーディング)
5. [F-Stack / DPDK で動かす](#f-stack--dpdk-で動かす)
6. [`tokio-fstack` ライブラリ](#tokio-fstack-ライブラリ)
7. [ワーカーモデル（マルチコア）](#ワーカーモデルマルチコア)
8. [設定リファレンス](#設定リファレンス)
9. [テスト](#テスト)
10. [制限事項](#制限事項)
11. [参考資料](#参考資料)

## できること

- **Velocity 互換のモダンフォワーディング**
  - バックエンド（Paper）が送る `velocity:player_info` Login Plugin Request に
    HMAC-SHA256 署名付きで応答します。IP アドレス・UUID・ユーザー名・
    プロパティ（スキン等）を転送するため、バックエンド側の
    `velocity.enabled: true` + 共有シークレットだけで本物の UUID とスキンが使えます。
- **オンラインモード（Mojang 認証）**
  - RSA-1024 / AES-CFB8 の暗号化ログイン、`hasJoined` によるセッション検証、
    署名付き SHA-1 サーバーハッシュを実装しています。
  - `hasJoined` は `rustls` + `hyper`（HTTP/1.1）で、`tokio-fstack` の
    ソケットを `hyper_util::rt::TokioIo` でアダプトして送信します。
- **サーバー一覧（ステータス）**
  - MOTD（`&` カラーコード対応）、最大人数、オンライン人数、プレイヤーサンプル、
    ファビコン、レガシー ping（1.4〜1.6 形式）に対応。
  - バックエンドへ中継する `passthrough` モードも選べます。
- **ホスト名ルーティング**
  - `forced_hosts` によるバーチャルホスト振り分け、`try` による接続先フォールバック。
- **バージョン非依存のリレー**
  - 1.13（protocol 393）以降のクライアントを、プロトコルバージョンを変更せず
    そのままリレーします。1.20.2+ の Configuration ステートにも対応。
  - 圧縮しきい値はクライアント側とバックエンド側で独立に管理し、
    パケットごとに再エンコードします。
- **F-Stack とカーネルソケットの両対応**
  - 同じ非同期コード（リアクター、圧縮、暗号化）を両方のバックエンドで使用。
    F-Stack なしでもテスト・開発が可能です。

## リポジトリ構成

```
.
├── Cargo.toml                     # ワークスペース
├── README.md
├── docs/
│   ├── architecture.md            # リアクターとスレッドモデルの詳細
│   ├── fstack.md                  # F-Stack/DPDK デプロイ手順
│   ├── protocol.md                # プロトコルとフォワーディング仕様
│   └── workers.md                 # ワーカーモデル / native-mt / RingBuffer
├── crates/
│   ├── tokio-fstack/              # Tokio 互換ソケットライブラリ
│   │   ├── src/{lib,sys,ops,reactor,worker,poll_io,stream,listener,udp}.rs
│   │   ├── build.rs               # F-Stack リンク設定
│   │   ├── examples/echo_{server,client}.rs
│   │   └── tests/socket.rs        # リアクター/ソケットの統合テスト
│   └── mc-proxy/                  # Velocity 互換プロキシ
│       ├── config.example.toml
│       ├── src/
│       │   ├── protocol/          # VarInt, フレーミング, 圧縮, 暗号, パケット
│       │   ├── proxy/             # セッション, バックエンド, ステータス
│       │   ├── forwarding.rs      # Velocity モダンフォワーディング
│       │   ├── crypto.rs          # RSA/AES/オフライン UUID/サーバーハッシュ
│       │   ├── auth.rs            # hasJoined (HTTP/HTTPS over tokio-fstack)
│       │   ├── config.rs / servers.rs / sockets.rs
│       │   └── main.rs
│       └── tests/e2e.rs           # 偽 Paper バックエンド + 偽クライアント
└── scripts/
    └── paper-smoke-client.py      # 実 Paper に対するログイン確認クライアント
```

## クイックスタート（カーネルソケット）

```bash
# ビルド
cargo build --release

# 設定例を出力して編集
./target/release/mc-proxy --print-example > config.toml

# フォワーディングシークレットを生成
./target/release/mc-proxy gen-secret
# 例: fa0bfe460290ff64fbf2a41f1439539b92213c78af05adee7a3f388b30bf4dae

# 設定の検証
./target/release/mc-proxy --config config.toml --check

# 起動
./target/release/mc-proxy --config config.toml
# INFO mc-proxy started backend="std" listen=0.0.0.0:25565 online_mode=false forwarding=true ...
```

最小構成の `config.toml`:

```toml
bind = "0.0.0.0:25565"
motd = "&aHello &lMinecraft"
max_players = 100
forwarding_secret = "<gen-secret の出力>"

[[servers]]
name = "lobby"
address = "127.0.0.1:25566"
```

バックエンド（Paper 1.13+）の設定:

```yaml
# config/paper-global.yml
proxies:
  velocity:
    enabled: true
    online-mode: true          # プロキシで認証済みの UUID を信用する
    secret: "<config.toml と同じシークレット>"
```

```properties
# server.properties
online-mode=false
server-ip=127.0.0.1
```

バックエンドのポートは外部に公開せず、プロキシからの接続だけを許可してください。
プロキシは `forwarding_secret` が設定されている場合、
`velocity:player_info` を要求してこないバックエンドへのログインを拒否します
（Velocity の `modern` モードと同じ挙動です）。

## Velocity モダンフォワーディング

Paper はログイン中に Login Plugin Request（`velocity:player_info`）を送り、
プロキシは次のバイト列を返します（[Velocity の
`PlayerDataForwarding.java`](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/PlayerDataForwarding.java)
および [Paper の `VelocityProxy.java`](https://github.com/PaperMC/Paper/blob/main/paper-server/src/main/java/com/destroystokyo/paper/proxy/VelocityProxy.java)
準拠）。

```
HMAC-SHA256(secret, payload) 32 bytes
payload:
  VarInt   forwarding version (1)
  String   remote address ("203.0.113.7")
  UUID     16 bytes
  String   username
  VarInt   property count
  repeat:
    String property name
    String property value
    bool   has_signature
    String signature (has_signature のとき)
```

本実装は常にバージョン 1 を返します。Paper はバージョン 1〜4 を受け付けるため、
スキンや署名鍵を扱わないプロキシとして最も互換性の高い選択です。
`forwarding_secret` が空の場合はフォワーディングを行いません
（オフラインのバックエンド向け）。

ログインの流れ:

```
client                mc-proxy                         Paper backend
  │ Handshake(login)     │                                  │
  ├─────────────────────▶│ Handshake + Login Start          │
  │                      ├─────────────────────────────────▶│
  │                      │ Login Plugin Request             │
  │                      │◀─────────────────────────────────┤
  │                      │ Login Plugin Response (HMAC 付き) │
  │                      ├─────────────────────────────────▶│
  │                      │ Set Compression / Login Success  │
  │                      │◀─────────────────────────────────┤
  │ Set Compression      │                                  │
  │◀─────────────────────┤ Login Success                    │
  ├─────────────────────▶│ Login Acknowledged (1.20.2+)     │
  │ Login Acknowledged   │                                  │
  ├─────────────────────▶│ ── Configuration/Play をリレー ──▶│
```

オンラインモードでは、クライアントとの間で

1. Encryption Request（RSA 公開鍵 + verify token）
2. Encryption Response（共有シークレット + token を RSA で暗号化）
3. AES-128-CFB8 を両方向で有効化
4. `sha1("" + shared_secret + public_key)` を Minecraft の符号付き 16 進数に変換し、
   `https://sessionserver.mojang.com/session/minecraft/hasJoined` で検証
5. 得られたプロファイルをフォワーディングデータに使用

を行います。詳細は [docs/protocol.md](docs/protocol.md) を参照してください。

## F-Stack / DPDK で動かす

F-Stack は DPDK が NIC を専有した状態で、その上に FreeBSD 由来の TCP/IP
スタックを提供します。Rust 側は `ff_*` API を直接呼び出します。

### 1. F-Stack と DPDK のビルド

```bash
git clone https://github.com/F-Stack/f-stack.git
cd f-stack
make            # DPDK と FreeBSD スタックをビルド
sudo make install
```

### 2. hugepages と NIC のバインド

```bash
echo 1024 | sudo tee /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
sudo mkdir -p /mnt/huge
sudo mount -t hugetlbfs nodev /mnt/huge
# NIC を DPDK 用ドライバにバインド（vfio-pci など）
sudo modprobe vfio-pci
# 例: 0000:00:06.0 を vfio-pci に
sudo dpdk-devbind.py --bind=vfio-pci 0000:00:06.0
```

`/etc/f-stack.conf` にアドレスやゲートウェイを設定します
（詳細は [F-Stack の設定ドキュメント](https://f-stack.org/) を参照）。

### 3. プロキシを F-Stack バックエンドでビルド

```bash
FSTACK_LIB_DIR=/usr/local/lib \
PKG_CONFIG_PATH=/usr/local/lib/pkgconfig \
cargo build --release -p mc-proxy --features fstack
```

`build.rs` が `pkg-config --static --libs libdpdk` の結果と
`libfstack.a`（`--whole-archive`）をリンクします。
F-Stack を共有ライブラリとしてリンクする場合は `FSTACK_LIBS="fstack dpdk"`
のように明示できます。

F-Stack のバージョンは `ff_epoll_create` / `ff_epoll_ctl` / `ff_epoll_wait`
（`lib/ff_epoll.h`）を提供するものが必要です。

### 4. EAL 引数と起動

```toml
[fstack]
eal_args = ["--conf", "/etc/f-stack.conf", "--proc-type=primary", "--file-prefix=mc-proxy"]
```

```bash
sudo ./target/release/mc-proxy --config config.toml
```

F-Stack は hugepage と NIC へのアクセスが必要なため、root または
`CAP_SYS_RAWIO` / `CAP_IPC_LOCK` などの権限で起動してください。

**注意点**

- F-Stack バックエンドでは、プロキシの全ソケットが F-Stack 上に作られます。
  Mojang のセッションサーバーへも F-Stack のソケットで接続します。
  DPDK が NIC を専有している環境では、F-Stack 側にインターネットへの
  ルートが必要です（もしくは管理用 NIC を分ける、`online_mode = false` にする）。
- バックエンドのアドレスは IP リテラルを推奨します。名前解決はホストカーネルの
  DNS を使うため、DPDK 専有環境では解決できない場合があります。

F-Stack 固有のスレッドモデル（`ff_*` を呼べるのは `ff_run` を実行している
1 スレッドだけ）への対応は [docs/architecture.md](docs/architecture.md) を
参照してください。

## `tokio-fstack` ライブラリ

`tokio::net` のサブセットを同じ形で提供します。

| `tokio-fstack` | Tokio |
|---|---|
| `TcpListener` | `tokio::net::TcpListener` |
| `TcpStream` | `tokio::net::TcpStream` |
| `OwnedReadHalf` / `OwnedWriteHalf` | 同名 |
| `UdpSocket` | `tokio::net::UdpSocket` |

`AsyncRead` / `AsyncWrite` を実装しているため、`AsyncReadExt`、
`AsyncWriteExt`、`tokio::io::copy`、`tokio::select!`、`tokio::spawn` が
そのまま使えます。

```rust
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // F-Stack 利用時のみ意味を持つ EAL 引数（カーネル backend では無視）
    tokio_fstack::init(tokio_fstack::Config::default())?;

    let listener = tokio_fstack::TcpListener::bind("0.0.0.0:25565").await?;
    loop {
        let (mut stream, peer) = listener.accept().await?;
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let _ = stream.write_all(&buf[..n]).await;
            println!("{peer}");
        });
    }
}
```

UDP:

```rust
let socket = tokio_fstack::UdpSocket::bind("0.0.0.0:25566").await?;
let mut buf = [0u8; 1500];
let (n, peer) = socket.recv_from(&mut buf).await?;
socket.send_to(&buf[..n], peer).await?;
```

エコーサーバー/クライアントの完全な例:

```bash
cargo run --example echo_server
cargo run --example echo_client
```

### 設計の要点

- **単一のリアクター**が `epoll`（カーネル backend）または
  `ff_epoll_*`（F-Stack backend）を待ち、ソケットごとの `Waker` を起こします。
- **F-Stack backend では全 syscall を専用スタックスレッドで実行**します。
  F-Stack の API は `ff_init`/`ff_run` を呼んだスレッドでしか安全に使えないため、
  Tokio のワーカースレッドからは決して `ff_*` を呼びません。
- 読み書きは**呼び出し側のバッファ**に対して実行されますが、`OpFuture` の
  Drop がスタック側の syscall 完了を待つため、キャンセル時も use-after-free は
  発生しません（[docs/architecture.md](docs/architecture.md)）。
- ソケットの fd は `Drop` でリアクターから登録解除され、F-Stack 側では
  `ff_close` がスタックスレッド上で実行されます。

## ワーカーモデル（マルチコア）

```
CPU Core 0                                  CPU Core 1
┌──────────────────────────────────┐        ┌──────────────────────────────────┐
│ F-Stack event loop / reactor     │        │ F-Stack event loop / reactor     │
│  ├─ RX/TX queue 0                │        │  ├─ RX/TX queue 1                │
│  ├─ TCP connections              │        │  ├─ TCP connections              │
│  └─ readiness → Waker            │        │  └─ readiness → Waker            │
│ Tokio current_thread runtime     │        │ Tokio current_thread runtime     │
│  └─ proxy worker 0               │        │  └─ proxy worker 1               │
│        ▲ RingBuffer (SPSC/MPSC)  │◀──────▶│        ▲ RingBuffer              │
└──────────────────────────────────┘        └──────────────────────────────────┘
```

```toml
workers = 4        # 4 つの worker（それぞれ独立したリアクター + runtime）
pin_cores = true   # コアにピン留め（best effort）
```

- 各 worker は独立したリアクター（標準: epoll / F-Stack: スタック）と
  `current_thread` Tokio ランタイムを持ち、`SO_REUSEPORT` で同じアドレスを
  待ち受けます。
- 接続は worker に固定され、クライアント→バックエンドのリレーまで同じ
  worker 内で完結します。
- プレイヤーのログイン/ログアウトはロックフリーのリングバッファ
  （`WorkerBus`）で他の worker に複製され、どの worker が応答する ping でも
  クラスタ全体の人数・サンプルを返します。
- 標準バックエンドでは 1 プロセスで N worker、F-Stack では
  **1 プロセス = 1 スタック**のため 1 プロセス 1 worker（N プロセス +
  `SO_REUSEPORT`）でスケールします。F-Stack v2.0 の native-mt
  （`thread_mode=1`）への直接対応は今後の課題です。

詳細と設計上のトレードオフは [docs/workers.md](docs/workers.md) を参照して
ください。

## 設定リファレンス

`config.example.toml` も参照してください。

| キー | 既定値 | 説明 |
|---|---|---|
| `bind` | `"0.0.0.0:25565"` | 待ち受けアドレス |
| `motd` | `"A Minecraft Server"` | MOTD。`&` カラーコードと `\n` に対応 |
| `max_players` | `100` | サーバー一覧に表示する最大人数 |
| `version_name` | `mc-proxy <version>` | サーバー一覧のバージョン名 |
| `online_mode` | `false` | Mojang セッション認証を行う |
| `workers` | `1` | ワーカー数（標準バックエンドのみ 2 以上） |
| `pin_cores` | `true` | ワーカーをコアにピン留め（best effort） |
| `compression_threshold` | `256` | 圧縮しきい値。`-1` で無効 |
| `forwarding_secret` | `""` | Velocity モダンフォワーディングのシークレット |
| `forwarding_secret_file` | なし | シークレットをファイルから読む |
| `favicon` | なし | 64×64 PNG のパス |
| `servers` | 必須 | `{ name, address }` の配列。先頭が既定サーバー |
| `forced_hosts` | `{}` | 接続ホスト名 → サーバー名 |
| `try.servers` | `[]` | 接続時に順に試すサーバー名 |
| `status.mode` | `"proxy"` | `proxy` / `passthrough` |
| `timeouts.handshake_ms` | `10000` | ハンドシェイク/ステータスのタイムアウト |
| `timeouts.login_ms` | `30000` | クライアントログインのタイムアウト |
| `timeouts.backend_ms` | `30000` | バックエンド接続・ログインのタイムアウト |
| `auth.session_server` | `https://sessionserver.mojang.com` | `hasJoined` のベース URL |
| `auth.timeout_ms` | `5000` | 認証 HTTP のタイムアウト |
| `fstack.eal_args` | `[]` | `ff_init` に渡す引数 |

## テスト

```bash
cargo test --workspace          # ユニット + 統合 + E2E（計 68 テスト、ほかに通信必須の 1 件）
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo check -p mc-proxy --features fstack     # F-Stack バックエンドのコンパイル
```

E2E テスト（`crates/mc-proxy/tests/e2e.rs`）は、Paper と同じ検証を行う
偽バックエンド（`velocity:player_info` の要求、HMAC 検証、圧縮、
Login Success、エコー）と偽クライアントを立てて次を検証します。

- ステータス JSON / ping / レガシー ping
- オフラインモード + モダンフォワーディングの UUID・IP・名前
- 双方向リレーと 300 KB パケットの圧縮リレー
- 強制ホストルーティング
- オンラインモード（RSA / AES-CFB8 / モックセッションサーバー / サーバーハッシュ検証）
- 未対応プロトコルの拒否、フォワーディング未要求バックエンドの拒否
- 生バイト列からのハンドシェイク/ステータスデコード（回帰テスト）
- マルチワーカー（2〜3 worker）でのプレイヤー状態の複製と並列接続

`crates/tokio-fstack/tests/socket.rs` はリアクターの統合テストです
（TCP/UDP エコー、split、512 KB 転送、キャンセル安全性、32 同時接続など）。

`#[ignore]` 付きの 1 件は、実際の `sessionserver.mojang.com` に対して
TLS/HTTP クライアントを検証するものです（`--ignored` で実行）。

### 実 Paper での検証

`scripts/paper-smoke-client.py` を使って、**Paper 1.21.4 (build 232)** に対する
実際のログインを確認済みです。

```text
Paper 側の設定:
  server.properties: online-mode=false, server-port=25566
  config/paper-global.yml:
    proxies.velocity.enabled: true
    proxies.velocity.online-mode: true
    proxies.velocity.secret: "integration-test-secret"

proxy 側:
  forwarding_secret = "integration-test-secret"
  online_mode = false
  [[servers]] name = "paper" address = "127.0.0.1:25566"
```

結果（抜粋）:

```text
  <- [login] packet 0x03 (2 bytes)          # Set Compression (256)
  <- [login] packet 0x02 (27 bytes)         # Login Success
  <- [config] packet 0x07 (24504 bytes)     # Registry Data ...
  <- [config] packet 0x0d (26739 bytes)     # Update Tags
  <- [config] packet 0x03 (0 bytes)         # Finish Configuration
  <- [play] packet 0x2c (91 bytes)          # Login (Join Game)
  <- [play] packet 0x0b (2 bytes)
  <- [play] packet 0x3a (9 bytes)
REAL PAPER PLAY STATE REACHED THROUGH MC-PROXY
```

Paper のログにも `UUID of player PaperTest is 0223fb8d-...`（プロキシが転送した
オフライン UUID）が出力され、モダンフォワーディングの HMAC 検証・UUID 転送・
Configuration ステートのリレーが実サーバーで動作することが確認できました。

> プロキシはクライアントのプロトコルバージョンをそのままバックエンドへ転送し、
> Play/Configuration のパケットをそのまま中継します。クライアントとバックエンドの
> バージョンが一致している必要があります（Velocity と同じ制約です）。

## 制限事項

- プレイ中の `/server` コマンドによるサーバー切り替えは未実装です。
  接続先は `forced_hosts` / `try` で決まります。
- レガシー（BungeeCord 形式）フォワーディングは未対応です。
- クライアントとバックエンドの Minecraft バージョンは一致している必要があります
  （プロキシは Play/Configuration パケットを解釈せず中継します）。
- サーバー一覧の `version.protocol` はクライアントのプロトコルを返します。
  バックエンドがそのバージョンに対応しているかはプロキシからは判定できません。
- ログイン中の Cookie Request/Response の中継は行いません（1.20.5+ の
  トランスファー機能を使う場合のみ影響）。
- プレイヤー数の上限は `max_players` に強制されません（表示のみ）。
- レート制限・接続数制限・帯域制限はありません。公開環境では
  ファイアウォールや systemd の制限を併用してください。
- `workers > 1` は標準バックエンドのみ対応です。F-Stack では
  1 プロセス 1 worker で複数プロセス起動してください（native-mt 対応は
  [docs/workers.md](docs/workers.md) の今後の課題）。
- `workers > 1` は標準バックエンドのみ対応です。F-Stack では
  1 プロセス 1 worker で複数プロセス起動してください（native-mt 対応は
  [docs/workers.md](docs/workers.md) の今後の課題）。
- F-Stack バックエンドでは DNS 解決にホストカーネルのリゾルバを使います。

## 参考資料

- [Minecraft Java Edition protocol](https://minecraft.wiki/w/Java_Edition_protocol) —
  ハンドシェイク、ステータス、ログイン、圧縮の仕様
- [Velocity modern forwarding](https://docs.papermc.io/velocity/dev/player-information-forwarding/) —
  Paper 側の設定とフォワーディングモード
- [Velocity `PlayerDataForwarding.java`](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/PlayerDataForwarding.java) —
  ペイロードの正確なレイアウト
- [Paper `VelocityProxy.java`](https://github.com/PaperMC/Paper/blob/main/paper-server/src/main/java/com/destroystokyo/paper/proxy/VelocityProxy.java) —
  バックエンド側の HMAC 検証
- [F-Stack](https://f-stack.org/) / [F-Stack GitHub](https://github.com/F-Stack/f-stack) —
  `ff_api.h`、`ff_epoll.h`、スレッドモデル
