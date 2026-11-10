---
name: mc-proxy-fstack
description: >-
  Build, run, test, and extend the mc-proxy workspace: a Velocity-compatible
  Minecraft Java Edition reverse proxy (modern forwarding, online mode, status,
  multi-worker) and tokio-fstack, a Tokio-compatible TcpListener/TcpStream/UdpSocket
  library for F-Stack/DPDK and plain Linux. Use when adding Minecraft version
  support, changing proxy/protocol behavior, scaling workers, deploying on
  F-Stack/DPDK, or debugging login/forwarding/compression issues.
---

# mc-proxy / tokio-fstack

## Deliverables

- **`tokio-fstack`**: Tokio-compatible `TcpListener`, `TcpStream` and
  `UdpSocket` on top of either kernel sockets or F-Stack (`ff_*` /
  `ff_epoll_*`). One reactor abstraction covers both backends. Includes
  per-worker reactors, `SO_REUSEPORT` listeners and a lock-free ring-buffer
  `WorkerBus` for cross-worker messages.
- **`mc-proxy`**: Velocity-compatible reverse proxy with modern forwarding
  (HMAC-SHA256), online mode (RSA / AES-CFB8 / `hasJoined`), server-list ping
  (MOTD / favicon / legacy ping), compression, version-agnostic relay for
  1.13+, `forced_hosts` / `try` routing, and multi-worker scaling.
- **Verification**: 68 tests (unit / E2E / socket / doc) plus one
  network-dependent test. Validated end-to-end against **real Paper 1.21.4**
  (login -> configuration -> play). The F-Stack backend is built and run with
  DPDK 24.11.8 + F-Stack v2.0 on the `net_tap` PMD (server-list ping answered
  through `ff_socket`/`ff_epoll_*`).
- **Docs**: `README.md`, `docs/{architecture,workers,fstack,protocol}.md`,
  this `SKILL.md`, and a version-upgrade prompt.

## When to use this skill

- Configure, run or deploy `mc-proxy` on kernel sockets or F-Stack.
- Add support for a new Minecraft Java Edition version.
- Change Velocity forwarding, online mode, status, compression, or relay
  behavior.
- Change the `tokio-fstack` reactor, sockets, workers, or F-Stack backend.
- Debug login failures, backend connection problems, forwarding/HMAC errors,
  or compression/framing errors.

## Repository layout

```
crates/tokio-fstack/          Tokio-compatible sockets + reactor + workers + bus
  src/sys.rs                  non-blocking syscalls (libc / ff_*)
  src/ops.rs                  Op / OpFuture / cancellation-safe protocol
  src/reactor.rs              Platform / ScheduledIo / per-worker reactor
  src/worker.rs               WorkerConfig / spawn_worker / WorkerBus
  src/stream.rs, listener.rs, udp.rs
  src/poll_io.rs              EAGAIN -> readiness -> retry
  build.rs                    F-Stack/DPDK link flags
  examples/echo_{server,client}.rs
  tests/socket.rs             reactor integration tests
crates/mc-proxy/              Velocity-compatible proxy
  src/protocol/               VarInt, framing, compression, cipher, packets
  src/proxy/                  session, backend, status, worker pool
  src/forwarding.rs           Velocity modern forwarding
  src/crypto.rs               RSA / AES-CFB8 / offline UUID / server hash
  src/auth.rs                 hasJoined (rustls + hyper over tokio-fstack)
  src/config.rs, servers.rs, sockets.rs
  examples/bench.rs           status/login/relay load generator + fake backend
  tests/e2e.rs                fake Paper + fake client E2E
  config.example.toml
docs/architecture.md          reactor / thread model / buffer ownership
docs/workers.md               worker model / native-mt / ring buffer
docs/fstack.md                F-Stack/DPDK deployment
docs/protocol.md              protocol / forwarding details
docs/benchmark.md             kernel vs F-Stack benchmark results
prompts/minecraft-version-upgrade.md
scripts/paper-smoke-client.py real-Paper smoke-test client
scripts/build-fstack.sh       F-Stack/DPDK build
scripts/run-fstack-net-tap.sh F-Stack net_tap runtime test
```

## Quick commands

```bash
cargo build --release -p mc-proxy
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo check -p mc-proxy --features fstack --all-targets

# Configuration
./target/release/mc-proxy --print-example > config.toml
./target/release/mc-proxy gen-secret
./target/release/mc-proxy --config config.toml --check
./target/release/mc-proxy --config config.toml
```

### F-Stack build

```bash
FSTACK_LIB_DIR=/usr/local/lib \
PKG_CONFIG_PATH=/usr/local/lib/pkgconfig \
cargo build --release -p mc-proxy --features fstack
```

F-Stack needs root, hugepages and a NIC bound to DPDK. See
[docs/fstack.md](docs/fstack.md).

## Architecture invariants (do not break)

1. **F-Stack thread affinity**: `ff_*` may only be called from the thread that
   runs `ff_init`/`ff_run`, and those calls must happen on the **process main
   thread**. `run_with_stack` does `ff_init` on main, runs the app on a separate
   thread, and enters `ff_run` on main; every socket syscall is queued back to
   that thread. Never call `ff_*` from Tokio workers.
