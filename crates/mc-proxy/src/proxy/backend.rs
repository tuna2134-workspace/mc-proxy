//! Connecting to, and logging in to, a backend server.

use std::net::SocketAddr;
use std::time::Duration;

use thiserror::Error;
use tracing::{debug, warn};

use crate::auth::GameProfile;
use crate::forwarding::{create_forwarding_data, requested_version, ForwardingData};
use crate::protocol::codec::Packet;
use crate::protocol::frame::{FrameReader, FrameWriter};
use crate::protocol::handshake::{self, Handshake, HANDSHAKE_ID};
use crate::protocol::login::{self, clientbound, serverbound, PluginRequest};
use crate::protocol::ProtocolError;
use crate::proxy::Shared;
use crate::servers::Server;
use crate::sockets::{OwnedReadHalf, OwnedWriteHalf, TcpStream};

/// Errors while establishing a backend session.
#[derive(Debug, Error)]
pub enum BackendError {
    /// Socket error.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Protocol error.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// The backend expects an online-mode login, which proxies cannot provide.
    #[error("backend is in online-mode; set online-mode=false on the backend")]
    OnlineModeBackend,
    /// The backend rejected the login.
    #[error("backend rejected login: {0}")]
    Rejected(String),
    /// The backend never asked for forwarding data.
    #[error("backend did not request player forwarding data")]
    ForwardingNotRequested,
    /// The backend is not reachable.
    #[error("backend unreachable: {0}")]
    Unreachable(String),
    /// A read or write timed out.
    #[error("backend timed out")]
    Timeout,
}

/// A logged-in backend connection, ready to relay play/configuration packets.
pub struct BackendConnection {
    /// Name of the server that accepted the login.
    pub server_name: String,
    /// Packet reader.
    pub reader: FrameReader<OwnedReadHalf>,
    /// Packet writer.
    pub writer: FrameWriter<OwnedWriteHalf>,
    /// The backend's Login Success packet (relayed to the client).
    pub login_success: Packet,
    /// Login-state packets received before Login Success that should be
    /// forwarded to the client (e.g. cookie requests).
    pub pending: Vec<Packet>,
    /// Whether forwarding data was sent.
    pub forwarded: bool,
}

/// Open a raw connection to `server`, split into frame reader and writer.
pub async fn open(
    server: &Server,
    timeout: Duration,
) -> Result<(FrameReader<OwnedReadHalf>, FrameWriter<OwnedWriteHalf>), BackendError> {
    let addr = tokio::time::timeout(timeout, server.resolve())
        .await
        .map_err(|_| BackendError::Timeout)
        .and_then(|result| result.map_err(|err| BackendError::Unreachable(err.to_string())))?;
    debug!(server = %server.name, %addr, "connecting to backend");
    let stream = tokio::time::timeout(timeout, TcpStream::connect_addr(addr))
        .await
        .map_err(|_| BackendError::Timeout)??;
    stream.set_nodelay(true).ok();
    let (read_half, write_half) = stream.into_split();
    Ok((FrameReader::new(read_half), FrameWriter::new(write_half)))
}

