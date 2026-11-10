//! The readiness reactor and its two platform drivers.
//!
//! # Architecture
//!
//! ```text
//!   tokio task threads                       reactor / F-Stack thread
//!   ------------------                       ----------------------
//!   poll_read() ──submit Op──────────────────▶ execute syscall (non-blocking)
//!        ▲                                         │
//!        └──────── wake (Waker) ◀───────────────────┘
//!   poll_read() gets EAGAIN
//!        │
//!        ├── arm(READ) ──submit Interest──────────▶ epoll_ctl(MOD, EPOLLIN)
//!        │                                         │
//!        └──────── wake on readiness ◀────── epoll_wait / ff_epoll_wait
//! ```
//!
//! The standard backend executes syscalls inline on the calling thread and
//! keeps only readiness on the driver thread. The F-Stack backend executes
//! *everything* (syscalls and epoll updates) on the thread that called
//! `ff_init`/`ff_run` -- the process main thread -- because F-Stack is a
//! single-threaded user-space network stack. Application tasks run on a
//! separate thread and submit operations to that thread.
//!
//! Readiness is level-triggered: once an event is delivered for a direction
//! its interest is cleared, so a socket that nobody is waiting on does not wake
//! the reactor in a loop; it is re-armed the next time a task waits.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use crate::ops::{Op, Slot};
use crate::sys;
use crate::sys::{READ, WRITE};
use crate::Config;

/// A command executed by the thread that owns the I/O platform.
pub(crate) enum Command {
    Op(Op, Arc<Slot>),
    Register(i32),
    Deregister(i32),
    Interest(i32, u8),
    Shutdown(i32, i32),
    Close(i32),
}

/// Execute a command against the given epoll instance.
fn apply(epfd: i32, command: Command) {
    match command {
        Command::Op(op, slot) => slot.run(op),
        Command::Register(fd) => {
            let mut ev = sys::make_event(0, fd as u64);
            let _ = sys::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, &mut ev);
        }
        Command::Deregister(fd) => {
            let mut ev = sys::make_event(0, fd as u64);
            let _ = sys::epoll_ctl(epfd, libc::EPOLL_CTL_DEL, fd, &mut ev);
        }
        Command::Interest(fd, bits) => {
            let mut mask = 0u32;
            if bits & READ != 0 {
                mask |= libc::EPOLLIN as u32;
            }
            if bits & WRITE != 0 {
                mask |= libc::EPOLLOUT as u32;
            }
            let mut ev = sys::make_event(mask, fd as u64);
            let _ = sys::epoll_ctl(epfd, libc::EPOLL_CTL_MOD, fd, &mut ev);
        }
        Command::Shutdown(fd, how) => {
            let _ = sys::shutdown(fd, how);
        }
        Command::Close(fd) => sys::close(fd),
    }
}

#[cfg_attr(not(feature = "fstack"), allow(dead_code))]
enum Kind {
    /// Standard backend: syscalls run inline, readiness on the driver thread.
    #[cfg_attr(feature = "fstack", allow(dead_code))]
    Std { epfd: i32 },
    /// F-Stack backend: everything runs on the F-Stack thread.
    Fstack { queue: Mutex<VecDeque<Command>> },
    /// F-Stack native-mt: syscalls run inline on the owning lcore.
    #[cfg(feature = "fstack")]
    FstackInline { epfd: std::sync::atomic::AtomicI32 },
}

/// The I/O platform: where syscalls and event registrations are executed.
pub(crate) struct Platform {
    kind: Kind,
}

impl Platform {
    #[cfg_attr(feature = "fstack", allow(dead_code))]
    fn std(epfd: i32) -> Self {
        Self {
            kind: Kind::Std { epfd },
        }
    }

    #[cfg_attr(not(feature = "fstack"), allow(dead_code))]
    fn fstack() -> Self {
        Self {
            kind: Kind::Fstack {
                queue: Mutex::new(VecDeque::new()),
            },
        }
    }

    #[cfg(feature = "fstack")]
    fn fstack_inline() -> Self {
        Self {
            kind: Kind::FstackInline {
                epfd: std::sync::atomic::AtomicI32::new(-1),
            },
        }
    }

