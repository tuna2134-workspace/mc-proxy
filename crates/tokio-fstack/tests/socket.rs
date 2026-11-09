//! Integration tests for the reactor and socket types.
//!
//! These run against the standard (kernel) backend, exercising exactly the
//! same async machinery that the F-Stack backend uses: op submission,
//! `EAGAIN`/readiness transitions, cancellation and buffer ownership.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_fstack::{TcpListener, TcpStream, UdpSocket};

async fn echo_server() -> io::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) => return,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
            });
        }
    });
    Ok(addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_echo_roundtrip() -> io::Result<()> {
    let addr = echo_server().await?;
    let mut stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    assert!(stream.nodelay()?);
    assert_eq!(stream.peer_addr()?.ip(), addr.ip());

    stream.write_all(b"hello f-stack").await?;
    let mut buf = [0u8; 13];
    stream.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"hello f-stack");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_split_large_transfer() -> io::Result<()> {
    let addr = echo_server().await?;
    let stream = TcpStream::connect(addr).await?;
    let (mut reader, mut writer) = stream.into_split();

    let payload: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
    let expected = payload.clone();
    let writer_task = tokio::spawn(async move {
        writer.write_all(&payload).await?;
        writer.shutdown().await
    });

    let mut received = vec![0u8; expected.len()];
    reader.read_exact(&mut received).await?;
    writer_task.await.expect("writer task panicked")?;
    assert_eq!(received, expected);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_eof_is_reported() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream.write_all(b"bye").await.unwrap();
        drop(stream);
    });

    let mut client = TcpStream::connect(addr).await?;
    let mut buf = [0u8; 16];
    let n = client.read(&mut buf).await?;
    assert_eq!(&buf[..n], b"bye");
    let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
        .await
        .expect("EOF was not observed")?;
    assert_eq!(n, 0, "expected EOF");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_cancelled_read_does_not_lose_data() -> io::Result<()> {
    let addr = echo_server().await?;
    let mut stream = TcpStream::connect(addr).await?;

    // No data yet: this read is cancelled by the timeout.
    let mut buf = [0u8; 8];
    let cancelled = tokio::time::timeout(Duration::from_millis(100), stream.read(&mut buf)).await;
    assert!(cancelled.is_err(), "read should have timed out");

    // A fresh read must still work (and must not observe a bogus EOF).
    stream.write_all(b"after-cancel").await?;
    let mut buf = [0u8; 12];
    stream.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"after-cancel");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_many_concurrent_connections() -> io::Result<()> {
    let addr = echo_server().await?;
    let mut tasks = Vec::new();
    for client_id in 0..32u8 {
        tasks.push(tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let payload = vec![client_id; 4096];
            stream.write_all(&payload).await.unwrap();
            let mut buf = vec![0u8; payload.len()];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, payload);
        }));
    }
    for task in tasks {
        task.await.expect("client task panicked");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_send_receive() -> io::Result<()> {
    let a = UdpSocket::bind("127.0.0.1:0").await?;
    let b = UdpSocket::bind("127.0.0.1:0").await?;
    let a_addr = a.local_addr()?;
    let b_addr = b.local_addr()?;

    a.send_to(b"ping", b_addr).await?;
    let mut buf = [0u8; 64];
    let (n, peer) = b.recv_from(&mut buf).await?;
    assert_eq!(&buf[..n], b"ping");
    assert_eq!(peer, a_addr);

    b.send_to(&buf[..n], peer).await?;
    let (n, peer) = a.recv_from(&mut buf).await?;
    assert_eq!(&buf[..n], b"ping");
    assert_eq!(peer, b_addr);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_empty_datagram_and_connected_api() -> io::Result<()> {
    let a = UdpSocket::bind("127.0.0.1:0").await?;
    let b = UdpSocket::bind("127.0.0.1:0").await?;
    let b_addr = b.local_addr()?;

    a.connect(b_addr)?;
    assert_eq!(a.peer_addr()?, b_addr);
    a.send(b"").await?;
    let mut buf = [0u8; 8];
    let n = b.recv(&mut buf).await?;
    assert_eq!(n, 0, "empty datagram should be delivered");

    a.send(b"unicast").await?;
    let n = b.recv(&mut buf).await?;
    assert_eq!(&buf[..n], b"unicast");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_readiness_and_try_apis() -> io::Result<()> {
    let a = UdpSocket::bind("127.0.0.1:0").await?;
    let b = UdpSocket::bind("127.0.0.1:0").await?;
    let _b_addr = b.local_addr()?;

    // Nothing to read yet.
    let mut buf = [0u8; 16];
    match a.try_recv_from(&mut buf) {
        Err(err) => assert_eq!(err.kind(), io::ErrorKind::WouldBlock),
        Ok(_) => panic!("unexpected datagram"),
    }

    b.send_to(b"ready", a.local_addr()?).await?;
    // `readable` must complete once the datagram arrived.
    tokio::time::timeout(Duration::from_secs(5), a.readable())
        .await
        .expect("readable timed out")?;
    let (n, _) = a.try_recv_from(&mut buf)?;
    assert_eq!(&buf[..n], b"ready");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listener_bind_and_local_addr() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert_ne!(addr.port(), 0);
    Ok(())
}
