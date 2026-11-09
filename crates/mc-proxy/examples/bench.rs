//! Blocking benchmark client and fake Velocity-aware backend.
//!
//! ```text
//! bench backend 0.0.0.0:25566 [secret]
//! bench status  <addr> [seconds] [threads]
//! bench login   <addr> [seconds] [threads]
//! bench relay   <addr> [seconds] [threads] [payload-bytes]
//! ```
//!
//! The client runs on kernel sockets so it can drive either the standard
//! proxy (loopback) or the F-Stack proxy (via net_tap) from the outside.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use mc_proxy::forwarding::verify_forwarding_data;
use mc_proxy::protocol::codec::{write_varint, Packet, Reader};
use mc_proxy::protocol::handshake::{Handshake, NextState};
use mc_proxy::protocol::login::{
    self, clientbound, encode_login_success, encode_plugin_request, serverbound, LoginStart,
    PluginResponse,
};

const PROTOCOL: i32 = 770;
const MAX_FRAME: usize = 8 * 1024 * 1024;

fn pio(err: mc_proxy::protocol::ProtocolError) -> io::Error {
    io::Error::other(err.to_string())
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str).unwrap_or("") {
        "backend" => run_backend(
            &args[2],
            args.get(3).map(String::as_str).unwrap_or("bench-secret"),
        ),
        "status" => run_load(&args, LoadMode::Status),
        "login" => run_load(&args, LoadMode::Login),
        "relay" => run_load(&args, LoadMode::Relay),
        _ => {
            eprintln!(
                "usage: bench backend <addr> [secret] | status|login|relay <addr> [secs] [threads] [payload]"
            );
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// Framing (blocking)
// ---------------------------------------------------------------------------

struct Peer {
    reader: io::BufReader<TcpStream>,
    writer: io::BufWriter<TcpStream>,
    compression: Option<u32>,
}

impl Peer {
    fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        Ok(Self {
            reader: io::BufReader::with_capacity(16 * 1024, stream.try_clone()?),
            writer: io::BufWriter::with_capacity(16 * 1024, stream),
            compression: None,
        })
    }

    fn send(&mut self, id: i32, payload: &[u8]) -> io::Result<()> {
        let mut body = Vec::with_capacity(payload.len() + 5);
        write_varint(&mut body, id);
        body.extend_from_slice(payload);
        let mut frame = Vec::with_capacity(body.len() + 16);
        if let Some(threshold) = self.compression {
            if body.len() >= threshold as usize {
                let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(&body)?;
                let compressed = encoder.finish()?;
                write_varint(&mut frame, body.len() as i32);
                frame.extend_from_slice(&compressed);
            } else {
                write_varint(&mut frame, 0);
                frame.extend_from_slice(&body);
            }
        } else {
            frame.extend_from_slice(&body);
        }
        let mut out = Vec::with_capacity(frame.len() + 5);
        write_varint(&mut out, frame.len() as i32);
        out.extend_from_slice(&frame);
        self.writer.write_all(&out)?;
        self.writer.flush()
    }

    fn recv(&mut self) -> io::Result<(i32, Vec<u8>)> {
        let len = read_varint(&mut self.reader)?;
        if len < 0 || len as usize > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad frame length",
            ));
        }
        let mut frame = vec![0u8; len as usize];
        self.reader.read_exact(&mut frame)?;
        let payload = if self.compression.is_some() {
            let (data_len, used) = read_varint_slice(&frame)?;
            if data_len == 0 {
                frame[used..].to_vec()
            } else {
                let mut decoder = ZlibDecoder::new(&frame[used..]);
                let mut out = Vec::with_capacity(data_len as usize);
                decoder.read_to_end(&mut out)?;
                if out.len() != data_len as usize {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "bad zlib length",
                    ));
                }
                out
            }
        } else {
            frame
        };
        let (id, used) = read_varint_slice(&payload)?;
        Ok((id, payload[used..].to_vec()))
    }
}

fn read_varint<R: Read>(reader: &mut R) -> io::Result<i32> {
    let mut value = 0u32;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte)?;
        value |= u32::from(byte[0] & 0x7f) << shift;
        if byte[0] & 0x80 == 0 {
            return Ok(value as i32);
        }
        shift += 7;
        if shift >= 35 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "varint too long",
            ));
        }
    }
}