    #[cfg(feature = "fstack")]
    fn set_inline_epfd(&self, fd: i32) {
        if let Kind::FstackInline { epfd } = &self.kind {
            epfd.store(fd, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn dispatch(&self, command: Command) {
        match &self.kind {
            Kind::Std { epfd } => apply(*epfd, command),
            Kind::Fstack { queue } => queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(command),
            #[cfg(feature = "fstack")]
            Kind::FstackInline { epfd } => {
                let epfd = epfd.load(std::sync::atomic::Ordering::Relaxed);
                if epfd >= 0 {
                    apply(epfd, command);
                }
            }
        }
    }

    fn register(&self, fd: i32) {
        self.dispatch(Command::Register(fd));
    }

    fn deregister(&self, fd: i32) {
        self.dispatch(Command::Deregister(fd));
    }

    fn set_interest(&self, fd: i32, bits: u8) {
        self.dispatch(Command::Interest(fd, bits));
    }

    fn close(&self, fd: i32) {
        self.dispatch(Command::Close(fd));
    }

    #[cfg(feature = "fstack")]
    fn drain_queue(&self) -> Vec<Command> {
        match &self.kind {
            Kind::Fstack { queue } => queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
                .collect(),
            Kind::FstackInline { .. } | Kind::Std { .. } => Vec::new(),
        }
    }
}

struct InterestState {
    bits: u8,
    closed: bool,
}

/// Per-socket reactor state.
pub(crate) struct ScheduledIo {
    fd: i32,
    platform: Arc<Platform>,
    /// Registry owning this entry; used to remove it on close.
    reactor: Weak<Reactor>,
    state: Mutex<InterestState>,
    readiness: AtomicU8,
    read_waker: Mutex<Option<Waker>>,
    write_waker: Mutex<Option<Waker>>,
}

impl ScheduledIo {
    /// Consume the readiness bit for `direction`.
    fn take_ready(&self, direction: u8) -> bool {
        self.readiness.fetch_and(!direction, Ordering::AcqRel) & direction != 0
    }

    fn waker_slot(&self, direction: u8) -> &Mutex<Option<Waker>> {
        if direction & READ != 0 {
            &self.read_waker
        } else {
            &self.write_waker
        }
    }

    /// Register interest in `direction` with the platform.
    fn arm(&self, direction: u8) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return;
        }
        let next = state.bits | direction;
        if next != state.bits {
            state.bits = next;
            self.platform.set_interest(self.fd, next);
        }
    }

    /// Drop interest in `directions` (called by the driver after delivery).
    fn disarm(&self, directions: u8) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return;
        }
        let next = state.bits & !directions;
        if next != state.bits {
            state.bits = next;
            self.platform.set_interest(self.fd, next);
        }
    }

    /// Called on the driver thread when epoll reports events for this socket.
    fn deliver(&self, ready: u8) {
        self.readiness.fetch_or(ready, Ordering::AcqRel);
        self.disarm(ready);
        if ready & READ != 0 {
            if let Some(waker) = self
                .read_waker
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                waker.wake();
            }
        }
        if ready & WRITE != 0 {
            if let Some(waker) = self
                .write_waker
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                waker.wake();
            }
        }
    }

    /// Mark the socket closed and release its kernel resources exactly once.
    fn close(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed {
                return;
            }
            state.closed = true;
            state.bits = 0;
        }
        self.platform.deregister(self.fd);
        self.platform.close(self.fd);
    }
}

/// A socket's registration in the reactor.
pub(crate) struct Io {
    fd: i32,
    sched: Arc<ScheduledIo>,
}

