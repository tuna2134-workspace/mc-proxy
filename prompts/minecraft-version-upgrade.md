# プロンプト: Minecraft 新バージョンへの追従

以下をそのまま AI コーディングエージェントに渡してください。
`<NEW_VERSION>` / `<NEW_PROTOCOL>` / `<PAPER_VERSION>` を対象バージョンに置き換えます。

---

## 役割

あなたは Rust ワークスペース `mc-proxy` のメンテナです。

- `crates/tokio-fstack`: F-Stack/DPDK とカーネルソケット両対応の、
  Tokio 互換 `TcpListener` / `TcpStream` / `UdpSocket` ライブラリ
- `crates/mc-proxy`: Velocity 互換の Minecraft Java Edition リバースプロキシ

## ミッション

Minecraft Java Edition **`<NEW_VERSION>`（protocol `<NEW_PROTOCOL>`）** に
プロキシが追従できるようにする。既存の 1.13+ サポートを壊さず、
**実サーバー（Paper `<PAPER_VERSION>`）でのログイン成功まで検証**する。

## 重要な前提

- プロキシは **Play / Configuration ステートのパケットを解釈せず、そのまま
  リレー**する（クライアントとバックエンドは同一バージョン）。
  したがって、バージョンアップの多くは **Handshake / Status / Login /
  Forwarding** の確認だけで済む。
- バージョン固有の処理は以下に限定されている:
  - `Handshake::decode` / `is_supported_protocol`
  - `LoginStart::decode`（1.20.2+ の UUID、それ以前の追加フィールド）
  - ログイン状態のパケット ID（1.13 以降ほぼ安定）
  - Login Success（中継するだけでパースしない）
  - 1.20.2+ の Login Acknowledged
  - Velocity modern forwarding（ペイロードは常に version 1）
  - 圧縮・フレーミング（1.13 以降不変）
- ソケットは **worker アフィン**。別 worker へ移動させない。
- F-Stack バックエンドは **1 プロセス 1 スタック**。`workers > 1` は
  標準バックエンドのみ。
- 既存の公開 API・設定の後方互換を保つ。

## 最初に確認すること（推測で実装しない）

1. **プロトコル番号**
   - <https://minecraft.wiki/w/Java_Edition_protocol/Protocol_version_numbers>
     で `<NEW_VERSION>` の protocol を確認。
2. **プロトコル差分**
   - <https://minecraft.wiki/w/Java_Edition_protocol>
     で Handshake / Status / Login / Configuration のパケット ID と
     フィールドを確認。特に:
     - Login Start に新しいフィールドが増えていないか
     - Login Success / Set Compression / Login Plugin Request/Response の ID
     - Encryption Request/Response のレイアウト
     - Login Acknowledged / Cookie Response の ID
3. **Velocity フォワーディングの変更**
   - <https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/PlayerDataForwarding.java>
   - <https://github.com/PaperMC/Paper/blob/main/paper-server/src/main/java/com/destroystokyo/paper/proxy/VelocityProxy.java>
   - ペイロードのフィールド順、`velocity:player_info` のバージョン、
     HMAC の対象範囲が変わっていないか。
   - Paper が version 1 を受け付け続けるか（新しい必須バージョンが
     ないか）を確認。
4. **F-Stack 側**
   - 今回の変更がソケット API に影響しないか（通常は不要）。
