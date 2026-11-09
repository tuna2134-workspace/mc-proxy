# プロトコル実装メモ

対象: Minecraft Java Edition 1.13（protocol 393）以降。
プロキシはクライアントのプロトコルバージョンを変更せずにバックエンドへ
転送し、プレイ/Configuration ステートのパケットは不透明にリレーします。
そのため、future のバージョンでもログイン ID が変わらない限り動作します。

## 1. フレーミング

```
非圧縮: [VarInt length][VarInt packet id][payload]
圧縮:   [VarInt length][VarInt uncompressed_length][zlib(packet id + payload)]
        uncompressed_length == 0 のときは無圧縮のまま
```

- `length` は「それ以降のバイト数」。
- 圧縮しきい値以上（`>=`）のパケットだけを圧縮する。しきい値 0 は全圧縮。
- 受信時は `uncompressed_length` を上限として確保し、展開後の長さが
  一致しない場合はプロトコルエラーとする。
- フレーム長の上限は 8 MiB、展開後も 8 MiB。

暗号化が有効な場合、AES-128-CFB8 は**フレーム全体**（長さ VarInt を含む）
に適用されます。実装は `protocol/cipher.rs` の `Cfb8` で、
`cfb8` クレートのテストベクタと一致することをユニットテストで確認しています。

## 2. ログインシーケンス

### 2.1 クライアント → プロキシ

| id | パケット | 備考 |
|---|---|---|
| 0x00 | Login Start | name (+ 1.20.2+ は UUID) |
| 0x01 | Encryption Response | オンラインモードのみ |
| 0x02 | Login Plugin Response | 今回は未使用（無視） |
| 0x03 | Login Acknowledged | 1.20.2+ |

### 2.2 プロキシ → クライアント

| id | パケット | 備考 |
|---|---|---|
| 0x00 | Login Disconnect | JSON チャット |
| 0x01 | Encryption Request | オンラインモードのみ |
| 0x02 | Login Success | バックエンドのパケットをそのまま中継 |
| 0x03 | Set Compression | 設定値（`compression_threshold`） |
| 0x04 | Login Plugin Request | バックエンドから転送（Cookie Request 等） |

### 2.3 プロキシ → バックエンド

1. Handshake（protocol version、クライアントが指定した仮想ホスト、next state = 2）
2. Login Start（認証済みの名前 + UUID。1.20.2+ は UUID を含む）
3. バックエンドが Set Compression を送れば、そのしきい値で両方向を切り替え
4. `velocity:player_info` の Login Plugin Request に HMAC 付きで応答
5. Login Success を受信したら、1.20.2+ ではこちらから Login Acknowledged を送信
6. クライアントの Login Acknowledged を待ってからリレー開始

> バックエンドがオンラインモード（Encryption Request を送ってくる）の場合は
> プロキシでは中継できないため、明示的なエラーにしています。

## 3. Velocity モダンフォワーディング

`forwarding_secret` が設定されている場合、プロキシは
`velocity:player_info` を要求してこないバックエンドへのログインを拒否します
（Velocity の `modern` モードと同じ）。

応答ペイロード（バージョン 1）:

```
signature = HMAC-SHA256(secret, payload)
payload =
  VarInt   1
  String   peer IP（例 "203.0.113.7"）
  UUID     16 bytes
  String   username
  VarInt   property count
  for each:
    String name
    String value
    bool   has_signature
    String signature
```

Paper 側は `VelocityProxy.checkIntegrity` で先頭 32 バイトの HMAC を検証し、
その後 `readVarInt`（バージョン）→ `readAddress` → `readUUID` →
`readUtf(16)` → `readProperties` の順で読みます。本実装はこの順序に一致します。

## 4. オンラインモード

1. プロキシは起動時に 1024-bit RSA 鍵を生成（`ServerKey`）。
2. Encryption Request:
   - server id: `""`
   - public key: X.509 SubjectPublicKeyInfo DER
   - verify token: 4 バイトの乱数
3. Encryption Response を受信し、RSA/PKCS#1 v1.5 で共有シークレットと
   verify token を復号。共有シークレットは 16 バイトでなければならない。
4. AES-128-CFB8 を両方向で有効化（鍵 = IV = 共有シークレット）。
5. サーバーハッシュ:
   `sha1(server_id + shared_secret + public_key_der)` を
   Java の `BigInteger.toString(16)` と同じ符号付き 16 進文字列に変換。
6. `GET {session_server}/session/minecraft/hasJoined?username=..&serverId=..&ip=..`
   を HTTPS（`rustls` + `webpki-roots`）で送信。HTTP/1.1 は `hyper` を使い、
   `hyper_util::rt::TokioIo` で `tokio-fstack` のソケットをそのまま
   アダプトするため、F-Stack 環境でも動作します。
7. 200 + JSON ならプロファイル（UUID、正規名、プロパティ）を採用。
   204/404 は認証失敗として切断。

サーバーハッシュの実装は Java の

```java
String hash = new BigInteger(digest).toString(16);
```

と一致することを、既知の入力に対する期待値テストで確認しています。

## 5. ステータス

- Status Response の `version.protocol` にはクライアントのプロトコル
  バージョンをそのまま返します（常に互換として表示される）。
- MOTD の `&` コードは `§` に変換し、`{"text": ...}` として返します。
- プレイヤーサンプルは接続中のプレイヤー（最大 12 名）。
- レガシー ping（`0xFE`）は 1.6 形式（`§1\0proto\0version\0motd\0online\0max`）
  と 1.4/1.5 形式（`motd§online§max`）を UTF-16BE で返します。
- `status.mode = "passthrough"` では、既定バックエンドへステータス要求と
  ping/pong を中継します（失敗時はローカル応答にフォールバック）。

## 6. バージョンごとの注意

| バージョン | protocol | 備考 |
|---|---|---|
| 1.13 | 393 | Login Plugin Message 導入。モダンフォワーディングの最低ライン |
| 1.16 | 735 | Login Success の UUID が文字列から 16 バイトに変更（プロキシは中継するだけ） |
| 1.19 | 759 | Login Start にプロファイル鍵データ（無視） |
| 1.19.3 | 761 | プロファイル鍵データ廃止 |
| 1.20.2 | 764 | Login Start に UUID、Login Acknowledged、Configuration ステート |
| 1.20.5 | 766 | Cookie Request/Response（ログイン中は未対応） |
| 1.21.x | 767–770+ | Play パケットは不透明リレー |

プレイ/Configuration のパケット ID はバージョンごとに異なりますが、
クライアントとバックエンドが同じバージョンであるため、プロキシは ID を
解釈せずにそのまま転送できます。