impl Io {
    /// Register `fd` with the reactor.
    pub(crate) fn new(fd: i32) -> io::Result<Io> {
        let reactor = reactor()?;
        let sched = Arc::new(ScheduledIo {
            fd,
            platform: Arc::clone(&reactor.platform),
            reactor: Arc::downgrade(&reactor),
            state: Mutex::new(InterestState {
                bits: 0,
                closed: false,
            }),
            readiness: AtomicU8::new(0),
            read_waker: Mutex::new(None),
            write_waker: Mutex::new(None),
        });
        reactor
            .sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fd, sched.clone());
        reactor.platform.register(fd);
        Ok(Io { fd, sched })
    }

    /// The raw file descriptor.
    pub(crate) fn fd(&self) -> i32 {
        self.fd
    }

    /// Wait until the socket is readable.
    pub(crate) fn readable(&self) -> ReadinessFuture {
        ReadinessFuture {
            sched: self.sched.clone(),
            direction: READ,
        }
    }

    /// Wait until the socket is writable.
    pub(crate) fn writable(&self) -> ReadinessFuture {
        ReadinessFuture {
            sched: self.sched.clone(),
            direction: WRITE,
        }
    }
}

impl Drop for Io {
    fn drop(&mut self) {
        if let Some(reactor) = self.sched.reactor.upgrade() {
            reactor
                .sockets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.fd);
        }
        self.sched.close();
    }
}

/// Future returned by [`Io::readable`]/[`Io::writable`].
pub(crate) struct ReadinessFuture {
    sched: Arc<ScheduledIo>,
    direction: u8,
}

impl ReadinessFuture {
    /// Poll for readiness. Never returns an error: socket errors surface from
    /// the subsequent syscall.
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.sched.take_ready(self.direction) {
            return Poll::Ready(());
        }
        // Register the waker before arming, then re-check: an event delivered
        // in between is observed by the second check.
        {
            let slot = self.sched.waker_slot(self.direction);
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
        }
        self.sched.arm(self.direction);
        if self.sched.take_ready(self.direction) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl std::future::Future for ReadinessFuture {
    type Output = ();

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        ReadinessFuture::poll(&mut self, cx)
    }
}

/// The global reactor.
pub(crate) struct Reactor {
    pub(crate) platform: Arc<Platform>,
    pub(crate) sockets: Mutex<HashMap<i32, Arc<ScheduledIo>>>,
}

static REACTOR: std::sync::OnceLock<Arc<Reactor>> = std::sync::OnceLock::new();
#[cfg(feature = "fstack")]
static FSTACK_EPFD: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
static INIT_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// Reactor of the worker this thread belongs to. Set by [`crate::worker`]
    /// before the worker's tasks start running.
    static CURRENT: RefCell<Option<Arc<Reactor>>> = const { RefCell::new(None) };
}