5. ベースラインのテストを実行して現状を記録する:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo check -p mc-proxy --features fstack
```

## 変更対象ファイル

| ファイル | 確認・変更内容 |
|---|---|
| `crates/mc-proxy/src/protocol/handshake.rs` | `MIN_PROTOCOL`（393）/ `MAX_PROTOCOL`（1000）、`is_supported_protocol`。通常は `MAX_PROTOCOL` を超えない限り変更不要 |
| `crates/mc-proxy/src/protocol/login.rs` | ログイン状態のパケット ID 定数、`LoginStart::decode` のバージョン分岐、`encode_login_start` の UUID 条件 |
| `crates/mc-proxy/src/proxy/session.rs` | ログインシーケンス、1.20.2+ の Login Acknowledged 待ち、暗号化ハンドシェイク、エラーメッセージ |
| `crates/mc-proxy/src/proxy/backend.rs` | バックエンドの Handshake / Login Start / Plugin Request 応答 / Login Acknowledged |
| `crates/mc-proxy/src/forwarding.rs` | フォワーディングペイロードのバージョンとフィールド |
| `crates/mc-proxy/src/crypto.rs` | サーバーハッシュ、オフライン UUID（通常は不変） |
| `crates/mc-proxy/src/protocol/frame.rs` | フレーミング・圧縮（通常は不変） |
| `crates/mc-proxy/src/protocol/status.rs` | Status JSON / legacy ping |
| `crates/mc-proxy/tests/e2e.rs` | `PROTOCOL` 定数（現在 770）、新バージョンのログイン/リレー/圧縮テスト |
| `scripts/paper-smoke-client.py` | `PROTOCOL`（現在 769）と Configuration ステートのパケット ID |
| `docs/protocol.md` | バージョン表・ログインシーケンス・フォワーディング仕様 |
| `README.md` | 対応バージョン、テスト数、検証結果 |
| `docs/workers.md` | 影響がある場合のみ |

## 手順

### 1. 差分の分類

まず「どの種類の変更か」を判定し、該当する対応だけを行う。

| 変更の種類 | 対応 |
|---|---|
| protocol 番号だけ | テストの `PROTOCOL`、ドキュメント、Paper 実機確認 |
| Login Start にフィールド追加 | `LoginStart::decode` を更新し、ユニットテスト追加 |
| ログイン状態の ID 変更 | `protocol/login.rs` の定数と E2E の ID を更新 |
| Login Success のレイアウト変更 | **パースしない**ので原則不要（中継のみ） |
| Configuration ステートの ID 変更 | `LOGIN_ACKNOWLEDGED`（0x03）等を更新。リレーは不変 |
| 暗号化ハンドシェイク変更 | `authenticate` / `crypto.rs` を更新し、オンライン E2E を追加 |
| フォワーディング仕様変更 | `forwarding.rs` と `backend.rs` を更新し、Paper 実機で検証 |
| 圧縮/フレーミング変更 | `frame.rs` と単体テストを更新（Minecraft では稀） |
| Status JSON 変更 | `status.rs` / `proxy/status.rs` を更新 |

### 2. 実装

- バージョン固有の分岐は**最小限**にする。Play/Configuration の
  パケット ID をプロキシに持ち込まない。
- 定数は意味のある名前で一箇所にまとめる。
- 既存の 1.13〜現行バージョンのテストを壊さない。
- オフラインモードではクライアントが送る UUID を信用せず、
  `offline_uuid(name)` を使う（現行仕様を維持）。
- オンラインモードのサーバーハッシュ検証・HMAC 検証を弱めない。

### 3. テストの更新

- `crates/mc-proxy/tests/e2e.rs` の `PROTOCOL` を新バージョンに更新。
- バージョン依存のログイン形式（UUID の有無など）のテストを追加。
- 以下の既存テストが通ることを確認:
  - `offline_login_forwards_identity_and_relays`
  - `compressed_large_packet_relays`
  - `online_mode_login_uses_encryption_and_session_server`
  - `multi_worker_replicates_player_state`
  - `raw_handshake_bytes_are_decoded`
- `scripts/paper-smoke-client.py` は実 Paper のバージョンに合わせて
  `PROTOCOL` と Configuration の ID を更新する。

### 4. 実 Paper での検証

```bash
# Java 21（Adoptium JRE）
curl -L -o jre.tar.gz \
  "https://api.adoptium.net/v3/binary/latest/21/ga/linux/x64/jre/hotspot/normal/eclipse"
tar xzf jre.tar.gz
export PATH="$PWD/jre/bin:$PATH"
java -version

# Paper の最新ビルド（fill API）
URL=$(curl -s "https://fill.papermc.io/v3/projects/paper/versions/<PAPER_VERSION>/builds" \
  | python3 -c 'import json,sys; b=max(json.load(sys.stdin), key=lambda x:x["id"]); print(b["downloads"]["server:default"]["url"])')
curl -L -o paper.jar "$URL"
java -jar paper.jar --nogui   # 初回は eula.txt を生成して終了
echo "eula=true" > eula.txt

# server.properties
online-mode=false
server-port=25566
server-ip=127.0.0.1
level-type=minecraft\:flat
view-distance=3
simulation-distance=3

# config/paper-global.yml
proxies:
  velocity:
    enabled: true
    online-mode: true
    secret: "integration-test-secret"
```

プロキシ側:

```toml
bind = "127.0.0.1:25599"
forwarding_secret = "integration-test-secret"
online_mode = false

[[servers]]
name = "paper"
address = "127.0.0.1:25566"
```

```bash
./target/release/mc-proxy --config proxy.toml
python3 scripts/paper-smoke-client.py 127.0.0.1 25599
# => REAL PAPER PLAY STATE REACHED THROUGH MC-PROXY
```

Paper のログに `UUID of player ...` が出て、Configuration →
Finish Configuration → Play まで到達すれば成功。
失敗時は Paper のログとプロキシの `RUST_LOG=debug` を確認し、
`Login Disconnect` の理由を特定する。

### 5. ドキュメント更新

- `docs/protocol.md` のバージョン表とログイン手順。
- `README.md` の対応バージョン、テスト数、実 Paper 検証の節。
- 必要なら `docs/workers.md`（通常は影響なし）。

### 6. 最終検証（必須）

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check -p mc-proxy --features fstack --all-targets
cargo build --release -p mc-proxy
```

## 完了条件

- 新バージョンのクライアント（または同等のテストクライアント）が
  ステータス ping・ログイン・Configuration・Play まで到達する。
- Velocity モダンフォワーディングが Paper に受理され、
  転送 UUID が Paper のログに一致する。
- 既存バージョンのテストがすべて通る。
- `cargo clippy -D warnings` / `cargo fmt --check` / `--features fstack`
  のコンパイルがクリーン。
- README / docs が新バージョンに更新されている。

## 禁止事項

- Play / Configuration のパケットをバージョン別にパースする実装を
  プロキシに追加しない（リレーを維持する）。
- クライアントが送る UUID をオフラインモードで信用しない。
- フォワーディングの HMAC 検証を省略・弱体化しない。
- 1 つのバージョンにハードコードして他バージョンを落とさない。
- worker アフィニティ（ソケットを別 worker へ移動しない）を壊さない。
- F-Stack バックエンドのコンパイルを壊さない。

## 出力フォーマット

1. 判定した差分の種類と根拠（公式ドキュメント/ソースの該当箇所）
2. 変更したファイルと理由
3. テスト結果（コマンドと要約）
4. 実 Paper 検証の結果（できればログ抜粋）
5. 残課題・注意点
