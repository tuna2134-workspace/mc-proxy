//! Online-mode authentication against the Mojang session server.
//!
//! HTTP/1.1 is spoken with [`hyper`] over the crate's own socket type
//! (`tokio-fstack`), optionally wrapped in rustls. Using hyper means the
//! request/response handling (chunked encoding, headers, limits) is the
//! battle-tested implementation instead of a hand-written parser, while still
//! working on an F-Stack-only host where the kernel network stack has been
//! taken over by DPDK: hyper talks to any `AsyncRead + AsyncWrite`, and
//! `hyper_util::rt::TokioIo` adapts our sockets to it.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::Bytes;
use hyper::header::{ACCEPT, CONNECTION, HOST, USER_AGENT};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite};
use uuid::Uuid;

use crate::protocol::login::Property;

/// A verified player profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameProfile {
    /// Player UUID.
    pub uuid: Uuid,
    /// Canonical player name.
    pub name: String,
    /// Skin and other properties.
    pub properties: Vec<Property>,
}

#[derive(Debug, Deserialize)]
struct ProfileJson {
    id: String,
    name: String,
    #[serde(default)]
    properties: Vec<PropertyJson>,
}

#[derive(Debug, Deserialize)]
struct PropertyJson {
    name: String,
    value: String,
    #[serde(default)]
    signature: Option<String>,
}

struct BaseUrl {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

/// Talks to the session server's `hasJoined` endpoint.
pub struct SessionAuthenticator {
    base: BaseUrl,
    timeout: Duration,
    tls: Option<Arc<rustls::ClientConfig>>,
}

impl SessionAuthenticator {
    /// Create an authenticator for a base URL such as
    /// `https://sessionserver.mojang.com`.
    pub fn new(base: &str, timeout: Duration) -> io::Result<Self> {
        let base = parse_base(base)?;
        let tls = if base.tls {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            Some(Arc::new(config))
        } else {
            None
        };
        Ok(Self { base, timeout, tls })
    }

    /// Verify that `username` owns a session for `server_hash`.
    ///
    /// Returns `Ok(None)` when the session server does not know the player.
    pub async fn has_joined(
        &self,
        username: &str,
        server_hash: &str,
        ip: Option<IpAddr>,
    ) -> io::Result<Option<GameProfile>> {
        let mut query = format!(
            "?username={}&serverId={}",
            percent_encode(username),
            percent_encode(server_hash)
        );
        if let Some(ip) = ip {
            query.push_str("&ip=");
            query.push_str(&percent_encode(&ip.to_string()));
        }
        let path = format!("{}/session/minecraft/hasJoined{}", self.base.path, query);
        let base = &self.base;
        let request = async {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((base.host.as_str(), base.port))
                .await?
                .collect();
            let mut last_error = None;
            for addr in addrs {
                match self.request(addr, &path).await {
                    Ok(response) => return Ok(response),
                    Err(err) => last_error = Some(err),
                }
            }
            Err(last_error.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "session server has no address",
                )
            }))
        };
        let response = tokio::time::timeout(self.timeout, request)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "session server timed out"))??;
        match response {
            Some(body) => {
                let profile: ProfileJson = serde_json::from_slice(&body)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
                Ok(Some(GameProfile {
                    uuid: parse_uuid(&profile.id)?,
                    name: profile.name,
                    properties: profile
                        .properties
                        .into_iter()
                        .map(|property| Property {
                            name: property.name,
                            value: property.value,
                            signature: property.signature,
                        })
                        .collect(),
                }))
            }
            None => Ok(None),
        }
    }

    async fn request(&self, addr: SocketAddr, path: &str) -> io::Result<Option<Vec<u8>>> {
        let stream = crate::sockets::TcpStream::connect_addr(addr).await?;
        stream.set_nodelay(true).ok();
        let (status, body) = match &self.tls {
            Some(config) => {
                let server_name = ServerName::try_from(self.base.host.clone())
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
                let connector = tokio_rustls::TlsConnector::from(Arc::clone(config));
                let tls = connector.connect(server_name, stream).await?;
                self.http_get(TokioIo::new(tls), path).await?
            }
            None => self.http_get(TokioIo::new(stream), path).await?,
        };
        match status {
            StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => Ok(Some(body)),
            status => Err(io::Error::other(format!(
                "session server returned HTTP {status}"
            ))),
        }
    }

    /// Perform a `GET` over an established (and, for HTTPS, already upgraded)
    /// connection.
    async fn http_get<I>(&self, io: TokioIo<I>, path: &str) -> io::Result<(StatusCode, Vec<u8>)>
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, connection) = hyper::client::conn::http1::handshake::<_, Empty<Bytes>>(io)
            .await
            .map_err(http_error)?;
        // Drive the connection while the request/response is in flight. The
        // future ends when the server closes the connection (we ask for
        // `Connection: close`) or when the sender is dropped.
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let host_header = if (self.base.tls && self.base.port == 443)
            || (!self.base.tls && self.base.port == 80)
        {
            self.base.host.clone()
        } else {
            format!("{}:{}", self.base.host, self.base.port)
        };
        let request = Request::builder()
            .method(Method::GET)
            .uri(path)
            .header(HOST, host_header)
            .header(
                USER_AGENT,
                format!("mc-proxy/{}", env!("CARGO_PKG_VERSION")),
            )
            .header(ACCEPT, "application/json")
            .header(CONNECTION, "close")
            .body(Empty::<Bytes>::new())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;

        let response = sender.send_request(request).await.map_err(http_error)?;
        let status = response.status();
        let body = Limited::new(response.into_body(), MAX_RESPONSE_BODY)
            .collect()
            .await
            .map_err(http_error)?;
        // Explicitly drop the sender so the connection future can finish.
        drop(sender);
        Ok((status, body.to_bytes().to_vec()))
    }
}