fn read_varint_slice(data: &[u8]) -> io::Result<(i32, usize)> {
    let mut value = 0u32;
    let mut shift = 0;
    for (index, byte) in data.iter().take(5).enumerate() {
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((value as i32, index + 1));
        }
        shift += 7;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "varint too long",
    ))
}

// ---------------------------------------------------------------------------
// Fake Paper backend
// ---------------------------------------------------------------------------

fn run_backend(addr: &str, secret: &str) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    eprintln!(
        "backend listening on {} (secret {}B)",
        listener.local_addr()?,
        secret.len()
    );
    let secret = Arc::new(secret.as_bytes().to_vec());
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let secret = Arc::clone(&secret);
        std::thread::spawn(move || {
            if let Err(err) = backend_session(stream, &secret) {
                eprintln!("backend session: {err}");
            }
        });
    }
    Ok(())
}

fn backend_session(stream: TcpStream, secret: &[u8]) -> io::Result<()> {
    let mut peer = Peer::new(stream)?;
    let (id, _) = peer.recv()?;
    if id != 0x00 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected handshake",
        ));
    }
    let (id, data) = peer.recv()?;
    if id != serverbound::LOGIN_START {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected login start",
        ));
    }
    let login_start = LoginStart::decode(&Packet::new(id, data), PROTOCOL).map_err(pio)?;
    peer.send(
        clientbound::PLUGIN_REQUEST,
        &encode_plugin_request(1, login::VELOCITY_PLAYER_INFO_CHANNEL, &[1]),
    )?;
    let (id, data) = peer.recv()?;
    if id != serverbound::PLUGIN_RESPONSE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected plugin response",
        ));
    }
    let response = PluginResponse::decode(&Packet::new(id, data)).map_err(pio)?;
    if !response.successful {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "forwarding refused",
        ));
    }
    let forwarded = verify_forwarding_data(secret, &response.data)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    let Some((_version, profile)) = forwarded else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad forwarding HMAC",
        ));
    };
    if profile.name != login_start.name {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "name mismatch"));
    }

    peer.send(
        clientbound::SET_COMPRESSION,
        &login::encode_set_compression(256),
    )?;
    peer.compression = Some(256);
    peer.send(
        clientbound::LOGIN_SUCCESS,
        &encode_login_success(&profile.uuid, &profile.name, &profile.properties),
    )?;
    let (id, _) = peer.recv()?;
    if id != serverbound::LOGIN_ACKNOWLEDGED {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected login acknowledged",
        ));
    }
    loop {
        let (id, data) = match peer.recv() {
            Ok(packet) => packet,
            Err(_) => return Ok(()),
        };
        peer.send(id, &data)?;
    }
}

// ---------------------------------------------------------------------------
// Load generator
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum LoadMode {
    Status,
    Login,
    Relay,
}

