//! TCP and UDP echo server built on `tokio-fstack`.
//!
//! With the default backend this is an ordinary kernel-socket server; with
//! `--features fstack` it serves from F-Stack's user-space stack.
//!
//! ```text
//! cargo run --example echo_server
//! cargo run --features fstack --example echo_server -- \
//!     --conf /etc/f-stack.conf --proc-type=primary
//! ```

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_fstack::{Config, TcpListener, TcpStream, UdpSocket};

fn main() -> std::io::Result<()> {
    // Everything after the executable name is passed to ff_init (ignored by
    // the standard backend). run_with_stack keeps ff_init/ff_run on the main
    // thread, which F-Stack requires.
    let eal_args: Vec<String> = std::env::args().skip(1).collect();
    let config = Config::default().with_eal_args(eal_args);
    tokio_fstack::run_with_stack(config, || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(server_main())
    })
}

async fn server_main() -> std::io::Result<()> {
    eprintln!("backend: {}", tokio_fstack::backend());

    let tcp = TcpListener::bind("0.0.0.0:7000").await?;
    let udp = UdpSocket::bind("0.0.0.0:7001").await?;
    eprintln!("tcp echo on {}", tcp.local_addr()?);
    eprintln!("udp echo on {}", udp.local_addr()?);

    let tcp_task = tokio::spawn(async move {
        loop {
            let (stream, peer) = tcp.accept().await?;
            tokio::spawn(async move {
                if let Err(err) = tcp_echo(stream, peer).await {
                    eprintln!("tcp client {peer} ended: {err}");
                }
            });
        }
        #[allow(unreachable_code)]
        Ok::<(), std::io::Error>(())
    });

    let udp_task = tokio::spawn(async move {
        let mut buf = [0u8; 65535];
        loop {
            let (len, peer) = udp.recv_from(&mut buf).await?;
            udp.send_to(&buf[..len], peer).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), std::io::Error>(())
    });

    tokio::select! {
        result = tcp_task => result.expect("tcp task panicked")?,
        result = udp_task => result.expect("udp task panicked")?,
    }
    Ok(())
}

async fn tcp_echo(mut stream: TcpStream, peer: SocketAddr) -> std::io::Result<()> {
    eprintln!("tcp client connected from {peer}");
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let len = stream.read(&mut buf).await?;
        if len == 0 {
            return Ok(());
        }
        stream.write_all(&buf[..len]).await?;
    }
}
