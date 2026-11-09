//! Per-connection session state machine.
//!
//! ```text
//! handshake ──▶ status ──▶ (status request, ping) ──▶ close
//!          └──▶ login  ──▶ [encryption] ──▶ backend login ──▶ relay
//! ```

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::timeout;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::auth::GameProfile;
use crate::config::StatusMode;
use crate::crypto::{minecraft_server_hash, offline_uuid, random_verify_token};
use crate::protocol::codec::Packet;
use crate::protocol::frame::CipherKeys;
use crate::protocol::frame::{FrameReader, FrameWriter};
use crate::protocol::handshake::{
    self, Handshake, NextState, HANDSHAKE_ID, MAX_PROTOCOL, MIN_PROTOCOL,
};
use crate::protocol::login::{self, clientbound, serverbound, EncryptionResponse, LoginStart};
use crate::protocol::status::{self, PING_REQUEST_ID, STATUS_REQUEST_ID};
use crate::protocol::ProtocolError;
use crate::proxy::backend;
use crate::proxy::status::build_status;
use crate::proxy::ClusterMessage;
use crate::proxy::Shared;
use crate::servers::Server;
use crate::sockets::{OwnedReadHalf, OwnedWriteHalf, TcpStream};

/// Errors that terminate a session.
#[derive(Debug, thiserror::Error)]
enum SessionError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("invalid username")]
    InvalidUsername,
    #[error("authentication failed")]
    AuthFailed,
    #[error("no backend available: {0}")]
    NoBackend(String),
    #[error("backend error: {0}")]
    Backend(String),
    #[error("connection closed")]
    Closed,
    #[error(transparent)]
    Crypto(#[from] crate::crypto::CryptoError),
}

impl SessionError {
    fn is_quiet(&self) -> bool {
        matches!(
            self,
            SessionError::Closed
                | SessionError::Protocol(ProtocolError::Eof)
                | SessionError::Protocol(ProtocolError::Io(_))
        )
    }
}

/// Handle one accepted connection.
pub async fn handle(stream: TcpStream, peer: SocketAddr, shared: Arc<Shared>) {
    shared.metrics.connections.fetch_add(1, Ordering::Relaxed);
    let result = run(stream, peer, Arc::clone(&shared)).await;
    if let Err(err) = &result {
        shared.metrics.errors.fetch_add(1, Ordering::Relaxed);
        if err.is_quiet() {
            debug!(%peer, error = %err, "connection closed");
        } else {
            debug!(%peer, error = %err, "session ended with an error");
        }
    }
}

async fn run(stream: TcpStream, peer: SocketAddr, shared: Arc<Shared>) -> Result<(), SessionError> {
    stream.set_nodelay(true).ok();
    let (read_half, write_half) = stream.into_split();
    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::new(write_half);

    // Legacy (pre-1.7) pings start with 0xFE instead of a frame length.
    let first = timeout(shared.config.handshake_timeout(), reader.peek_byte())
        .await
        .map_err(|_| SessionError::Timeout("first byte"))??;
    if status::is_legacy_ping(first) {
        return legacy_ping(&mut reader, &mut writer, &shared).await;
    }

    let packet = read_timeout(&mut reader, shared.config.handshake_timeout(), "handshake").await?;
    let handshake = Handshake::decode(&packet)?;
    debug!(
        protocol = handshake.protocol_version,
        host = %handshake.server_address,
        state = ?handshake.next_state,
        %peer,
        "handshake"
    );
    match handshake.next_state {
        // Server-list pings are answered regardless of the client version so
        // that old clients still see the MOTD.
        NextState::Status => handle_status(reader, writer, shared, handshake, peer).await,
        NextState::Login | NextState::Transfer => {
            if !handshake::is_supported_protocol(handshake.protocol_version) {
                let message = format!(
                    "Outdated client! This proxy supports Minecraft 1.13+ (protocol {MIN_PROTOCOL}..{MAX_PROTOCOL})."
                );
                send_disconnect(&mut writer, &message).await.ok();
                return Ok(());
            }
            handle_login(reader, writer, shared, handshake, peer).await
        }
    }
}

