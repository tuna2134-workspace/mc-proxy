//! Per-core workers and cross-worker messaging.
//!
//! A [`Worker`](spawn_worker) owns
//!
//! * an independent reactor (its own epoll instance on the standard backend;
//!   the `fstack` backend is single-stack and does not support `workers > 1`),
//! * a Tokio `current_thread` runtime running on a dedicated thread pinned to
//!   a CPU core, and
//! * an inbox of a [`WorkerBus`] ring buffer for messages from other workers.
//!
//! # F-Stack mapping
//!
//! F-Stack's native multi-thread mode (`thread_mode=1`) gives every lcore
//! thread its own stack instance (VNET isolation) and its own RX/TX queue, but
//! the current integration still creates one stack per process: `workers > 1`
//! is therefore rejected on the `fstack` backend. Scale F-Stack with one
//! process per core and `SO_REUSEPORT` (see `docs/workers.md`); a per-lcore
//! dispatch for `thread_mode=1` is a future step.
//!
//! Sockets are *worker affine*: they are created on, and their I/O operations
//! run on, the platform of the worker thread that created them. Do not move a
//! socket to another worker.

use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;

use crossbeam_queue::ArrayQueue;

use crate::reactor;

/// Configuration for one worker.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// Worker index (`0..workers`). Used as the bus address.
    pub id: usize,
    /// CPU core to pin the worker thread to (index into the available cores).
    /// `None` leaves the thread unpinned.
    pub core: Option<usize>,
    /// F-Stack EAL arguments (only used by the `fstack` backend, and only for
    /// the same single stack instance as the process).
    pub eal_args: Vec<String>,
}

impl WorkerConfig {
    /// Create a configuration for worker `id`.
    pub fn new(id: usize) -> Self {
        Self {
            id,
            core: None,
            eal_args: Vec::new(),
        }
    }

    /// Pin the worker to a core (best effort).
    pub fn with_core(mut self, core: usize) -> Self {
        self.core = Some(core);
        self
    }

    /// Pass EAL arguments to `ff_init` (F-Stack backend only).
    pub fn with_eal_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.eal_args = args.into_iter().map(Into::into).collect();
        self
    }
}

/// Handle to a running worker.
pub struct WorkerHandle {
    id: usize,
    runtime: tokio::runtime::Handle,
    thread: Option<JoinHandle<()>>,
    done: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl WorkerHandle {
    /// Worker index.
    pub fn id(&self) -> usize {
        self.id
    }

    /// Spawn a task on this worker's runtime.
    ///
    /// Tasks run on the worker thread, so sockets they create belong to this
    /// worker.
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(future);
    }

    /// Wait for the worker's main future to finish without blocking the
    /// calling runtime.
    pub async fn wait(mut self) -> io::Result<()> {
        if let Some(done) = self.done.take() {
            let _ = done.await;
        }
        match self.thread.take() {
            Some(thread) => thread
                .join()
                .map_err(|_| io::Error::other("worker thread panicked")),
            None => Ok(()),
        }
    }

    /// Wait for the worker's main future to finish, blocking the current
    /// thread.
    pub fn join(mut self) -> io::Result<()> {
        match self.thread.take() {
            Some(thread) => thread
                .join()
                .map_err(|_| io::Error::other("worker thread panicked")),
            None => Ok(()),
        }
    }

    /// The worker's Tokio runtime handle.
    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }
}

/// Start a worker and run `main` on it.
///
/// `main` is invoked on the worker thread, after the worker's reactor has been
/// installed as the thread's current reactor. Everything created by the future
/// (including sockets) belongs to this worker.
pub fn spawn_worker<F, Fut>(config: WorkerConfig, main: F) -> io::Result<WorkerHandle>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let reactor_config = crate::Config {
        eal_args: config.eal_args.clone(),
    };
    let reactor = reactor::build_worker(reactor_config)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name(format!("tokio-fstack-worker-{}", config.id))
        .build()?;
    let runtime_handle = runtime.handle().clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let id = config.id;

    let thread = std::thread::Builder::new()
        .name(format!("tokio-fstack-worker-{}", config.id))
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            if let Some(core) = config.core {
                pin_to_core(core);
            }
            reactor::set_current(Some(Arc::clone(&reactor)));
            runtime.block_on(async move { main().await });
            reactor::set_current(None);
            let _ = done_tx.send(());
        })?;

    Ok(WorkerHandle {
        id,
        runtime: runtime_handle,
        thread: Some(thread),
        done: Some(done_rx),
    })
}

