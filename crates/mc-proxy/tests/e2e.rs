//! End-to-end tests: a real proxy process-internal, speaking the Minecraft
//! protocol to a fake Velocity-aware backend and a fake client.
//!
//! The fake backend performs the same checks Paper does when Velocity modern
//! forwarding is enabled: it requests `velocity:player_info`, verifies the
//! HMAC-SHA256 and reads the forwarded identity.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mc_proxy::config::Config;
use mc_proxy::crypto::minecraft_server_hash;
use mc_proxy::forwarding::{verify_forwarding_data, ForwardingData};
use mc_proxy::protocol::codec::{Packet, Writer as CodecWriter};
use mc_proxy::protocol::frame::{CipherKeys, FrameReader, FrameWriter};
use mc_proxy::protocol::handshake::{Handshake, NextState};
use mc_proxy::protocol::login::{
    self, decode_properties, encode_login_success, encode_plugin_request, encode_plugin_response,
    encode_set_compression, PluginResponse,
};
use mc_proxy::protocol::status;
use mc_proxy::proxy::Proxy;
use mc_proxy::sockets::{OwnedReadHalf, OwnedWriteHalf, TcpListener, TcpStream};
use rand::rngs::OsRng;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;
use uuid::Uuid;

const PROTOCOL: i32 = 770;
const SECRET: &str = "unit-test-forwarding-secret";

type Reader = FrameReader<OwnedReadHalf>;
type Writer = FrameWriter<OwnedWriteHalf>;

fn io_timeout() -> Duration {
    Duration::from_secs(10)
}

async fn read_packet<R: tokio::io::AsyncRead + Unpin>(reader: &mut FrameReader<R>) -> Packet {
    timeout(io_timeout(), reader.read_packet())
        .await
        .expect("timed out waiting for a packet")
        .expect("failed to read a packet")
}

async fn write_packet<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    id: i32,
    data: &[u8],
) {
    timeout(io_timeout(), writer.write_packet(id, data))
        .await
        .expect("timed out writing a packet")
        .expect("failed to write a packet");
}

/// A fake Velocity-aware backend.
struct FakeBackend {
    addr: SocketAddr,
    forwarded: Arc<Mutex<Vec<ForwardingData>>>,
    logins: Arc<AtomicUsize>,
}

async fn spawn_backend(compression: i32) -> FakeBackend {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let forwarded = Arc::new(Mutex::new(Vec::new()));
    let logins = Arc::new(AtomicUsize::new(0));
    let forwarded_task = Arc::clone(&forwarded);
    let logins_task = Arc::clone(&logins);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let forwarded = Arc::clone(&forwarded_task);
            let logins = Arc::clone(&logins_task);
            tokio::spawn(async move {
                let _ = backend_session(stream, compression, forwarded, logins).await;
            });
        }
    });
    FakeBackend {
        addr,
        forwarded,
        logins,
    }
}