/// The reactor of the current worker, if the thread runs inside one.
pub(crate) fn current() -> Option<Arc<Reactor>> {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// Bind the current thread to a worker reactor (or clear the binding).
pub(crate) fn set_current(reactor: Option<Arc<Reactor>>) {
    CURRENT.with(|slot| *slot.borrow_mut() = reactor);
}

/// Get the reactor for this thread: the worker's reactor when running inside a
/// [`crate::worker::Worker`], otherwise the process-wide default reactor.
///
/// Initialises the default reactor with a default configuration when the caller
/// did not call [`crate::init`] first.
pub(crate) fn reactor() -> io::Result<Arc<Reactor>> {
    if let Some(reactor) = current() {
        return Ok(reactor);
    }
    if let Some(reactor) = REACTOR.get() {
        return Ok(Arc::clone(reactor));
    }
    crate::init(Config::default())?;
    REACTOR
        .get()
        .map(Arc::clone)
        .ok_or_else(|| io::Error::other("reactor failed to initialise"))
}

/// Submit an operation to the platform of the current worker.
pub(crate) fn dispatch(command: Command) {
    let reactor = current()
        .or_else(|| REACTOR.get().map(Arc::clone))
        .expect("tokio-fstack reactor used before initialisation");
    reactor.platform.dispatch(command);
}

/// Close a raw fd that was never registered with the reactor.
pub(crate) fn close_raw(fd: i32) {
    if let Some(reactor) = current().or_else(|| REACTOR.get().map(Arc::clone)) {
        reactor.platform.close(fd);
    }
}

/// Shut down part of a connection without waiting for completion.
pub(crate) fn shutdown(fd: i32, how: i32) {
    if let Some(reactor) = current().or_else(|| REACTOR.get().map(Arc::clone)) {
        reactor.platform.dispatch(Command::Shutdown(fd, how));
    }
}

/// Start the default reactor. Idempotent.
pub(crate) fn start(config: Config) -> io::Result<()> {
    // Inside a native-mt worker the thread-local reactor is already installed;
    // the global single-stack reactor must not be created.
    if current().is_some() {
        return Ok(());
    }
    let _guard = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if REACTOR.get().is_some() {
        return Ok(());
    }
    let reactor = build(config)?;
    let _ = REACTOR.set(reactor);
    Ok(())
}

/// Build an independent reactor for a worker.
///
/// The caller is responsible for binding it to the worker thread with
/// [`set_current`].
pub(crate) fn build_worker(config: Config) -> io::Result<Arc<Reactor>> {
    build(config)
}

#[cfg(not(feature = "fstack"))]
fn build(_config: Config) -> io::Result<Arc<Reactor>> {
    let epfd = sys::epoll_create()?;
    let reactor = Arc::new(Reactor {
        platform: Arc::new(Platform::std(epfd)),
        sockets: Mutex::new(HashMap::new()),
    });
    let driver = Arc::clone(&reactor);
    std::thread::Builder::new()
        .name("tokio-fstack-reactor".to_owned())
        .spawn(move || std_driver(driver))?;
    Ok(reactor)
}

#[cfg(feature = "fstack")]
fn build(_config: Config) -> io::Result<Arc<Reactor>> {
    Err(io::Error::other(
        "F-Stack must be initialised on the main thread: use \
         tokio_fstack::run_with_stack (or init_fstack + run_fstack_loop)",
    ))
}

/// Initialise F-Stack and create the reactor on the **calling thread**.
///
/// F-Stack's `ff_init` must run on the process main thread (it installs
/// per-thread FreeBSD state that later initialisation steps rely on). The
/// caller must subsequently call [`run_fstack_loop`] on the same thread;
/// the proxy/application runs on another thread and submits operations
/// through the reactor queue.
#[cfg(feature = "fstack")]
pub(crate) fn init_fstack(config: Config) -> io::Result<()> {
    let _guard = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if REACTOR.get().is_some() {
        return Ok(());
    }

    // argv[0] is the program name, the rest are EAL/F-Stack arguments.
    let mut args = Vec::with_capacity(config.eal_args.len() + 1);
    args.push(
        std::env::args()
            .next()
            .unwrap_or_else(|| "mc-proxy".to_owned()),
    );
    args.extend(config.eal_args);
    let c_args = sys::cstrings(&args)?;
    let mut argv: Vec<*const libc::c_char> = c_args.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());

    sys::init(argv.len() as i32 - 1, argv.as_ptr())?;
    let epfd = sys::epoll_create()?;
    let reactor = Arc::new(Reactor {
        platform: Arc::new(Platform::fstack()),
        sockets: Mutex::new(HashMap::new()),
    });
    if FSTACK_EPFD.set(epfd).is_err() {
        return Err(io::Error::other("F-Stack epoll fd already initialised"));
    }
    let _ = REACTOR.set(reactor);
    Ok(())
}

/// Run the F-Stack event loop on the calling thread until `ff_stop_run`.
///
/// Must be called on the same thread as [`init_fstack`].
#[cfg(feature = "fstack")]
pub(crate) fn run_fstack_loop() -> io::Result<()> {
    let reactor = match REACTOR.get() {
        Some(reactor) => Arc::clone(reactor),
        None => {
            return Err(io::Error::other(
                "run_fstack_loop called before init_fstack",
            ))
        }
    };
    let epfd = match FSTACK_EPFD.get() {
        Some(epfd) => *epfd,
        None => return Err(io::Error::other("F-Stack epoll fd is missing")),
    };
    let ctx = Box::new(StackContext {
        reactor,
        epfd,
        events: (0..512).map(|_| sys::make_event(0, 0)).collect(),
        batch: Vec::with_capacity(256),
    });
    let ctx = Box::into_raw(ctx);
    // SAFETY: `ctx` is leaked and outlives the (blocking) event loop; the
    // callback only dereferences it from this same thread.
    unsafe { sys::run(fstack_tick, ctx as *mut libc::c_void) };
    Ok(())
}