pub(crate) fn pin_to_core(index: usize) {
    let Some(cores) = core_affinity::get_core_ids() else {
        return;
    };
    if cores.is_empty() {
        return;
    }
    let core = cores[index % cores.len()];
    // Best effort: containers with restricted cpusets may refuse.
    let _ = core_affinity::set_for_current(core);
}

// ---------------------------------------------------------------------------
// Cross-worker ring buffer bus
// ---------------------------------------------------------------------------

struct Inbox<T> {
    queue: ArrayQueue<T>,
    waker: Mutex<Option<Waker>>,
}

impl<T> Inbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity.max(1)),
            waker: Mutex::new(None),
        }
    }

    fn push(&self, message: T) -> Result<(), T> {
        self.queue.push(message)?;
        let waker = self.waker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }
}

/// A bus of lock-free ring buffers, one per worker.
///
/// Messages are addressed to a single worker (point-to-point) or broadcast to
/// every worker. The ring buffer itself is `crossbeam_queue::ArrayQueue`; for
/// F-Stack multi-process deployments this maps directly onto DPDK `rte_ring`
/// (see `docs/workers.md`).
pub struct WorkerBus<T> {
    inboxes: Vec<Arc<Inbox<T>>>,
}

impl<T> WorkerBus<T> {
    /// Create a bus for `workers` workers, each with an inbox of
    /// `capacity` messages.
    pub fn new(workers: usize, capacity: usize) -> (Arc<Self>, Vec<WorkerBusReceiver<T>>) {
        let inboxes: Vec<Arc<Inbox<T>>> = (0..workers)
            .map(|_| Arc::new(Inbox::new(capacity)))
            .collect();
        let receivers = inboxes
            .iter()
            .map(|inbox| WorkerBusReceiver {
                inbox: Arc::clone(inbox),
            })
            .collect();
        (Arc::new(Self { inboxes }), receivers)
    }

    /// A cheap sender handle.
    pub fn sender(&self) -> WorkerBusSender<T>
    where
        T: Send + 'static,
    {
        WorkerBusSender {
            inboxes: self.inboxes.iter().map(Arc::clone).collect(),
        }
    }

    /// Number of workers served by this bus.
    pub fn workers(&self) -> usize {
        self.inboxes.len()
    }
}

/// Sending side of a [`WorkerBus`].
pub struct WorkerBusSender<T> {
    inboxes: Vec<Arc<Inbox<T>>>,
}

impl<T: Send + 'static> WorkerBusSender<T> {
    /// Send `message` to worker `to`.
    ///
    /// Returns the message back when the target's inbox is full.
    pub fn send(&self, to: usize, message: T) -> Result<(), T> {
        match self.inboxes.get(to) {
            Some(inbox) => inbox.push(message),
            None => Err(message),
        }
    }

    /// Send a clone of `message` to every worker.
    pub fn broadcast(&self, message: T)
    where
        T: Clone,
    {
        for inbox in &self.inboxes {
            let _ = inbox.push(message.clone());
        }
    }

    /// Send a clone of `message` to every worker except `from`.
    pub fn broadcast_from(&self, from: usize, message: T)
    where
        T: Clone,
    {
        for (index, inbox) in self.inboxes.iter().enumerate() {
            if index != from {
                let _ = inbox.push(message.clone());
            }
        }
    }

    /// Send to worker `to` without copying when it is the only recipient.
    pub fn try_send(&self, to: usize, message: T) -> Option<T> {
        match self.inboxes.get(to) {
            Some(inbox) => inbox.push(message).err(),
            None => Some(message),
        }
    }
}

impl<T> Clone for WorkerBusSender<T> {
    fn clone(&self) -> Self {
        Self {
            inboxes: self.inboxes.clone(),
        }
    }
}

