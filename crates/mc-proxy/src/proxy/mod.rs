//! The proxy: shared state, worker pool, listener and per-connection sessions.
//!
//! # Worker model
//!
//! ```text
//! CPU core 0                         CPU core 1
//! ┌─────────────────────────┐        ┌─────────────────────────┐
//! │ reactor / stack thread  │        │ reactor / stack thread  │
//! │  ├─ RX/TX queue 0       │        │  ├─ RX/TX queue 1       │
//! │  └─ TCP connections     │        │  └─ TCP connections     │
//! │ Tokio current_thread    │        │ Tokio current_thread    │
//! │  └─ proxy worker 0      │        │  └─ proxy worker 1      │
//! │      ▲ ring buffer bus  │◀──────▶│      ▼ ring buffer bus  │
//! └─────────────────────────┘        └─────────────────────────┘
//! ```
//!
//! Each worker owns an independent reactor and Tokio runtime, binds the
//! listener with `SO_REUSEPORT`, and serves its own connections. Player
//! join/leave events are replicated to the other workers through lock-free
//! ring buffers ([`tokio_fstack::WorkerBus`]); the server-list sample and
//! player count each worker reports are therefore cluster-wide.

pub mod backend;
pub mod session;
pub mod status;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;
use uuid::Uuid;

use crate::auth::SessionAuthenticator;
use crate::config::{Config, ConfigError};
use crate::crypto::{CryptoError, ServerKey};
use crate::servers::ServerRegistry;
use crate::sockets::TcpListener;
use tokio_fstack::{WorkerBus, WorkerBusSender, WorkerConfig, WorkerHandle};

/// Counters exposed for logging and tests.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Accepted TCP connections.
    pub connections: AtomicU64,
    /// Successful logins.
    pub logins: AtomicU64,
    /// Sessions that ended with an error.
    pub errors: AtomicU64,
    /// Packets relayed from client to backend.
    pub packets_from_clients: AtomicU64,
    /// Packets relayed from backend to client.
    pub packets_from_backends: AtomicU64,
    /// Bytes accepted on the listener (approximate, payload bytes).
    pub bytes_from_clients: AtomicU64,
    /// Bytes sent to clients (approximate, payload bytes).
    pub bytes_to_clients: AtomicU64,
}

/// State shared by every worker (immutable after start-up).
pub struct Immutable {
    /// Effective configuration.
    pub config: Config,
    /// Backend servers and routing.
    pub servers: ServerRegistry,
    /// Velocity forwarding secret (empty when forwarding is disabled).
    pub forwarding_secret: Vec<u8>,
    /// RSA key pair used for online-mode encryption.
    pub key: Option<ServerKey>,
    /// Session server client used for online mode.
    pub authenticator: Option<SessionAuthenticator>,
    /// Favicon as a `data:` URI.
    pub favicon: Option<String>,
    /// Metrics (shared by all workers).
    pub metrics: Metrics,
}

/// A cluster event replicated between workers.
#[derive(Clone, Debug)]
pub enum ClusterMessage {
    /// A player logged in on a worker.
    Joined {
        /// Originating worker.
        worker: usize,
        /// Player name.
        name: String,
        /// Player UUID.
        uuid: Uuid,
    },
    /// A player disconnected from a worker.
    Left {
        /// Originating worker.
        worker: usize,
        /// Player UUID.
        uuid: Uuid,
    },
}

/// Per-worker state.
pub struct Shared {
    inner: Arc<Immutable>,
    /// Worker index.
    pub worker_id: usize,
    /// Worker-local replicated view of connected players (name, uuid).
    pub players: Mutex<Vec<(String, Uuid)>>,
    /// Worker-local replicated player count.
    pub online: AtomicUsize,
    /// Bus used to replicate events to the other workers.
    pub bus: Option<WorkerBusSender<ClusterMessage>>,
}

impl std::ops::Deref for Shared {
    type Target = Immutable;

    fn deref(&self) -> &Immutable {
        &self.inner
    }
}

impl Shared {
    /// Create the state for worker `worker_id`.
    pub fn new(
        inner: Arc<Immutable>,
        worker_id: usize,
        bus: Option<WorkerBusSender<ClusterMessage>>,
    ) -> Self {
        Self {
            inner,
            worker_id,
            players: Mutex::new(Vec::new()),
            online: AtomicUsize::new(0),
            bus,
        }
    }

    /// Whether Velocity modern forwarding is configured.
    pub fn forwarding_enabled(&self) -> bool {
        !self.forwarding_secret.is_empty()
    }

    /// Current number of players known to this worker.
    pub fn online_players(&self) -> usize {
        self.online.load(Ordering::Relaxed)
    }