fn run_load(args: &[String], mode: LoadMode) -> io::Result<()> {
    let addr: SocketAddr = args[2]
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid address"))?;
    let seconds: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
    let threads: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);
    let payload = vec![0x5Au8; args.get(5).and_then(|s| s.parse().ok()).unwrap_or(64)];

    let stop = Arc::new(AtomicBool::new(false));
    let ops = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let latencies = Arc::new(Mutex::new(Vec::<u64>::new()));
    let start = Instant::now();

    let mut handles = Vec::with_capacity(threads);
    for index in 0..threads {
        let stop = Arc::clone(&stop);
        let ops = Arc::clone(&ops);
        let errors = Arc::clone(&errors);
        let latencies = Arc::clone(&latencies);
        let payload = payload.clone();
        handles.push(std::thread::spawn(move || {
            let mut local_latencies = Vec::new();
            let mut local_ops = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let started = Instant::now();
                let result = match mode {
                    LoadMode::Status => status_once(addr).inspect(|_| {
                        local_latencies.push(started.elapsed().as_micros() as u64);
                    }),
                    LoadMode::Login => login_once(addr, index).inspect(|_| {
                        local_latencies.push(started.elapsed().as_micros() as u64);
                    }),
                    LoadMode::Relay => {
                        relay_once(addr, index, &payload, &stop, &mut local_latencies)
                    }
                };
                match result {
                    Ok(count) => {
                        local_ops += count;
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
            ops.fetch_add(local_ops, Ordering::Relaxed);
            latencies.lock().unwrap().extend(local_latencies);
        }));
    }

    std::thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = handle.join();
    }
    let elapsed = start.elapsed().as_secs_f64();
    let total_ops = ops.load(Ordering::Relaxed);
    let total_errors = errors.load(Ordering::Relaxed);
    let mut samples = latencies.lock().unwrap().clone();
    samples.sort_unstable();
    let avg = if samples.is_empty() {
        0
    } else {
        samples.iter().sum::<u64>() / samples.len() as u64
    };
    let p50 = samples.get(samples.len() / 2).copied().unwrap_or(0);
    let p99 = samples.get(samples.len() * 99 / 100).copied().unwrap_or(0);
    let mode_name = match mode {
        LoadMode::Status => "status",
        LoadMode::Login => "login",
        LoadMode::Relay => "relay",
    };
    println!(
        "mode={mode_name} addr={addr} threads={threads} seconds={elapsed:.2} ops={total_ops} errors={total_errors} rate={:.0}/s avg={avg}us p50={p50}us p99={p99}us{}",
        total_ops as f64 / elapsed,
        if matches!(mode, LoadMode::Relay) {
            format!(" payload={}B", payload.len())
        } else {
            String::new()
        }
    );
    Ok(())
}

fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

fn status_once(addr: SocketAddr) -> io::Result<u64> {
    let mut peer = Peer::new(connect(addr)?)?;
    let handshake = Handshake {
        protocol_version: PROTOCOL,
        server_address: "localhost".into(),
        server_port: 25565,
        next_state: NextState::Status,
    };
    peer.send(0x00, &handshake.encode().into_inner())?;
    peer.send(0x00, &[])?;
    let (id, data) = peer.recv()?;
    if id != 0x00 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected status",
        ));
    }
    let mut reader = Reader::new(&data);
    let _json = reader.read_string(32_767).map_err(pio)?;
    Ok(1)
}

fn login_once(addr: SocketAddr, index: usize) -> io::Result<u64> {
    let mut peer = Peer::new(connect(addr)?)?;
    login(&mut peer, index)?;
    Ok(1)
}

fn relay_once(
    addr: SocketAddr,
    index: usize,
    payload: &[u8],
    stop: &AtomicBool,
    latencies: &mut Vec<u64>,
) -> io::Result<u64> {
    let mut peer = Peer::new(connect(addr)?)?;
    login(&mut peer, index)?;
    let mut ops = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        peer.send(0x33, payload)?;
        let (id, data) = peer.recv()?;
        if id != 0x33 || data != payload {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad echo"));
        }
        latencies.push(started.elapsed().as_micros() as u64);
        ops += 1;
    }
    Ok(ops)
}

fn login(peer: &mut Peer, index: usize) -> io::Result<()> {
    let name = format!("Bench{index}{:04x}", std::process::id() & 0xffff);
    let handshake = Handshake {
        protocol_version: PROTOCOL,
        server_address: "localhost".into(),
        server_port: 25565,
        next_state: NextState::Login,
    };
    peer.send(0x00, &handshake.encode().into_inner())?;
    peer.send(
        serverbound::LOGIN_START,
        &login::encode_login_start(&name, None, PROTOCOL),
    )?;
    loop {
        let (id, data) = peer.recv()?;
        match id {
            clientbound::SET_COMPRESSION => {
                let threshold =
                    login::decode_set_compression(&Packet::new(id, data)).map_err(pio)?;
                peer.compression = Some(threshold);
            }
            clientbound::LOGIN_SUCCESS => {
                peer.send(serverbound::LOGIN_ACKNOWLEDGED, &[])?;
                return Ok(());
            }
            clientbound::DISCONNECT => {
                let reason = login::decode_disconnect_reason(&Packet::new(id, data));
                return Err(io::Error::other(reason));
            }
            _ => {}
        }
    }
}