/// Receiving side of a [`WorkerBus`].
pub struct WorkerBusReceiver<T> {
    inbox: Arc<Inbox<T>>,
}

impl<T> WorkerBusReceiver<T> {
    /// Take the next message, if one is queued.
    pub fn try_recv(&self) -> Option<T> {
        self.inbox.queue.pop()
    }

    /// Poll for a message, registering the current task as a waker.
    ///
    /// This is cancel safe: a message is only removed when it is returned.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<T> {
        if let Some(message) = self.inbox.queue.pop() {
            return Poll::Ready(message);
        }
        {
            let mut waker = self.inbox.waker.lock().unwrap_or_else(|e| e.into_inner());
            *waker = Some(cx.waker().clone());
        }
        if let Some(message) = self.inbox.queue.pop() {
            return Poll::Ready(message);
        }
        Poll::Pending
    }

    /// Wait for the next message.
    pub async fn recv(&self) -> T {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }
}

impl<T> Clone for WorkerBusReceiver<T> {
    fn clone(&self) -> Self {
        Self {
            inbox: Arc::clone(&self.inbox),
        }
    }
}

/// Whether a worker pool with more than one worker can run on the active
/// backend.
///
/// The standard backend always supports it. The current F-Stack integration
/// creates one stack instance per reactor and therefore supports one worker per
/// process; multi-core F-Stack deployments run one process per core with
/// `SO_REUSEPORT`, as described in `docs/workers.md`.
pub fn multi_worker_supported() -> bool {
    !cfg!(feature = "fstack")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bus_delivers_and_wakes() {
        let (bus, mut receivers) = WorkerBus::new(2, 8);
        let sender = bus.sender();
        let receiver = receivers.pop().unwrap();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                    .await
                    .expect("message should arrive")
            })
        });
        std::thread::sleep(Duration::from_millis(50));
        sender.send(1, 42u32).unwrap();
        assert_eq!(handle.join().unwrap(), 42);
    }

    #[test]
    fn bus_broadcast_reaches_every_worker() {
        let (bus, receivers) = WorkerBus::<u32>::new(3, 4);
        let sender = bus.sender();
        sender.broadcast(7);
        for receiver in receivers {
            assert_eq!(receiver.try_recv(), Some(7));
        }
    }

    #[test]
    fn full_inbox_returns_the_message() {
        let (bus, _receivers) = WorkerBus::<u32>::new(1, 1);
        let sender = bus.sender();
        assert!(sender.send(0, 1).is_ok());
        assert_eq!(sender.send(0, 2), Err(2));
    }

    #[test]
    fn bus_across_two_workers_delivers_everything() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (bus, mut receivers) = WorkerBus::<u32>::new(2, 64);
        let receiver = receivers.remove(0);
        let counter = Arc::new(AtomicUsize::new(0));
        let worker_counter = Arc::clone(&counter);
        let worker = spawn_worker(WorkerConfig::new(0), move || async move {
            tokio::spawn(async move {
                loop {
                    let _ = receiver.recv().await;
                    worker_counter.fetch_add(1, Ordering::Relaxed);
                }
            });
            std::future::pending::<()>().await;
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let sender = bus.sender();
        for index in 0..9u32 {
            sender.send(0, index).unwrap();
        }
        for _ in 0..100 {
            if counter.load(Ordering::Relaxed) == 9 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(counter.load(Ordering::Relaxed), 9);
        drop(worker);
    }

    #[test]
    fn worker_runs_sockets_and_tasks() {
        use std::io::{Read, Write};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let handle = spawn_worker(WorkerConfig::new(0).with_core(0), move || async move {
            let listener = crate::TcpListener::bind("127.0.0.1:0").await.unwrap();
            addr_tx.send(listener.local_addr().unwrap()).unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let (mut read, mut write) = stream.into_split();
            let mut buf = [0u8; 4];
            read.read_exact(&mut buf).await.unwrap();
            write.write_all(&buf).await.unwrap();
        })
        .unwrap();

        let addr = addr_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
        handle.join().unwrap();
    }
}