/// Stop the F-Stack event loop (thread-safe).
#[cfg(feature = "fstack")]
pub(crate) fn stop_fstack_loop() {
    sys::stop_run();
}

/// One lcore's state in native-mt mode.
#[cfg(feature = "fstack")]
type InlineSetup = Box<dyn FnOnce() -> Box<dyn FnMut() + Send> + Send>;

#[cfg(feature = "fstack")]
struct InlineSlot {
    reactor: Arc<Reactor>,
    epfd: std::sync::atomic::AtomicI32,
    events: Mutex<Vec<sys::EpollEvent>>,
    /// Called once on the lcore: builds the Tokio runtime and spawns the app.
    setup: Mutex<Option<InlineSetup>>,
    /// Drives the lcore's Tokio runtime from the F-Stack loop.
    pump: Mutex<Option<Box<dyn FnMut() + Send>>>,
}

/// Native-mt context: one inline reactor/runtime per F-Stack lcore.
#[cfg(feature = "fstack")]
pub(crate) struct MultiContext {
    slots: Vec<InlineSlot>,
}

#[cfg(feature = "fstack")]
impl MultiContext {
    pub(crate) fn workers(&self) -> usize {
        self.slots.len()
    }
}

/// Initialise F-Stack in native-mt mode (`thread_mode=1`) on the calling
/// (main) thread. Application code is not spawned here: the first tick on
/// each lcore creates that lcore's Tokio runtime, so all `ff_*` calls and all
/// socket I/O stay on the owning lcore thread.
#[cfg(feature = "fstack")]
pub(crate) fn init_fstack_multi(workers: usize, config: Config) -> io::Result<Arc<MultiContext>> {
    if workers == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "f-stack native-mt needs at least one worker",
        ));
    }
    let _guard = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if REACTOR.get().is_some() {
        return Err(io::Error::other(
            "F-Stack is already initialised for a single-stack reactor",
        ));
    }

    let mut args = Vec::with_capacity(config.eal_args.len() + 1);
    args.push(
        std::env::args()
            .next()
            .unwrap_or_else(|| "mc-proxy".to_owned()),
    );
    args.extend(config.eal_args);
    let c_args = sys::cstrings(&args)?;
    let mut argv: Vec<*const libc::c_char> = c_args.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());
    sys::init(argv.len() as i32 - 1, argv.as_ptr())?;

    let slots = (0..workers)
        .map(|_| InlineSlot {
            reactor: Arc::new(Reactor {
                platform: Arc::new(Platform::fstack_inline()),
                sockets: Mutex::new(HashMap::new()),
            }),
            epfd: std::sync::atomic::AtomicI32::new(-1),
            events: Mutex::new((0..512).map(|_| sys::make_event(0, 0)).collect()),
            setup: Mutex::new(None),
            pump: Mutex::new(None),
        })
        .collect();
    Ok(Arc::new(MultiContext { slots }))
}

/// Install the per-lcore setup closure (runtime + application future).
#[cfg(feature = "fstack")]
pub(crate) fn set_inline_setup(ctx: &MultiContext, index: usize, setup: InlineSetup) {
    *ctx.slots[index]
        .setup
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(setup);
}

/// Reactor belonging to worker `index` in native-mt mode.
#[cfg(feature = "fstack")]
pub(crate) fn multi_reactor(ctx: &MultiContext, index: usize) -> Arc<Reactor> {
    Arc::clone(&ctx.slots[index].reactor)
}

/// Run the native-mt F-Stack loop until `ff_stop_run`.
#[cfg(feature = "fstack")]
pub(crate) fn run_fstack_multi(ctx: Arc<MultiContext>) -> io::Result<()> {
    let raw = Arc::into_raw(ctx);
    // SAFETY: the context is kept alive by the raw Arc for the whole loop;
    // each lcore only touches its own slot.
    unsafe { sys::run(multi_tick, raw as *mut libc::c_void) };
    // SAFETY: `ff_run` has returned, so no lcore can touch the context.
    unsafe { drop(Arc::from_raw(raw)) };
    Ok(())
}

