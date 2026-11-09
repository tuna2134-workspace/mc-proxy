//! Client for the `echo_server` example.
//!
//! ```text
//! cargo run --example echo_client
//! cargo run --features fstack --example echo_client -- \
//!     --conf /etc/f-stack.conf --proc-type=primary
//! ```

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_fstack::{Config, TcpStream, UdpSocket};

fn main() -> std::io::Result<()> {
    let eal_args: Vec<String> = std::env::args().skip(1).collect();
    let config = Config::default().with_eal_args(eal_args);
    tokio_fstack::run_with_stack(config, || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(client_main())
    })
}

async fn client_main() -> std::io::Result<()> {
    let mut tcp = TcpStream::connect("127.0.0.1:7000").await?;
    tcp.set_nodelay(true)?;
    let payload = b"hello over tokio-fstack";
    tcp.write_all(payload).await?;
    let mut buf = [0u8; 64];
    let len = tcp.read(&mut buf).await?;
    assert_eq!(&buf[..len], payload);
    println!("tcp echo ok: {} bytes", len);

    let udp = UdpSocket::bind("0.0.0.0:0").await?;
    let target: std::net::SocketAddr = "127.0.0.1:7001".parse().unwrap();
    udp.send_to(b"hello over udp", target).await?;
    let (len, peer) = tokio::time::timeout(Duration::from_secs(5), udp.recv_from(&mut buf))
        .await
        .expect("udp echo timed out")?;
    println!("udp echo ok: {} bytes from {}", len, peer);
    Ok(())
}