/// Connect to `server` and complete the login exchange on behalf of `profile`.
pub async fn login(
    shared: &Shared,
    server: &Server,
    handshake: &Handshake,
    profile: &GameProfile,
    peer: SocketAddr,
    timeout: Duration,
) -> Result<BackendConnection, BackendError> {
    let (mut reader, mut writer) = open(server, timeout).await?;

    // Handshake: forward the client's protocol version and virtual host so
    // that the backend sees the same "server address" the player used.
    let backend_handshake = handshake::backend_handshake(
        handshake.protocol_version,
        &handshake.server_address,
        handshake.server_port,
    );
    write(
        &mut writer,
        HANDSHAKE_ID,
        &backend_handshake.encode().into_inner(),
        timeout,
    )
    .await?;
    // Login Start with the authenticated identity.
    let login_start = login::encode_login_start(
        &profile.name,
        Some(&profile.uuid),
        handshake.protocol_version,
    );
    write(&mut writer, serverbound::LOGIN_START, &login_start, timeout).await?;

    let mut pending = Vec::new();
    let mut forwarded = false;
    loop {
        let packet = read(&mut reader, timeout).await?;
        match packet.id {
            clientbound::SET_COMPRESSION => {
                let threshold = login::decode_set_compression(&packet)?;
                debug!(threshold, "backend enabled compression");
                reader.set_compression(Some(threshold));
                writer.set_compression(Some(threshold));
            }
            clientbound::PLUGIN_REQUEST => {
                let request = PluginRequest::decode(&packet)?;
                debug!(channel = %request.channel, "backend login plugin request");
                if request.channel == login::VELOCITY_PLAYER_INFO_CHANNEL
                    && shared.forwarding_enabled()
                {
                    let version = requested_version(&request.data);
                    let data = ForwardingData {
                        address: peer.ip().to_string(),
                        uuid: profile.uuid,
                        name: profile.name.clone(),
                        properties: profile.properties.clone(),
                    };
                    // Version 1 is understood by every Velocity-aware backend
                    // (Paper supports up to version 4); it carries exactly the
                    // fields we have, so we always answer with version 1.
                    let payload = create_forwarding_data(&shared.forwarding_secret, &data, 1)?;
                    let response =
                        login::encode_plugin_response(request.message_id, true, &payload);
                    write(
                        &mut writer,
                        serverbound::PLUGIN_RESPONSE,
                        &response,
                        timeout,
                    )
                    .await?;
                    forwarded = true;
                    debug!(requested_version = version, "sent modern forwarding data");
                } else {
                    let response = login::encode_plugin_response(request.message_id, false, &[]);
                    write(
                        &mut writer,
                        serverbound::PLUGIN_RESPONSE,
                        &response,
                        timeout,
                    )
                    .await?;
                }
            }
            clientbound::LOGIN_SUCCESS => {
                if shared.forwarding_enabled() && !forwarded {
                    // Velocity refuses logins to backends that are not
                    // configured to consume forwarding data: without it the
                    // player's identity would be unauthenticated.
                    warn!(
                        server = %server.name,
                        "backend did not request velocity:player_info forwarding"
                    );
                    return Err(BackendError::ForwardingNotRequested);
                }
                // 1.20.2+ (764) moves to the configuration state, which the
                // proxy acknowledges immediately (the real client's
                // acknowledgement is consumed separately).
                if handshake.protocol_version >= 764 {
                    write(&mut writer, serverbound::LOGIN_ACKNOWLEDGED, &[], timeout).await?;
                }
                return Ok(BackendConnection {
                    server_name: server.name.clone(),
                    reader,
                    writer,
                    login_success: packet,
                    pending,
                    forwarded,
                });
            }
            clientbound::DISCONNECT => {
                let reason = login::decode_disconnect_reason(&packet);
                return Err(BackendError::Rejected(reason));
            }
            clientbound::ENCRYPTION_REQUEST => {
                return Err(BackendError::OnlineModeBackend);
            }
            _ => {
                debug!(id = packet.id, "buffering login packet for the client");
                pending.push(packet);
            }
        }
    }
}

async fn read(
    reader: &mut FrameReader<OwnedReadHalf>,
    timeout: Duration,
) -> Result<Packet, BackendError> {
    tokio::time::timeout(timeout, reader.read_packet())
        .await
        .map_err(|_| BackendError::Timeout)?
        .map_err(BackendError::Protocol)
}

async fn write(
    writer: &mut FrameWriter<OwnedWriteHalf>,
    id: i32,
    data: &[u8],
    timeout: Duration,
) -> Result<(), BackendError> {
    tokio::time::timeout(timeout, writer.write_packet(id, data))
        .await
        .map_err(|_| BackendError::Timeout)?
        .map_err(BackendError::Protocol)
}