async fn backend_session(
    stream: TcpStream,
    compression: i32,
    forwarded: Arc<Mutex<Vec<ForwardingData>>>,
    logins: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::new(write_half);

    let handshake_packet = read_packet(&mut reader).await;
    let handshake = Handshake::decode(&handshake_packet).expect("handshake");
    assert_eq!(handshake.next_state, NextState::Login);
    let login_start = read_packet(&mut reader).await;
    assert_eq!(login_start.id, 0x00);

    // Ask for Velocity modern forwarding data.
    let request = encode_plugin_request(7, login::VELOCITY_PLAYER_INFO_CHANNEL, &[1]);
    write_packet(&mut writer, 0x04, &request).await;
    let response_packet = read_packet(&mut reader).await;
    assert_eq!(response_packet.id, 0x02);
    let response = PluginResponse::decode(&response_packet).expect("plugin response");
    assert!(response.successful, "proxy refused to forward player data");
    assert_eq!(response.message_id, 7);
    let (version, data) = verify_forwarding_data(SECRET.as_bytes(), &response.data)
        .expect("verify forwarding data")
        .expect("HMAC mismatch");
    assert_eq!(version, 1);
    forwarded.lock().unwrap().push(data);

    if compression >= 0 {
        write_packet(&mut writer, 0x03, &encode_set_compression(compression)).await;
        writer.set_compression(Some(compression as u32));
        reader.set_compression(Some(compression as u32));
    }

    let uuid = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
    let success = encode_login_success(&uuid, "TestPlayer", &[]);
    write_packet(&mut writer, 0x02, &success).await;
    logins.fetch_add(1, Ordering::Relaxed);

    let ack = read_packet(&mut reader).await;
    assert_eq!(ack.id, 0x03, "proxy must acknowledge the login");

    // Echo every packet back so the test can check bidirectional relaying.
    loop {
        match reader.read_packet().await {
            Ok(packet) => {
                write_packet(&mut writer, packet.id, &packet.data).await;
            }
            Err(_) => return Ok(()),
        }
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

async fn spawn_proxy(
    config: &str,
) -> (
    SocketAddr,
    Arc<mc_proxy::Shared>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let (addr, views, handle) = spawn_proxy_workers(config).await;
    (addr, views[0].clone(), handle)
}

async fn spawn_proxy_workers(
    config: &str,
) -> (
    SocketAddr,
    Vec<Arc<mc_proxy::Shared>>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    init_tracing();
    let config = Config::from_toml(config).expect("proxy config");
    let proxy = Proxy::bind(config).await.expect("bind proxy");
    let addr = proxy.local_addr();
    let views = proxy.worker_views().to_vec();
    let handle = proxy.spawn();
    (addr, views, handle)
}

struct TestClient {
    reader: Reader,
    writer: Writer,
}

impl TestClient {
    async fn connect(addr: SocketAddr, host: &str, next: NextState) -> Self {
        let stream = TcpStream::connect(addr).await.expect("connect to proxy");
        let (read_half, write_half) = stream.into_split();
        let mut client = Self {
            reader: FrameReader::new(read_half),
            writer: FrameWriter::new(write_half),
        };
        let handshake = Handshake {
            protocol_version: PROTOCOL,
            server_address: host.to_owned(),
            server_port: 25565,
            next_state: next,
        };
        write_packet(&mut client.writer, 0x00, &handshake.encode().into_inner()).await;
        client
    }

    async fn send(&mut self, id: i32, data: &[u8]) {
        write_packet(&mut self.writer, id, data).await;
    }

    async fn read(&mut self) -> Packet {
        read_packet(&mut self.reader).await
    }

    /// Send Login Start and wait for Login Success, handling Set Compression.
    async fn login(&mut self, name: &str, uuid: Option<Uuid>) {
        let start = login::encode_login_start(name, uuid.as_ref(), PROTOCOL);
        self.send(0x00, &start).await;
        loop {
            let packet = self.read().await;
            match packet.id {
                0x03 => {
                    let threshold = login::decode_set_compression(&packet).unwrap();
                    self.reader.set_compression(Some(threshold));
                    self.writer.set_compression(Some(threshold));
                }
                0x02 => {
                    // Login Success: verify the relayed profile.
                    let mut reader = packet.reader();
                    let uuid = reader.read_uuid().unwrap();
                    let name = reader.read_string(16).unwrap();
                    assert_eq!(name, "TestPlayer");
                    assert_eq!(
                        uuid,
                        Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef)
                    );
                    break;
                }
                0x00 => {
                    let reason = login::decode_disconnect_reason(&packet);
                    panic!("proxy disconnected the client: {reason}");
                }
                other => panic!("unexpected login packet 0x{other:02x}"),
            }
        }
        // 1.20.2+ enters the configuration state.
        self.send(0x03, &[]).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_ping_returns_motd_and_counts() {
    let backend = spawn_backend(256).await;
    let config = format!(
        r#"
bind = "127.0.0.1:0"
motd = "&aHello &lWorld"
max_players = 42
version_name = "mc-proxy-test"
forwarding_secret = "{SECRET}"

[[servers]]
name = "lobby"
address = "{}"

[try]
servers = ["lobby"]
"#,
        backend.addr
    );
    let (addr, _shared, task) = spawn_proxy(&config).await;

    let mut client = TestClient::connect(addr, "localhost", NextState::Status).await;
    client.send(status::STATUS_REQUEST_ID, &[]).await;
    let response = client.read().await;
    assert_eq!(response.id, status::STATUS_RESPONSE_ID);
    let mut reader = response.reader();
    let json: serde_json::Value =
        serde_json::from_str(&reader.read_string(32767).unwrap()).unwrap();
    assert_eq!(json["version"]["name"], "mc-proxy-test");
    assert_eq!(json["version"]["protocol"], PROTOCOL);
    assert_eq!(json["players"]["max"], 42);
    assert_eq!(json["players"]["online"], 0);
    assert_eq!(json["description"]["text"], "§aHello §lWorld");

    // Ping/pong echo.
    client
        .send(status::PING_REQUEST_ID, &status::encode_pong(1234))
        .await;
    let pong = client.read().await;
    assert_eq!(pong.id, status::PONG_RESPONSE_ID);
    assert_eq!(status::decode_ping(&pong).unwrap(), 1234);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_login_forwards_identity_and_relays() {
    let backend = spawn_backend(256).await;
    let config = format!(
        r#"
bind = "127.0.0.1:0"
motd = "test"
online_mode = false
compression_threshold = 64
forwarding_secret = "{SECRET}"

[[servers]]
name = "lobby"
address = "{}"

[try]
servers = ["lobby"]
"#,
        backend.addr
    );
    let (addr, shared, task) = spawn_proxy(&config).await;

    let mut client = TestClient::connect(addr, "localhost", NextState::Login).await;
    client.login("TestPlayer", None).await;

    // The fake backend saw the forwarded identity.
    let forwarded = backend.forwarded.lock().unwrap().clone();
    assert_eq!(forwarded.len(), 1);
    let data = &forwarded[0];
    assert_eq!(data.name, "TestPlayer");
    assert_eq!(data.address, "127.0.0.1");
    assert_eq!(data.uuid, mc_proxy::crypto::offline_uuid("TestPlayer"));

    // Bidirectional relay: send a packet, expect the backend's echo.
    let payload: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
    client.send(0x7F, &payload).await;
    let echoed = client.read().await;
    assert_eq!(echoed.id, 0x7F);
    assert_eq!(&echoed.data[..], &payload[..]);

    assert_eq!(shared.online_players(), 1);
    assert_eq!(backend.logins.load(Ordering::Relaxed), 1);

    drop(client);
    // Wait for the session to clean up.
    for _ in 0..100 {
        if shared.online_players() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(shared.online_players(), 0);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compressed_large_packet_relays() {
    let backend = spawn_backend(16).await;
    let config = format!(
        r#"
bind = "127.0.0.1:0"
compression_threshold = 8
forwarding_secret = "{SECRET}"

[[servers]]
name = "lobby"
address = "{}"

[try]
servers = ["lobby"]
"#,
        backend.addr
    );
    let (addr, _shared, task) = spawn_proxy(&config).await;

    let mut client = TestClient::connect(addr, "localhost", NextState::Login).await;
    client.login("TestPlayer", Some(Uuid::from_u128(7))).await;

    let payload: Vec<u8> = (0..300_000u32).map(|i| ((i * 7) % 256) as u8).collect();
    client.send(0x10, &payload).await;
    let echoed = client.read().await;
    assert_eq!(echoed.id, 0x10);
    assert_eq!(echoed.data.len(), payload.len());
    assert_eq!(&echoed.data[..], &payload[..]);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forced_host_routing_selects_backend() {
    let lobby = spawn_backend(256).await;
    let survival = spawn_backend(256).await;
    let config = format!(
        r#"
bind = "127.0.0.1:0"
forwarding_secret = "{SECRET}"

[[servers]]
name = "lobby"
address = "{}"

[[servers]]
name = "survival"
address = "{}"

[forced_hosts]
"survival.example.com" = "survival"

[try]
servers = ["lobby"]
"#,
        lobby.addr, survival.addr
    );
    let (addr, _shared, task) = spawn_proxy(&config).await;

    let mut client = TestClient::connect(addr, "survival.example.com", NextState::Login).await;
    client.login("TestPlayer", None).await;
    client.send(0x01, b"ping").await;
    let echoed = client.read().await;
    assert_eq!(echoed.id, 0x01);

    assert_eq!(survival.logins.load(Ordering::Relaxed), 1);
    assert_eq!(lobby.logins.load(Ordering::Relaxed), 0);
    assert_eq!(survival.forwarded.lock().unwrap().len(), 1);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn online_mode_login_uses_encryption_and_session_server() {
    let backend = spawn_backend(256).await;

    // Mock session server returning a fixed profile.
    let session = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let session_addr = session.local_addr().unwrap();
    let captured_query = Arc::new(Mutex::new(String::new()));
    let captured = Arc::clone(&captured_query);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = session.accept().await else {
                return;
            };
            let captured = Arc::clone(&captured);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    let Ok(n) = stream.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&request);
                let path = text
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_owned();
                *captured.lock().unwrap() = path;
                let body = r#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch","properties":[{"name":"textures","value":"dGV4dHVyZQ==","signature":"c2ln"}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    let config = format!(
        r#"
bind = "127.0.0.1:0"
online_mode = true
forwarding_secret = "{SECRET}"

[auth]
session_server = "http://{session_addr}"

[[servers]]
name = "lobby"
address = "{}"

[try]
servers = ["lobby"]
"#,
        backend.addr
    );
    let (addr, _shared, task) = spawn_proxy(&config).await;

    let stream = TcpStream::connect(addr).await.unwrap();
    let (read_half, write_half) = stream.into_split();
    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::new(write_half);
    let handshake = Handshake {
        protocol_version: PROTOCOL,
        server_address: "localhost".into(),
        server_port: 25565,
        next_state: NextState::Login,
    };
    write_packet(&mut writer, 0x00, &handshake.encode().into_inner()).await;
    write_packet(
        &mut writer,
        0x00,
        &login::encode_login_start("Notch", None, PROTOCOL),
    )
    .await;

    let request = read_packet(&mut reader).await;
    assert_eq!(request.id, 0x01, "expected an Encryption Request");
    let mut request_reader = request.reader();
    let server_id = request_reader.read_string(64).unwrap();
    assert_eq!(server_id, "");
    let public_key = request_reader.read_byte_array(2048).unwrap().to_vec();
    let verify_token = request_reader.read_byte_array(64).unwrap().to_vec();

    let mut shared_secret = [0u8; 16];
    rand::RngCore::fill_bytes(&mut OsRng, &mut shared_secret);
    let rsa_key = RsaPublicKey::from_public_key_der(&public_key).unwrap();
    let encrypted_secret = rsa_key
        .encrypt(&mut OsRng, Pkcs1v15Encrypt, &shared_secret)
        .unwrap();
    let encrypted_token = rsa_key
        .encrypt(&mut OsRng, Pkcs1v15Encrypt, &verify_token)
        .unwrap();
    let mut response = CodecWriter::new();
    response
        .byte_array(&encrypted_secret)
        .byte_array(&encrypted_token);
    write_packet(&mut writer, 0x01, &response.into_inner()).await;

    // Everything after this point is AES/CFB8 encrypted.
    let keys = CipherKeys::from_shared_secret(shared_secret);
    reader.enable_encryption(&keys);
    writer.enable_encryption(&keys);

    loop {
        let packet = read_packet(&mut reader).await;
        match packet.id {
            0x03 => {
                let threshold = login::decode_set_compression(&packet).unwrap();
                reader.set_compression(Some(threshold));
                writer.set_compression(Some(threshold));
            }
            0x02 => break,
            0x00 => panic!(
                "proxy disconnected during online-mode login: {}",
                login::decode_disconnect_reason(&packet)
            ),
            other => panic!("unexpected packet 0x{other:02x}"),
        }
    }

    // Verify the session server was queried with the correct server hash.
    let path = captured_query.lock().unwrap().clone();
    let expected_hash = minecraft_server_hash("", &shared_secret, &public_key);
    assert!(
        path.contains(&format!("serverId={expected_hash}")),
        "unexpected session query: {path}"
    );
    assert!(path.contains("username=Notch"));
    assert!(path.contains("ip=127.0.0.1"));

    // The backend must have received the authenticated profile over forwarding.
    let forwarded = backend.forwarded.lock().unwrap().clone();
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0].name, "Notch");
    assert_eq!(
        forwarded[0].uuid,
        Uuid::parse_str("069a79f4-44e9-4726-a5be-fca90e38aaf5").unwrap()
    );
    assert_eq!(forwarded[0].properties.len(), 1);
    assert_eq!(
        forwarded[0].properties[0].signature.as_deref(),
        Some("c2ln")
    );

    // Finish login and exchange a play packet.
    write_packet(&mut writer, 0x03, &[]).await;
    write_packet(&mut writer, 0x22, b"encrypted-play").await;
    let echoed = read_packet(&mut reader).await;
    assert_eq!(echoed.id, 0x22);
    assert_eq!(&echoed.data[..], b"encrypted-play");
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_protocol_is_disconnected() {
    let backend = spawn_backend(256).await;
    let config = format!(
        r#"
bind = "127.0.0.1:0"
forwarding_secret = "{SECRET}"

[[servers]]
name = "lobby"
address = "{}"

[try]
servers = ["lobby"]
"#,
        backend.addr
    );
    let (addr, _shared, task) = spawn_proxy(&config).await;

    let stream = TcpStream::connect(addr).await.unwrap();
    let (read_half, write_half) = stream.into_split();
    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::new(write_half);
    let handshake = Handshake {
        protocol_version: 340,
        server_address: "localhost".into(),
        server_port: 25565,
        next_state: NextState::Login,
    };
    write_packet(&mut writer, 0x00, &handshake.encode().into_inner()).await;
    let packet = read_packet(&mut reader).await;
    assert_eq!(packet.id, 0x00);
    let reason = login::decode_disconnect_reason(&packet);
    assert!(reason.contains("1.13"), "unexpected reason: {reason}");
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_ping_gets_utf16_response() {
    let backend = spawn_backend(256).await;
    let config = format!(
        r#"
bind = "127.0.0.1:0"
motd = "Legacy"
max_players = 7

[[servers]]
name = "lobby"
address = "{}"

[try]
servers = ["lobby"]
"#,
        backend.addr
    );
    let (addr, _shared, task) = spawn_proxy(&config).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&[0xFE, 0x01, 0xFA]).await.unwrap();
    let mut response = Vec::new();
    let mut buf = [0u8; 512];
    let units = timeout(io_timeout(), async {
        let mut units = 0usize;
        loop {
            if response.len() >= 3 && units == 0 {
                units = u16::from_be_bytes([response[1], response[2]]) as usize;
            }
            if units > 0 && response.len() >= 3 + units * 2 {
                return units;
            }
            match stream.read(&mut buf).await {
                Ok(0) => return units,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(err) => panic!("read error: {err}"),
            }
        }
    })
    .await
    .expect("legacy ping timed out");
    assert_eq!(response[0], 0xFF);
    assert!(units > 0);
    let text: Vec<u16> = response[3..3 + units * 2]
        .chunks_exact(2)
        .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
        .collect();
    let text = String::from_utf16(&text).unwrap();
    assert!(text.contains("Legacy"), "unexpected legacy ping: {text}");
    assert!(
        text.ends_with("\u{0}0\u{0}7"),
        "unexpected player counts: {text}"
    );
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwarding_is_required_when_secret_is_set() {
    // A backend that never asks for forwarding data must be refused.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let (read_half, write_half) = stream.into_split();
                let mut reader = FrameReader::new(read_half);
                let mut writer = FrameWriter::new(write_half);
                let _ = read_packet(&mut reader).await;
                let _ = read_packet(&mut reader).await;
                let uuid = Uuid::nil();
                let _ = writer
                    .write_packet(0x02, &encode_login_success(&uuid, "TestPlayer", &[]))
                    .await;
                let _ = read_packet(&mut reader).await;
                // Keep the connection open briefly.
                tokio::time::sleep(Duration::from_secs(2)).await;
            });
        }
    });

    let config = format!(
        r#"
bind = "127.0.0.1:0"
forwarding_secret = "{SECRET}"

[[servers]]
name = "lobby"
address = "{backend_addr}"

[try]
servers = ["lobby"]
"#
    );
    let (addr, _shared, task) = spawn_proxy(&config).await;

    let mut client = TestClient::connect(addr, "localhost", NextState::Login).await;
    client
        .send(
            0x00,
            &login::encode_login_start("TestPlayer", None, PROTOCOL),
        )
        .await;
    let packet = client.read().await;
    assert_eq!(packet.id, 0x00, "expected a login disconnect");
    let reason = login::decode_disconnect_reason(&packet);
    assert!(
        reason.contains("forwarding") || reason.contains("Unable to connect"),
        "unexpected reason: {reason}"
    );
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn property_forwarding_roundtrips_through_protocol() {
    // Pure protocol check of the property codec used by forwarding.
    let profile_properties = vec![login::Property {
        name: "textures".into(),
        value: "dGV4dHVyZXM=".into(),
        signature: Some("c2ln".into()),
    }];
    let payload = encode_login_success(&Uuid::from_u128(9), "Notch", &profile_properties);
    let mut reader = mc_proxy::protocol::codec::Reader::new(&payload);
    let _uuid = reader.read_uuid().unwrap();
    let _name = reader.read_string(16).unwrap();
    let properties = decode_properties(&mut reader).unwrap();
    assert_eq!(properties, profile_properties);

    // Plugin response encoding used by the backend authenticator.
    let encoded = encode_plugin_response(3, true, b"payload");
    let packet = Packet::new(0x02, encoded);
    let response = PluginResponse::decode(&packet).unwrap();
    assert_eq!(response.message_id, 3);
    assert!(response.successful);
    assert_eq!(response.data, b"payload");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_handshake_bytes_are_decoded() {
    // Regression test for the status-ping byte layout: peek the first frame
    // length byte, then decode the handshake with the protocol version.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, write_half) = stream.into_split();
        let mut reader = FrameReader::new(read_half);
        let first = reader.peek_byte().await.unwrap();
        assert_eq!(first, 0x10);
        let packet = reader.read_packet().await.unwrap();
        let handshake = Handshake::decode(&packet).unwrap();
        assert_eq!(handshake.protocol_version, 769);
        assert_eq!(handshake.server_address, "localhost");
        assert_eq!(handshake.next_state, NextState::Status);
        let mut writer = FrameWriter::new(write_half);
        writer.write_packet(0x00, b"ok").await.unwrap();
    });
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut read_half, mut write_half) = stream.into_split();
    // frame length 16: id, VarInt 769 (0x81 0x06), address, port, next state
    let raw: &[u8] = &[
        0x10, 0x00, 0x81, 0x06, 0x09, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't', 0x63,
        0xdd, 0x01,
    ];
    write_half.write_all(raw).await.unwrap();
    write_half.flush().await.unwrap();
    let mut buf = [0u8; 16];
    let n = read_half.read(&mut buf).await.unwrap();
    assert!(n > 0);
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_worker_replicates_player_state() {
    let backend = spawn_backend(256).await;
    let config = format!(
        r#"
bind = "127.0.0.1:0"
workers = 2
pin_cores = false
forwarding_secret = "{SECRET}"

[[servers]]
name = "lobby"
address = "{}"

[try]
servers = ["lobby"]
"#,
        backend.addr
    );
    let (addr, views, task) = spawn_proxy_workers(&config).await;
    assert_eq!(views.len(), 2);

    let mut client = TestClient::connect(addr, "localhost", NextState::Login).await;
    client.login("TestPlayer", None).await;

    // The joining worker updates its own view immediately; the other worker
    // receives the event through the ring-buffer bus.
    for (index, view) in views.iter().enumerate() {
        let mut observed = 0;
        for _ in 0..100 {
            observed = view.online_players();
            if observed == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            observed, 1,
            "worker {index} did not replicate the join event"
        );
    }

    // A status connection served by either worker reports the global player.
    let mut status_client = TestClient::connect(addr, "localhost", NextState::Status).await;
    status_client.send(status::STATUS_REQUEST_ID, &[]).await;
    let response = status_client.read().await;
    let mut reader = response.reader();
    let json: serde_json::Value =
        serde_json::from_str(&reader.read_string(32767).unwrap()).unwrap();
    assert_eq!(json["players"]["online"], 1);
    assert_eq!(json["players"]["sample"][0]["name"], "TestPlayer");

    drop(client);
    for view in &views {
        let mut observed = view.online_players();
        for _ in 0..100 {
            observed = view.online_players();
            if observed == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(observed, 0, "leave event was not replicated");
    }
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_worker_serves_parallel_connections() {
    let backend = spawn_backend(256).await;
    let config = format!(
        r#"
bind = "127.0.0.1:0"
workers = 3
pin_cores = false
forwarding_secret = "{SECRET}"

[[servers]]
name = "lobby"
address = "{}"

[try]
servers = ["lobby"]
"#,
        backend.addr
    );
    let (addr, views, task) = spawn_proxy_workers(&config).await;
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let mut clients = Vec::new();
    for index in 0..9u8 {
        let mut release = release_rx.clone();
        clients.push(tokio::spawn(async move {
            let mut client = TestClient::connect(addr, "localhost", NextState::Login).await;
            // Distinct names -> distinct offline UUIDs, so the replicated
            // player registry counts every client.
            client.login(&format!("TestPlayer{index}"), None).await;
            let payload = vec![index; 512];
            client.send(0x33, &payload).await;
            let echoed = client.read().await;
            assert_eq!(echoed.id, 0x33);
            assert_eq!(&echoed.data[..], &payload[..]);
            // Stay connected until the test has checked the replicated view.
            let _ = release.wait_for(|released| *released).await;
            drop(client);
        }));
    }
    for (index, view) in views.iter().enumerate() {
        let mut observed = 0;
        for _ in 0..200 {
            observed = view.online_players();
            if observed == 9 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(observed, 9, "worker {index} missed player events");
    }

    // Release the clients and wait for the leave events to propagate.
    release_tx.send(true).unwrap();
    for client in clients {
        client.await.expect("client failed");
    }
    for (index, view) in views.iter().enumerate() {
        let mut observed = view.online_players();
        for _ in 0..200 {
            observed = view.online_players();
            if observed == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(observed, 0, "worker {index} missed leave events");
    }
    task.abort();
}