2. **Worker affinity**: a socket belongs to the worker/reactor that created it.
   Do not move sockets between workers (the proxy pins each connection to one
   worker).
3. **Cross-worker state uses the ring buffer**: player join/leave events are
   replicated through `WorkerBus` (lock-free `ArrayQueue`), not a shared mutex
   over the data plane.
4. **Play/Configuration packets are relayed opaquely**: do not add
   version-specific packet parsing to the relay. Version-dependent handling is
   limited to handshake, status, login and forwarding.
5. **Buffer lifetime**: dropping an `OpFuture` waits for the in-flight syscall
   to finish. Breaking this protocol reintroduces use-after-free.
6. **Readiness is level-triggered and disarmed after delivery**; readiness bits
   are consumed on wake. Otherwise `EAGAIN` spins.
7. **Never weaken authentication/forwarding**: keep HMAC-SHA256,
   server-hash verification, RSA/AES-CFB8 and session-server checks intact.
8. **Offline UUIDs are derived from the name**; do not trust the UUID sent by
   the client.

## Core APIs

### tokio-fstack

```rust
use tokio::io::{AsyncReadExt, AsyncWriteExt};

let listener = tokio_fstack::TcpListener::bind("0.0.0.0:25565").await?;
let (mut stream, peer) = listener.accept().await?;
stream.write_all(b"hello").await?;

let socket = tokio_fstack::UdpSocket::bind("0.0.0.0:25566").await?;
let (n, peer) = socket.recv_from(&mut buf).await?;
socket.send_to(&buf[..n], peer).await?;
```

- Implements `AsyncRead` / `AsyncWrite`, `into_split`, `bind_reuseport`.
- Workers:

```rust
let handle = tokio_fstack::spawn_worker(
    tokio_fstack::WorkerConfig::new(0).with_core(0),
    || async { /* accept loop */ },
)?;
handle.join()?;
```

- Bus:

```rust
let (bus, receivers) = tokio_fstack::WorkerBus::<Msg>::new(workers, 4096);
bus.sender().broadcast_from(worker_id, msg);
```

### mc-proxy configuration (excerpt)

| Key | Default | Description |
|---|---|---|
| `bind` | `0.0.0.0:25565` | Listen address |
| `online_mode` | `false` | Mojang session authentication |
| `workers` | `1` | Worker count (2+ on the standard backend) |
| `pin_cores` | `true` | Pin workers to CPU cores |
| `compression_threshold` | `256` | `-1` disables compression |
| `forwarding_secret` | `""` | Velocity modern forwarding secret |
| `status.mode` | `proxy` | `proxy` or `passthrough` |
| `[fstack] eal_args` | `[]` | Arguments passed to `ff_init` |

### Paper side

```yaml
# config/paper-global.yml
proxies:
  velocity:
    enabled: true
    online-mode: true
    secret: "<same as the proxy's forwarding_secret>"
```

```properties
online-mode=false
server-ip=127.0.0.1
```

## Recipes

### 1. Add support for a new Minecraft version

Read [docs/protocol.md](docs/protocol.md) and use the detailed prompt in
[prompts/minecraft-version-upgrade.md](prompts/minecraft-version-upgrade.md)
(currently written in Japanese). Essential steps:

- Confirm the new protocol number and the login/configuration changes in the
  official protocol docs.
- Check `protocol/login.rs` packet IDs and `LoginStart::decode`.
- Check the Velocity forwarding payload and the `velocity:player_info`
  version accepted by Paper.
- Update `PROTOCOL` in `tests/e2e.rs` and in `scripts/paper-smoke-client.py`.
- Validate against a real Paper server up to the play state.
- Do not touch the play-state relay unless framing/compression itself changed.

### 2. Run multiple workers

```toml
workers = 4
pin_cores = true
```

- Standard backend only. Each worker binds with `SO_REUSEPORT`.
- Player state is replicated per worker through the ring-buffer bus.
- F-Stack uses one stack per process: run one process per core with
  `workers = 1` (see [docs/workers.md](docs/workers.md)).

### 3. Benchmark both backends

```bash
cargo build --release --example bench -p mc-proxy
./target/release/examples/bench backend 0.0.0.0:25566 bench-secret &
./target/release/examples/bench status <proxy-addr> 5 8
./target/release/examples/bench login  <proxy-addr> 5 8
./target/release/examples/bench relay  <proxy-addr> 5 8 64
```

Measured on net_tap (1 core): status 3,150 (kernel) vs 2,918 (F-Stack) conn/s;
relay 64B 18,077 vs 5,800 msg/s. net_tap adds four tap crossings per relay
message and the single stack thread serialises all syscalls; this is not a
measurement of DPDK with a real NIC. See `docs/benchmark.md`.