#[cfg(feature = "fstack")]
unsafe extern "C" fn multi_tick(arg: *mut libc::c_void) -> i32 {
    // SAFETY: `arg` is the leaked MultiContext created by run_fstack_multi.
    let ctx = unsafe { &*(arg as *const MultiContext) };
    let index = sys::cur_proc_id();
    if index < 0 || index as usize >= ctx.slots.len() {
        return 0;
    }
    let slot = &ctx.slots[index as usize];

    let mut epfd = slot.epfd.load(std::sync::atomic::Ordering::Relaxed);
    if epfd < 0 {
        match sys::epoll_create() {
            Ok(fd) => {
                slot.epfd.store(fd, std::sync::atomic::Ordering::Relaxed);
                slot.reactor.platform.set_inline_epfd(fd);
                epfd = fd;
            }
            Err(_) => return 0,
        }
    }

    // First tick on this lcore: create the Tokio runtime and start the app.
    {
        let mut setup = slot.setup.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(setup) = setup.take() {
            let pump = setup();
            *slot.pump.lock().unwrap_or_else(|e| e.into_inner()) = Some(pump);
        }
    }

    // Deliver readiness events, then run the lcore's Tokio tasks inline.
    {
        let mut events = slot.events.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(count) = sys::epoll_wait(epfd, &mut events, 0) {
            for ev in events.iter().take(count) {
                dispatch_event(&slot.reactor.sockets, ev);
            }
        }
    }
    if let Some(pump) = slot.pump.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        pump();
    }
    0
}

/// Turn an epoll event mask into reactor readiness bits.
fn readiness_of(mask: u32) -> u8 {
    let err = libc::EPOLLERR as u32;
    let hup = libc::EPOLLHUP as u32;
    let mut ready = 0u8;
    if mask & (libc::EPOLLIN as u32 | hup | err) != 0 {
        ready |= READ;
    }
    if mask & (libc::EPOLLOUT as u32 | hup | err) != 0 {
        ready |= WRITE;
    }
    ready
}

/// Dispatch one epoll event to its socket.
fn dispatch_event(sched_registry: &Mutex<HashMap<i32, Arc<ScheduledIo>>>, ev: &sys::EpollEvent) {
    let ready = readiness_of(sys::event_mask(ev));
    if ready == 0 {
        return;
    }
    let fd = sys::event_data(ev) as i32;
    let sched = sched_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&fd)
        .cloned();
    if let Some(sched) = sched {
        sched.deliver(ready);
    }
}

#[cfg(not(feature = "fstack"))]
fn std_driver(reactor: Arc<Reactor>) {
    let epfd = match &reactor.platform.kind {
        Kind::Std { epfd } => *epfd,
        Kind::Fstack { .. } => unreachable!(),
    };
    let mut events: Vec<sys::EpollEvent> = (0..256).map(|_| sys::make_event(0, 0)).collect();
    loop {
        match sys::epoll_wait(epfd, &mut events, -1) {
            Ok(count) => {
                for ev in events.iter().take(count) {
                    dispatch_event(&reactor.sockets, ev);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    }
}

#[cfg(feature = "fstack")]
struct StackContext {
    reactor: Arc<Reactor>,
    epfd: i32,
    events: Vec<sys::EpollEvent>,
    batch: Vec<Command>,
}

#[cfg(feature = "fstack")]
unsafe extern "C" fn fstack_tick(arg: *mut libc::c_void) -> i32 {
    // SAFETY: `arg` is the leaked `StackContext` created in `run_fstack_loop`,
    // and the F-Stack loop runs this callback on the same thread.
    let ctx = unsafe { &mut *(arg as *mut StackContext) };

    // 1. Run queued syscalls and interest changes.
    {
        let mut queue = ctx.reactor.platform.drain_queue();
        ctx.batch.append(&mut queue);
    }
    for command in ctx.batch.drain(..) {
        apply(ctx.epfd, command);
    }

    // 2. Poll the F-Stack kqueue (ff_epoll_wait is always non-blocking).
    if let Ok(count) = sys::epoll_wait(ctx.epfd, &mut ctx.events, 0) {
        for ev in ctx.events.iter().take(count) {
            dispatch_event(&ctx.reactor.sockets, ev);
        }
    }
    0
}