async fn read_timeout<R: AsyncRead + Unpin>(
    reader: &mut FrameReader<R>,
    duration: Duration,
    what: &'static str,
) -> Result<Packet, SessionError> {
    timeout(duration, reader.read_packet())
        .await
        .map_err(|_| SessionError::Timeout(what))?
        .map_err(SessionError::from)
}

async fn send_disconnect<W: AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    message: &str,
) -> Result<(), SessionError> {
    let reason = serde_json::json!({ "text": message }).to_string();
    writer
        .write_packet(clientbound::DISCONNECT, &login::encode_disconnect(&reason))
        .await?;
    Ok(())
}

async fn legacy_ping(
    reader: &mut FrameReader<OwnedReadHalf>,
    writer: &mut FrameWriter<OwnedWriteHalf>,
    shared: &Shared,
) -> Result<(), SessionError> {
    // Consume the 0xFE marker, then look at the next byte to distinguish the
    // 1.6 handshake (0xFE 0x01) from the 1.4/1.5 one (0xFE).
    let _ = timeout(Duration::from_millis(500), reader.read_byte()).await;
    let modern = matches!(
        timeout(Duration::from_millis(250), reader.read_byte()).await,
        Ok(Ok(0x01))
    );
    let response = status::legacy_ping_response(
        127,
        &shared.config.version_name,
        &shared.config.motd,
        shared.online_players(),
        shared.config.max_players,
        modern,
    );
    writer.write_raw(&response).await?;
    Ok(())
}

async fn handle_status(
    mut reader: FrameReader<OwnedReadHalf>,
    mut writer: FrameWriter<OwnedWriteHalf>,
    shared: Arc<Shared>,
    handshake: Handshake,
    peer: SocketAddr,
) -> Result<(), SessionError> {
    let request = read_timeout(
        &mut reader,
        shared.config.handshake_timeout(),
        "status request",
    )
    .await?;
    if request.id != STATUS_REQUEST_ID {
        return Err(ProtocolError::UnexpectedPacket {
            id: request.id,
            state: "status",
        }
        .into());
    }

    match shared.config.status.mode {
        StatusMode::Proxy => {
            let json = build_status(&shared, handshake.protocol_version);
            writer
                .write_packet(
                    status::STATUS_RESPONSE_ID,
                    &status::encode_status_response(&json),
                )
                .await?;
        }
        StatusMode::Passthrough => {
            if let Err(err) =
                passthrough_status(&mut reader, &mut writer, &shared, &handshake).await
            {
                let server = shared.servers.default_server();
                warn!(
                    %peer,
                    server = %server.name,
                    protocol = handshake.protocol_version,
                    error = %err,
                    "status passthrough failed; answering locally"
                );
                let json = build_status(&shared, handshake.protocol_version);
                writer
                    .write_packet(
                        status::STATUS_RESPONSE_ID,
                        &status::encode_status_response(&json),
                    )
                    .await?;
            } else {
                return Ok(());
            }
        }
    }

    match read_timeout(&mut reader, shared.config.handshake_timeout(), "ping").await {
        Ok(ping) if ping.id == PING_REQUEST_ID => {
            let payload = status::decode_ping(&ping)?;
            writer
                .write_packet(status::PONG_RESPONSE_ID, &status::encode_pong(payload))
                .await?;
        }
        Ok(_) => {}
        Err(_) => {}
    }
    Ok(())
}