Tuning that matters for small-packet proxy traffic: `pkt_tx_delay=0`,
`idle_loop_interval=0`, `net.inet.tcp.delayed_ack=0`, 64 KiB send/recv
buffers (login +81% on 2 cores). Scale F-Stack by running **one process per
core** (separate `lcore_mask`, `file_prefix`, tap/IP): two processes reached
5,925 status/s (~2x one process). Cross-process player state is not shared
yet; `rte_ring`/`ff_ipc` or native-mt per-lcore dispatch is future work.

### 4. Debug a failed login

1. Run the proxy with `RUST_LOG=debug` and inspect `session ended` /
   `backend login failed`.
2. Inspect the login `Disconnect` reason:
   - `Outdated server!` -- client and Paper versions differ.
   - `Unable to verify player details` / forwarding error -- secret mismatch or
     `velocity.enabled` missing on Paper.
   - `backend is in online-mode` -- set `online-mode=false` on Paper.
3. Reproduce locally with `scripts/paper-smoke-client.py`.

### 5. Compression / relay issues

- The client-side and backend-side thresholds are independent and re-encoded
  per packet.
- `protocol/frame.rs` enforces an 8 MiB limit.
- Use `compressed_large_packet_relays` as a regression test.

### 6. Switch to F-Stack

- Build with `--features fstack`.
- Configure `[fstack] eal_args`.
- Hugepages / NIC binding / privileges: [docs/fstack.md](docs/fstack.md).
- One stack per process; scale with multiple processes + `SO_REUSEPORT`.

## Verification checklist

| Goal | Command / method |
|---|---|
| Unit + E2E | `cargo test --workspace` |
| Formatting | `cargo fmt --all -- --check` |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` |
| F-Stack compile | `cargo check -p mc-proxy --features fstack --all-targets` |
| F-Stack build | `FSTACK_LIB_DIR=/usr/local/lib cargo build --release -p mc-proxy --features fstack` |
| F-Stack runtime | `sudo ./scripts/run-fstack-net-tap.sh` (status ping through F-Stack) |
| Real Paper | Paper 1.21.x + `scripts/paper-smoke-client.py` |
| Mojang HTTP/TLS | `cargo test -p mc-proxy --lib real_session_server -- --ignored` |
| Release binary | `mc-proxy --check`, start, server-list ping |
| Benchmark | `cargo build --release --example bench` + `bench status/login/relay` |

Current test count: 68, plus one network-dependent test (`--ignored`).

## Pitfalls

- **F-Stack threads**: `ff_init`/`ff_run` must run on the main thread; calling
  them from a spawned thread crashes in `ff_freebsd_init`. Use `run_with_stack`.
- **Linker sets**: F-Stack boot hooks live in ELF linker-set sections; the
  build scripts add `--no-gc-sections`. Without it `vnet0` stays uninitialised
  and `ff_freebsd_init` segfaults at `V_tcp_do_ecn`.
- **net_tap MAC**: net_tap gives the tap the same MAC as the DPDK port, which
  breaks ARP. Set a different MAC on the kernel tap (see
  `scripts/run-fstack-net-tap.sh`).
- **Worker affinity**: using a socket from another worker routes I/O to the
  wrong stack. Keep each connection on one worker.
- **Status protocol**: the proxy reports the client's protocol version; it
  cannot know whether the backend supports it.
- **Login Success is not parsed**: it is relayed verbatim to absorb version
  layout differences.
- **Duplicate player names in tests**: offline UUIDs collide and the replica
  deduplicates them. Use unique names in multi-worker tests.
- **`workers > 1` on F-Stack**: fails at startup by design.
- **DNS**: kernel DNS is unavailable on a DPDK-only host; prefer IP literals.

## Limitations

- No `/server` switching.
- No legacy BungeeCord forwarding.
- No cookie request/response relay during login.
- `max_players` is cosmetic.
- Client and backend versions must match.
- F-Stack native-mt (`thread_mode=1`) is implemented behind
  `[fstack] native_mt = true`: per-lcore reactor/queue/epoll, `workers = N`,
  stack cores 0..N-1 and app cores N..2N-1. It is experimental: on net_tap the
  multi-queue flow affinity breaks F-Stack-initiated backend connections
  (login/relay time out), and status does not scale. Use one process per core
  with `SO_REUSEPORT` on net_tap/production until validated on a physical NIC
  with RSS.

## References

- [Minecraft protocol](https://minecraft.wiki/w/Java_Edition_protocol)
- [Protocol version numbers](https://minecraft.wiki/w/Java_Edition_protocol/Protocol_version_numbers)
- [Velocity player information forwarding](https://docs.papermc.io/velocity/dev/player-information-forwarding/)
- [Velocity `PlayerDataForwarding.java`](https://github.com/PaperMC/Velocity/blob/dev/3.0.0/proxy/src/main/java/com/velocitypowered/proxy/connection/PlayerDataForwarding.java)
- [Paper `VelocityProxy.java`](https://github.com/PaperMC/Paper/blob/main/paper-server/src/main/java/com/destroystokyo/paper/proxy/VelocityProxy.java)
- [F-Stack](https://f-stack.org/) / [F-Stack GitHub](https://github.com/F-Stack/f-stack)