    /// Apply an event replicated from another worker.
    pub fn apply_cluster_message(&self, message: ClusterMessage) {
        tracing::debug!(worker = self.worker_id, ?message, "cluster event");
        match message {
            ClusterMessage::Joined { name, uuid, .. } => {
                let mut players = self.players.lock().unwrap_or_else(|e| e.into_inner());
                if !players.iter().any(|(_, id)| *id == uuid) {
                    players.push((name, uuid));
                    self.online.fetch_add(1, Ordering::Relaxed);
                }
            }
            ClusterMessage::Left { uuid, .. } => {
                let mut players = self.players.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(index) = players.iter().position(|(_, id)| *id == uuid) {
                    players.remove(index);
                    self.online.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// Errors that can occur while starting the proxy.
#[derive(Debug, Error)]
pub enum ProxyError {
    /// Invalid configuration.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Socket error.
    #[error("socket error: {0}")]
    Io(#[from] std::io::Error),
    /// Cryptography error.
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
}

/// A bound proxy, ready to accept connections.
pub struct Proxy {
    /// Single-worker mode listener (owned by the calling runtime).
    listener: Option<TcpListener>,
    shared: Arc<Shared>,
    local_addr: SocketAddr,
    /// Worker handles (multi-worker mode).
    workers: Vec<WorkerHandle>,
    /// Per-worker views (one entry per worker; just one in single mode).
    views: Vec<Arc<Shared>>,
}

impl Proxy {
    /// Bind the listener(s) and prepare all shared state.
    pub async fn bind(config: Config) -> Result<Self, ProxyError> {
        config.validate()?;
        let immutable = Arc::new(build_immutable(&config)?);
        let workers = config.workers.max(1);

        if workers > 1 {
            if !tokio_fstack::multi_worker_supported() {
                return Err(ConfigError::Invalid(
                    "workers > 1 is not supported by the single-stack F-Stack backend; \
                     run one process per core with workers = 1 and SO_REUSEPORT \
                     (see docs/workers.md)"
                        .into(),
                )
                .into());
            }
            return Self::bind_workers(config, immutable).await;
        }

        crate::sockets::init(&config.fstack.eal_args)?;
        // SO_REUSEPORT also lets several F-Stack processes share the address
        // (one worker per process on stock F-Stack).
        let listener = TcpListener::bind_reuseport(&config.bind).await?;
        let local_addr = listener.local_addr()?;
        let shared = Arc::new(Shared::new(immutable, 0, None));
        let views = vec![Arc::clone(&shared)];
        Ok(Self {
            listener: Some(listener),
            shared,
            local_addr,
            workers: Vec::new(),
            views,
        })
    }

    async fn bind_workers(config: Config, immutable: Arc<Immutable>) -> Result<Self, ProxyError> {
        let count = config.workers.max(1);
        let (bus, receivers) = WorkerBus::<ClusterMessage>::new(count, 4096);
        let bus = Arc::new(bus);
        let mut handles = Vec::with_capacity(count);
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();

        // Worker 0 binds first so that a configured port of 0 resolves to a
        // concrete port which the other workers can share via SO_REUSEPORT.
        let shared0 = Arc::new(Shared::new(Arc::clone(&immutable), 0, Some(bus.sender())));
        let first_shared = Arc::clone(&shared0);
        let mut views = vec![Arc::clone(&shared0)];
        let mut receivers = receivers.into_iter();
        let receiver0 = receivers.next().expect("bus provides one inbox per worker");
        handles.push(start_worker(
            &config,
            0,
            config.bind.clone(),
            shared0,
            receiver0,
            Some(addr_tx),
        )?);

        let local_addr = addr_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "worker 0 did not report its listener address",
            )
        })?;

        for (id, receiver) in receivers.enumerate() {
            let id = id + 1;
            let shared = Arc::new(Shared::new(Arc::clone(&immutable), id, Some(bus.sender())));
            views.push(Arc::clone(&shared));
            handles.push(start_worker(
                &config,
                id,
                local_addr.to_string(),
                shared,
                receiver,
                None,
            )?);
        }

        Ok(Self {
            listener: None,
            shared: first_shared,
            local_addr,
            workers: handles,
            views,
        })
    }

    /// The address the proxy is listening on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Shared state of the first worker.
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Per-worker views (for diagnostics and tests).
    pub fn worker_views(&self) -> &[Arc<Shared>] {
        &self.views
    }

    /// Serve connections (single worker) or join the worker threads.
    pub async fn run(mut self) -> std::io::Result<()> {
        if let Some(listener) = self.listener.take() {
            return serve(listener, Arc::clone(&self.shared)).await;
        }
        for worker in self.workers.drain(..) {
            worker.wait().await?;
        }
        Ok(())
    }

    /// Spawn [`Proxy::run`] on the current runtime.
    pub fn spawn(self) -> tokio::task::JoinHandle<std::io::Result<()>> {
        tokio::spawn(self.run())
    }
}

/// Run the proxy in F-Stack native-mt mode (`thread_mode=1`).
///
/// One worker is started per lcore selected by `lcore_mask`; each worker has
/// its own F-Stack stack instance, reactor, epoll fd and SO_REUSEPORT
/// listener. `ff_init`/`ff_run` stay on the main thread.
#[cfg(feature = "fstack")]
pub fn run_fstack_multi(config: Config) -> Result<(), ProxyError> {
    config.validate()?;
    let count = config.workers.max(1);
    if count < 2 {
        return Err(ConfigError::Invalid(
            "run_fstack_multi requires workers >= 2; use Proxy::bind for one worker".into(),
        )
        .into());
    }
    if config.bind.ends_with(":0") {
        return Err(ConfigError::Invalid(
            "workers > 1 on F-Stack requires a concrete bind port (not :0)".into(),
        )
        .into());
    }

    let immutable = Arc::new(build_immutable(&config)?);
    let (bus, receivers) = WorkerBus::<ClusterMessage>::new(count, 4096);
    let bus = Arc::new(bus);
    let receivers = Arc::new(Mutex::new(receivers.into_iter()));
    let bind = config.bind.clone();
    let stack_config =
        tokio_fstack::Config::default().with_eal_args(config.fstack.eal_args.clone());

    tracing::info!(
        backend = "f-stack",
        workers = count,
        listen = %bind,
        "mc-proxy started (native-mt)"
    );

    tokio_fstack::run_with_stack_workers(count, stack_config, move |id| {
        let immutable = Arc::clone(&immutable);
        let bus = Arc::clone(&bus);
        let bind = bind.clone();
        let receiver = receivers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next()
            .expect("one inbox per worker");
        let shared = Arc::new(Shared::new(immutable, id, Some(bus.sender())));
        async move {
            let bus_shared = Arc::clone(&shared);
            tokio::spawn(async move {
                loop {
                    bus_shared.apply_cluster_message(receiver.recv().await);
                }
            });
            let listener = TcpListener::bind_reuseport(&bind).await?;
            tracing::info!(
                worker = id,
                listen = %listener.local_addr()?,
                "F-Stack worker listening"
            );
            serve(listener, shared).await
        }
    })?;
    Ok(())
}

/// Accept connections and spawn a session per connection.
pub(crate) async fn serve(listener: TcpListener, shared: Arc<Shared>) -> std::io::Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tracing::debug!(worker = shared.worker_id, %peer, "accepted connection");
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    session::handle(stream, peer, shared).await;
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, "accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

fn start_worker(
    config: &Config,
    id: usize,
    bind: String,
    shared: Arc<Shared>,
    receiver: tokio_fstack::WorkerBusReceiver<ClusterMessage>,
    addr_tx: Option<std::sync::mpsc::Sender<SocketAddr>>,
) -> Result<WorkerHandle, ProxyError> {
    let eal_args = config.fstack.eal_args.clone();
    let mut worker_config = WorkerConfig::new(id).with_eal_args(eal_args);
    if config.pin_cores {
        worker_config = worker_config.with_core(id);
    }
    let receiver = Mutex::new(Some(receiver));
    let handle = tokio_fstack::spawn_worker(worker_config, move || async move {
        let receiver = receiver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("worker inbox");
        let bus_shared = Arc::clone(&shared);
        tokio::spawn(async move {
            loop {
                let message = receiver.recv().await;
                bus_shared.apply_cluster_message(message);
            }
        });

        let listener = if id == 0 {
            // The first worker may bind a host name or port 0.
            TcpListener::bind_reuseport(bind.as_str()).await
        } else {
            match bind.parse::<SocketAddr>() {
                Ok(addr) => TcpListener::bind_reuseport_addr(addr).await,
                Err(err) => {
                    tracing::error!(worker = id, error = %err, "invalid bind address");
                    return;
                }
            }
        };
        let listener = match listener {
            Ok(listener) => listener,
            Err(err) => {
                tracing::error!(worker = id, error = %err, "worker failed to bind");
                return;
            }
        };
        if let Some(tx) = addr_tx {
            if let Ok(addr) = listener.local_addr() {
                let _ = tx.send(addr);
            }
        }
        if let Err(err) = serve(listener, shared).await {
            tracing::error!(worker = id, error = %err, "worker stopped");
        }
    })?;
    Ok(handle)
}

fn build_immutable(config: &Config) -> Result<Immutable, ProxyError> {
    let forwarding_secret = config.resolve_forwarding_secret()?;
    let favicon = config.load_favicon()?;
    let servers = ServerRegistry::from_config(config)?;
    let key = if config.online_mode {
        Some(ServerKey::generate()?)
    } else {
        None
    };
    let authenticator = if config.online_mode {
        Some(SessionAuthenticator::new(
            &config.auth.session_server,
            Duration::from_millis(config.auth.timeout_ms),
        )?)
    } else {
        None
    };
    Ok(Immutable {
        config: config.clone(),
        servers,
        forwarding_secret,
        key,
        authenticator,
        favicon,
        metrics: Metrics::default(),
    })
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy")
            .field("local_addr", &self.local_addr)
            .field("workers", &self.workers.len())
            .finish()
    }
}