/// Upper bound on the session server response body (Mojang replies are tiny).
const MAX_RESPONSE_BODY: usize = 256 * 1024;

fn http_error(err: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("session server HTTP error: {err}"))
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn parse_uuid(raw: &str) -> io::Result<Uuid> {
    Uuid::parse_str(raw)
        .or_else(|_| {
            if raw.len() == 32 {
                let hyphenated = format!(
                    "{}-{}-{}-{}-{}",
                    &raw[0..8],
                    &raw[8..12],
                    &raw[12..16],
                    &raw[16..20],
                    &raw[20..32]
                );
                Uuid::parse_str(&hyphenated)
            } else {
                Uuid::parse_str(raw)
            }
        })
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn parse_base(base: &str) -> io::Result<BaseUrl> {
    let (tls, rest) = if let Some(rest) = base.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = base.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session server URL must start with http:// or https://",
        ));
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let default_port = if tls { 443 } else { 80 };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(']') || host.ends_with(']') => (
            host.trim_matches(['[', ']']).to_owned(),
            port.parse::<u16>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid session server port")
            })?,
        ),
        _ => (authority.trim_matches(['[', ']']).to_owned(), default_port),
    };
    if host.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session server host is empty",
        ));
    }
    Ok(BaseUrl {
        tls,
        host,
        port,
        path: path.trim_end_matches('/').to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_base_urls() {
        let base = parse_base("https://sessionserver.mojang.com").unwrap();
        assert!(base.tls);
        assert_eq!(base.host, "sessionserver.mojang.com");
        assert_eq!(base.port, 443);
        assert_eq!(base.path, "");

        let base = parse_base("http://127.0.0.1:8080/prefix/").unwrap();
        assert!(!base.tls);
        assert_eq!(base.host, "127.0.0.1");
        assert_eq!(base.port, 8080);
        assert_eq!(base.path, "/prefix");

        assert!(parse_base("ftp://example.com").is_err());
    }

    #[tokio::test]
    async fn hyper_client_parses_chunked_profile() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                request.extend_from_slice(&buf[..n]);
            }
            let text = String::from_utf8_lossy(&request);
            assert!(text.starts_with("GET /session/minecraft/hasJoined?username=Notch&serverId="));
            assert!(text.to_ascii_lowercase().contains("host: 127.0.0.1"));
            let body =
                r#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch","properties":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let auth =
            SessionAuthenticator::new(&format!("http://{addr}"), Duration::from_secs(5)).unwrap();
        let profile = auth
            .has_joined("Notch", "deadbeef", None)
            .await
            .expect("hyper request should succeed")
            .expect("profile should be returned");
        assert_eq!(profile.name, "Notch");
        assert_eq!(
            profile.uuid,
            Uuid::parse_str("069a79f4-44e9-4726-a5be-fca90e38aaf5").unwrap()
        );
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(percent_encode("a b:c"), "a%20b%3Ac");
    }

    #[tokio::test]
    #[ignore = "requires outbound HTTPS to sessionserver.mojang.com"]
    async fn real_session_server_rejects_unknown_session() {
        let auth =
            SessionAuthenticator::new("https://sessionserver.mojang.com", Duration::from_secs(15))
                .unwrap();
        let result = auth
            .has_joined(
                "DefinitelyNotARealUser123",
                "0123456789abcdef0123456789abcdef0123456",
                None,
            )
            .await
            .expect("session server request should succeed");
        assert!(result.is_none());
    }

    #[test]
    fn uuid_without_hyphens() {
        assert_eq!(
            parse_uuid("069a79f444e94726a5befca90e38aaf5").unwrap(),
            Uuid::parse_str("069a79f4-44e9-4726-a5be-fca90e38aaf5").unwrap()
        );
    }
}