async fn passthrough_status(
    reader: &mut FrameReader<OwnedReadHalf>,
    writer: &mut FrameWriter<OwnedWriteHalf>,
    shared: &Shared,
    handshake: &Handshake,
) -> Result<(), SessionError> {
    let server = shared.servers.default_server();
    let backend_timeout = shared.config.backend_timeout();
    let (mut backend_reader, mut backend_writer, response) =
        match open_status_backend(server, handshake, backend_timeout).await {
            Ok(result) => result,
            Err(first) => {
                // A backend may close a status connection transiently (restart,
                // throttle, load balancer). One retry avoids falling back to
                // the local status for a single dropped connection.
                debug!(
                    server = %server.name,
                    error = %first,
                    "status passthrough failed; retrying once"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
                open_status_backend(server, handshake, backend_timeout).await?
            }
        };
    writer.write_packet(response.id, &response.data).await?;

    // From here on the client already has the passthrough status response.
    // If the backend disappears while relaying the ping, answer the ping
    // locally instead of making the caller send a *second* status response
    // (which corrupts the status stream for the client).
    let ping = match read_timeout(reader, shared.config.handshake_timeout(), "ping").await {
        Ok(ping) => ping,
        Err(_) => return Ok(()),
    };
    if ping.id != PING_REQUEST_ID {
        return Ok(());
    }

    let relayed = async {
        backend_writer.write_packet(ping.id, &ping.data).await?;
        let pong = timeout(
            shared.config.backend_timeout(),
            backend_reader.read_packet(),
        )
        .await
        .map_err(|_| SessionError::Timeout("backend pong"))??;
        writer.write_packet(pong.id, &pong.data).await?;
        Ok::<(), SessionError>(())
    }
    .await;

    if relayed.is_err() {
        // Paper/backends may close the status connection right after the
        // response, or a throttled connection may be dropped. Echo the ping
        // so the client still receives a valid pong.
        debug!(
            server = %server.name,
            "backend pong unavailable; answering ping locally"
        );
        let payload = status::decode_ping(&ping)?;
        writer
            .write_packet(status::PONG_RESPONSE_ID, &status::encode_pong(payload))
            .await?;
    }
    Ok(())
}

async fn open_status_backend(
    server: &Server,
    handshake: &Handshake,
    backend_timeout: Duration,
) -> Result<
    (
        FrameReader<OwnedReadHalf>,
        FrameWriter<OwnedWriteHalf>,
        Packet,
    ),
    SessionError,
> {
    let (mut reader, mut writer) = backend::open(server, backend_timeout)
        .await
        .map_err(|err| SessionError::Backend(err.to_string()))?;
    let status_handshake = Handshake {
        protocol_version: handshake.protocol_version,
        server_address: handshake.server_address.clone(),
        server_port: handshake.server_port,
        next_state: NextState::Status,
    };
    writer
        .write_packet(HANDSHAKE_ID, &status_handshake.encode().into_inner())
        .await?;
    writer.write_packet(STATUS_REQUEST_ID, &[]).await?;
    let response = timeout(backend_timeout, reader.read_packet())
        .await
        .map_err(|_| SessionError::Timeout("backend status response"))??;
    Ok((reader, writer, response))
}

fn valid_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

async fn handle_login(
    mut reader: FrameReader<OwnedReadHalf>,
    mut writer: FrameWriter<OwnedWriteHalf>,
    shared: Arc<Shared>,
    handshake: Handshake,
    peer: SocketAddr,
) -> Result<(), SessionError> {
    let login_packet =
        read_timeout(&mut reader, shared.config.login_timeout(), "login start").await?;
    let login_start = LoginStart::decode(&login_packet, handshake.protocol_version)?;
    if !valid_username(&login_start.name) {
        send_disconnect(&mut writer, "Invalid username").await.ok();
        return Err(SessionError::InvalidUsername);
    }
    debug!(player = %login_start.name, %peer, "login start");

    let profile = authenticate(
        &mut reader,
        &mut writer,
        &shared,
        &login_start,
        peer,
        handshake.protocol_version,
    )
    .await?;

    let mut last_error = String::new();
    for server in shared.servers.candidates(&handshake.server_address) {
        debug!(server = %server.name, "attempting backend login");
        match backend::login(
            &shared,
            server,
            &handshake,
            &profile,
            peer,
            shared.config.backend_timeout(),
        )
        .await
        {
            Ok(connection) => {
                return finish_login(reader, writer, shared, handshake, profile, connection, peer)
                    .await
            }
            Err(err) => {
                warn!(server = %server.name, error = %err, "backend login failed");
                last_error = err.to_string();
            }
        }
    }

    let message = format!("Unable to connect to a backend server: {last_error}");
    send_disconnect(&mut writer, &message).await.ok();
    Err(SessionError::NoBackend(last_error))
}

async fn authenticate(
    reader: &mut FrameReader<OwnedReadHalf>,
    writer: &mut FrameWriter<OwnedWriteHalf>,
    shared: &Shared,
    login_start: &LoginStart,
    peer: SocketAddr,
    protocol_version: i32,
) -> Result<GameProfile, SessionError> {
    if !shared.config.online_mode {
        // The UUID is derived from the name, exactly like the vanilla
        // offline-mode server. The UUID sent by 1.20.2+ clients is ignored:
        // trusting it would let players impersonate each other.
        return Ok(GameProfile {
            uuid: offline_uuid(&login_start.name),
            name: login_start.name.clone(),
            properties: Vec::new(),
        });
    }

    let key = shared.key.as_ref().ok_or(SessionError::AuthFailed)?;
    let verify_token = random_verify_token()?;
    // 1.20.5+ (protocol 766) requires the trailing Should Authenticate
    // boolean; older clients must not receive it.
    let request = login::encode_encryption_request(
        "",
        key.public_key_der(),
        &verify_token,
        protocol_version,
        true,
    );
    writer
        .write_packet(clientbound::ENCRYPTION_REQUEST, &request)
        .await?;

    let response_packet =
        read_timeout(reader, shared.config.login_timeout(), "encryption response").await?;
    let response = EncryptionResponse::decode(&response_packet)?;
    let secret = key.decrypt(&response.shared_secret)?;
    if secret.len() != 16 {
        send_disconnect(writer, "Invalid shared secret").await.ok();
        return Err(SessionError::AuthFailed);
    }
    let received_token = key.decrypt(&response.verify_token)?;
    if received_token != verify_token {
        send_disconnect(writer, "Invalid verify token").await.ok();
        return Err(SessionError::AuthFailed);
    }
    let mut secret_bytes = [0u8; 16];
    secret_bytes.copy_from_slice(&secret);

    // From now on every frame in both directions is AES/CFB8 encrypted.
    let keys = CipherKeys::from_shared_secret(secret_bytes);
    reader.enable_encryption(&keys);
    writer.enable_encryption(&keys);

    let server_hash = minecraft_server_hash("", &secret_bytes, key.public_key_der());
    let authenticator = shared
        .authenticator
        .as_ref()
        .ok_or(SessionError::AuthFailed)?;
    match authenticator
        .has_joined(&login_start.name, &server_hash, Some(peer.ip()))
        .await
    {
        Ok(Some(profile)) => Ok(GameProfile {
            uuid: profile.uuid,
            name: profile.name,
            properties: profile.properties,
        }),
        Ok(None) => {
            warn!(player = %login_start.name, "session server refused the player");
            send_disconnect(writer, "Failed to verify username!")
                .await
                .ok();
            Err(SessionError::AuthFailed)
        }
        Err(err) => {
            warn!(player = %login_start.name, error = %err, "session server request failed");
            send_disconnect(
                writer,
                "Authentication servers are down. Please try again later.",
            )
            .await
            .ok();
            Err(SessionError::AuthFailed)
        }
    }
}

async fn finish_login(
    mut reader: FrameReader<OwnedReadHalf>,
    mut writer: FrameWriter<OwnedWriteHalf>,
    shared: Arc<Shared>,
    handshake: Handshake,
    profile: GameProfile,
    mut connection: backend::BackendConnection,
    peer: SocketAddr,
) -> Result<(), SessionError> {
    let protocol = handshake.protocol_version;

    // The client connection gets the proxy's own compression threshold; the
    // backend connection keeps whatever threshold it negotiated.
    if let Some(threshold) = shared.config.compression() {
        writer
            .write_packet(
                clientbound::SET_COMPRESSION,
                &login::encode_set_compression(threshold as i32),
            )
            .await?;
        writer.set_compression(Some(threshold));
        reader.set_compression(Some(threshold));
    }

    for packet in connection.pending.drain(..) {
        writer.write_packet(packet.id, &packet.data).await?;
    }
    writer
        .write_packet(clientbound::LOGIN_SUCCESS, &connection.login_success.data)
        .await?;

    // 1.20.2+: the client acknowledges the login and enters the configuration
    // state. The backend was already acknowledged in `backend::login`.
    if protocol >= 764 {
        loop {
            let packet = read_timeout(
                &mut reader,
                shared.config.login_timeout(),
                "login acknowledged",
            )
            .await?;
            match packet.id {
                serverbound::LOGIN_ACKNOWLEDGED => break,
                serverbound::COOKIE_RESPONSE => {
                    debug!("ignoring cookie response during login");
                }
                other => {
                    debug!(
                        id = other,
                        "ignoring packet while waiting for login acknowledged"
                    );
                }
            }
        }
    }

    shared.metrics.logins.fetch_add(1, Ordering::Relaxed);
    let guard = PlayerGuard::new(Arc::clone(&shared), profile.name.clone(), profile.uuid);
    info!(
        player = %profile.name,
        uuid = %profile.uuid,
        server = %connection.server_name,
        protocol,
        %peer,
        "player connected"
    );

    let result = relay(
        reader,
        writer,
        connection.reader,
        connection.writer,
        Arc::clone(&shared),
    )
    .await;
    drop(guard);
    result
}

async fn relay(
    client_reader: FrameReader<OwnedReadHalf>,
    client_writer: FrameWriter<OwnedWriteHalf>,
    backend_reader: FrameReader<OwnedReadHalf>,
    backend_writer: FrameWriter<OwnedWriteHalf>,
    shared: Arc<Shared>,
) -> Result<(), SessionError> {
    let mut client_to_backend = tokio::spawn(pipe(
        client_reader,
        backend_writer,
        Arc::clone(&shared),
        Direction::ClientToBackend,
    ));
    let mut backend_to_client = tokio::spawn(pipe(
        backend_reader,
        client_writer,
        Arc::clone(&shared),
        Direction::BackendToClient,
    ));

    let outcome = tokio::select! {
        result = &mut client_to_backend => result,
        result = &mut backend_to_client => result,
    };
    client_to_backend.abort();
    backend_to_client.abort();

    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(SessionError::Closed),
    }
}

#[derive(Clone, Copy)]
enum Direction {
    ClientToBackend,
    BackendToClient,
}

async fn pipe(
    mut reader: FrameReader<OwnedReadHalf>,
    mut writer: FrameWriter<OwnedWriteHalf>,
    shared: Arc<Shared>,
    direction: Direction,
) -> Result<(), SessionError> {
    loop {
        let packet = reader.read_packet().await?;
        match direction {
            Direction::ClientToBackend => {
                shared
                    .metrics
                    .packets_from_clients
                    .fetch_add(1, Ordering::Relaxed);
                shared
                    .metrics
                    .bytes_from_clients
                    .fetch_add(packet.data.len() as u64, Ordering::Relaxed);
            }
            Direction::BackendToClient => {
                shared
                    .metrics
                    .packets_from_backends
                    .fetch_add(1, Ordering::Relaxed);
                shared
                    .metrics
                    .bytes_to_clients
                    .fetch_add(packet.data.len() as u64, Ordering::Relaxed);
            }
        }
        writer.write_packet(packet.id, &packet.data).await?;
    }
}

/// Registers a player in the worker's view and replicates the event to the
/// other workers; removes it again when the session ends.
struct PlayerGuard {
    shared: Arc<Shared>,
    name: String,
    uuid: Uuid,
}

impl PlayerGuard {
    fn new(shared: Arc<Shared>, name: String, uuid: Uuid) -> Self {
        {
            let mut players = shared.players.lock().unwrap_or_else(|e| e.into_inner());
            if !players.iter().any(|(_, id)| *id == uuid) {
                players.push((name.clone(), uuid));
                shared.online.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some(bus) = &shared.bus {
            bus.broadcast_from(
                shared.worker_id,
                ClusterMessage::Joined {
                    worker: shared.worker_id,
                    name: name.clone(),
                    uuid,
                },
            );
        }
        Self { shared, name, uuid }
    }
}

impl Drop for PlayerGuard {
    fn drop(&mut self) {
        {
            let mut players = self
                .shared
                .players
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(index) = players.iter().position(|(_, uuid)| *uuid == self.uuid) {
                players.remove(index);
                self.shared.online.fetch_sub(1, Ordering::Relaxed);
            }
        }
        if let Some(bus) = &self.shared.bus {
            bus.broadcast_from(
                self.shared.worker_id,
                ClusterMessage::Left {
                    worker: self.shared.worker_id,
                    uuid: self.uuid,
                },
            );
        }
        info!(player = %self.name, uuid = %self.uuid, "player disconnected");
    }
}
