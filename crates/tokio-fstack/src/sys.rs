//! Thin non-blocking syscall layer.
//!
//! Two implementations of the same surface live here:
//!
//! * the *standard* backend, which calls the host Linux kernel directly, and
//! * the *F-Stack* backend, which calls `ff_*` functions from `libfstack`.
//!
//! F-Stack exposes Linux-compatible constants (it translates `SO_*`, `TCP_*`,
//! `FIONBIO`, ... internally, see `lib/ff_syscall_wrapper.c`), so all socket
//! level constants come from `libc` for both backends.
//!
//! Every function is non-blocking: sockets are always created in non-blocking
//! mode and the reactor is responsible for waiting until an operation can make
//! progress.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::raw::{c_int, c_void};

use libc::socklen_t;

/// Readiness bit: the socket can make read progress.
pub(crate) const READ: u8 = 0b01;
/// Readiness bit: the socket can make write progress.
pub(crate) const WRITE: u8 = 0b10;

/// Flags passed to `send`/`sendto` (suppress `SIGPIPE` on the kernel backend).
#[cfg(not(feature = "fstack"))]
const SEND_FLAGS: c_int = libc::MSG_NOSIGNAL;
#[cfg(feature = "fstack")]
const SEND_FLAGS: c_int = 0;

/// An epoll event exactly as the kernel ABI expects it.
///
/// On x86_64/aarch64 Linux `struct epoll_event` is packed and 12 bytes wide;
/// using a `#[repr(C)]` Rust struct with natural alignment would be 16 bytes
/// and corrupt every event after the first one. `libc::epoll_event` already
/// mirrors the packed ABI, so it is reused here and only accessed through the
/// unaligned helpers below.
pub(crate) type EpollEvent = libc::epoll_event;

/// Read the `events` field of a (potentially packed) epoll event.
pub(crate) fn event_mask(ev: &EpollEvent) -> u32 {
    // SAFETY: `addr_of!` does not create a reference to the packed field.
    unsafe { std::ptr::addr_of!(ev.events).read_unaligned() }
}

/// Read the `data` field of a (potentially packed) epoll event.
pub(crate) fn event_data(ev: &EpollEvent) -> u64 {
    // SAFETY: `addr_of!` does not create a reference to the packed field.
    unsafe { std::ptr::addr_of!(ev.u64).read_unaligned() }
}

/// Build an epoll event with the given mask and opaque token.
pub(crate) fn make_event(mask: u32, token: u64) -> EpollEvent {
    // SAFETY: `libc::epoll_event` is a plain-old-data struct; all zeroes is a
    // valid bit pattern for it.
    let mut ev: EpollEvent = unsafe { std::mem::zeroed() };
    // SAFETY: writing through `addr_of_mut!` avoids taking a reference to a
    // packed field.
    unsafe {
        std::ptr::addr_of_mut!(ev.events).write_unaligned(mask);
        std::ptr::addr_of_mut!(ev.u64).write_unaligned(token);
    }
    ev
}

/// Convert a `SocketAddr` into the raw `sockaddr_storage` + length pair the
/// socket API expects.
pub(crate) fn sockaddr_of(addr: SocketAddr) -> (libc::sockaddr_storage, socklen_t) {
    // SAFETY: `sockaddr_storage` is plain-old-data and zeroes are valid.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match addr {
        SocketAddr::V4(v4) => {
            // SAFETY: the storage is large enough and correctly aligned for
            // `sockaddr_in` on all supported targets.
            let sin = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in) };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr.s_addr = u32::from(*v4.ip()).to_be();
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in>() as socklen_t,
            )
        }
        SocketAddr::V6(v6) => {
            // SAFETY: as above, for `sockaddr_in6`.
            let sin6 = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in6) };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_addr.s6_addr = v6.ip().octets();
            sin6.sin6_scope_id = v6.scope_id();
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in6>() as socklen_t,
            )
        }
    }
}

/// Convert a raw `sockaddr_storage` (as filled in by `accept`/`recvfrom`) into
/// a `SocketAddr`.
pub(crate) fn sockaddr_to(storage: &libc::sockaddr_storage) -> Option<SocketAddr> {
    match c_int::from(storage.ss_family) {
        libc::AF_INET => {
            // SAFETY: the family says this is a `sockaddr_in`, and the storage
            // is at least as large.
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            Some(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)),
                u16::from_be(sin.sin_port),
            )))
        }
        libc::AF_INET6 => {
            // SAFETY: the family says this is a `sockaddr_in6`.
            let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            Some(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(sin6.sin6_addr.s6_addr),
                u16::from_be(sin6.sin6_port),
                sin6.sin6_flowinfo,
                sin6.sin6_scope_id,
            )))
        }
        _ => None,
    }
}

/// Return `true` when `err` means "not ready yet, try again after readiness".
pub(crate) fn is_would_block(err: &io::Error) -> bool {
    let Some(code) = err.raw_os_error() else {
        return false;
    };
    code == libc::EAGAIN || code == libc::EWOULDBLOCK
}

/// Return `true` when `err` is a non-blocking `connect` being in progress.
pub(crate) fn is_in_progress(err: &io::Error) -> bool {
    let Some(code) = err.raw_os_error() else {
        return false;
    };
    code == libc::EINPROGRESS || code == libc::EALREADY
}

#[cfg(not(feature = "fstack"))]
mod imp {
    use super::*;

    #[inline]
    fn cvt(ret: c_int) -> io::Result<c_int> {
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret)
        }
    }

    pub(crate) fn socket(domain: c_int, ty: c_int, proto: c_int) -> io::Result<c_int> {
        // SAFETY: plain libc call, no pointers involved.
        cvt(unsafe { libc::socket(domain, ty | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, proto) })
    }

    #[cfg_attr(not(feature = "fstack"), allow(dead_code))]
    pub(crate) fn set_nonblocking(fd: c_int) -> io::Result<()> {
        // SAFETY: `fd` is owned by the caller and the pointer is valid.
        cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) })?;
        Ok(())
    }

    pub(crate) fn bind(fd: c_int, addr: SocketAddr) -> io::Result<()> {
        let (storage, len) = sockaddr_of(addr);
        // SAFETY: `storage` lives for the duration of the call and `len`
        // matches its family.
        cvt(unsafe { libc::bind(fd, &storage as *const _ as *const libc::sockaddr, len) })?;
        Ok(())
    }

    pub(crate) fn listen(fd: c_int, backlog: c_int) -> io::Result<()> {
        // SAFETY: plain libc call.
        cvt(unsafe { libc::listen(fd, backlog) })?;
        Ok(())
    }

    pub(crate) fn accept(fd: c_int) -> io::Result<(c_int, SocketAddr)> {
        // SAFETY: zeroed storage is a valid initial state for `accept`.
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as socklen_t;
        // SAFETY: the storage pointer and length are valid for writes.
        let client = cvt(unsafe {
            libc::accept4(
                fd,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
                libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            )
        })?;
        let peer = sockaddr_to(&storage).unwrap_or_else(|| {
            // A connected socket always has a peer; fall back to an unspecified
            // address rather than failing the accept.
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        });
        Ok((client, peer))
    }

    pub(crate) fn connect(fd: c_int, addr: SocketAddr) -> io::Result<()> {
        let (storage, len) = sockaddr_of(addr);
        // SAFETY: `storage` lives for the duration of the call.
        cvt(unsafe { libc::connect(fd, &storage as *const _ as *const libc::sockaddr, len) })?;
        Ok(())
    }

    pub(crate) fn getsockname(fd: c_int) -> io::Result<SocketAddr> {
        // SAFETY: zeroed storage is valid and sized correctly.
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as socklen_t;
        // SAFETY: the pointer and length are valid for writes.
        cvt(unsafe {
            libc::getsockname(fd, &mut storage as *mut _ as *mut libc::sockaddr, &mut len)
        })?;
        sockaddr_to(&storage).ok_or_else(|| io::Error::from(io::ErrorKind::Other))
    }

    pub(crate) fn getpeername(fd: c_int) -> io::Result<SocketAddr> {
        // SAFETY: zeroed storage is valid and sized correctly.
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as socklen_t;
        // SAFETY: the pointer and length are valid for writes.
        cvt(unsafe {
            libc::getpeername(fd, &mut storage as *mut _ as *mut libc::sockaddr, &mut len)
        })?;
        sockaddr_to(&storage).ok_or_else(|| io::Error::from(io::ErrorKind::Other))
    }

    pub(crate) fn setsockopt_int(
        fd: c_int,
        level: c_int,
        name: c_int,
        value: c_int,
    ) -> io::Result<()> {
        // SAFETY: the value pointer is valid for the given length.
        cvt(unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                &value as *const c_int as *const c_void,
                std::mem::size_of::<c_int>() as socklen_t,
            )
        })?;
        Ok(())
    }

    pub(crate) fn getsockopt_int(fd: c_int, level: c_int, name: c_int) -> io::Result<c_int> {
        let mut value: c_int = 0;
        let mut len = std::mem::size_of::<c_int>() as socklen_t;
        // SAFETY: the value pointer and length are valid for writes.
        cvt(unsafe {
            libc::getsockopt(
                fd,
                level,
                name,
                &mut value as *mut c_int as *mut c_void,
                &mut len,
            )
        })?;
        Ok(value)
    }

    pub(crate) fn recv(fd: c_int, buf: *mut u8, len: usize, flags: c_int) -> io::Result<usize> {
        // SAFETY: the caller guarantees `buf` is valid for `len` writable bytes
        // for the duration of the call.
        let ret = unsafe { libc::recv(fd, buf as *mut c_void, len, flags) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    pub(crate) fn send(fd: c_int, buf: *const u8, len: usize) -> io::Result<usize> {
        // SAFETY: the caller guarantees `buf` is valid for `len` readable bytes
        // for the duration of the call.
        let ret = unsafe { libc::send(fd, buf as *const c_void, len, SEND_FLAGS) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    pub(crate) fn recv_from(
        fd: c_int,
        buf: *mut u8,
        len: usize,
    ) -> io::Result<(usize, Option<SocketAddr>)> {
        // SAFETY: zeroed storage is valid.
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut addr_len = std::mem::size_of::<libc::sockaddr_storage>() as socklen_t;
        // SAFETY: all pointers are valid for the call.
        let ret = unsafe {
            libc::recvfrom(
                fd,
                buf as *mut c_void,
                len,
                0,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut addr_len,
            )
        };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok((ret as usize, sockaddr_to(&storage)))
        }
    }

    pub(crate) fn send_to(
        fd: c_int,
        buf: *const u8,
        len: usize,
        addr: SocketAddr,
    ) -> io::Result<usize> {
        let (storage, addr_len) = sockaddr_of(addr);
        // SAFETY: all pointers are valid for the call.
        let ret = unsafe {
            libc::sendto(
                fd,
                buf as *const c_void,
                len,
                SEND_FLAGS,
                &storage as *const _ as *const libc::sockaddr,
                addr_len,
            )
        };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    pub(crate) fn shutdown(fd: c_int, how: c_int) -> io::Result<()> {
        // SAFETY: plain libc call.
        cvt(unsafe { libc::shutdown(fd, how) })?;
        Ok(())
    }

    pub(crate) fn close(fd: c_int) {
        // SAFETY: the caller transfers ownership of `fd`.
        unsafe {
            libc::close(fd);
        }
    }

    pub(crate) fn epoll_create() -> io::Result<c_int> {
        // SAFETY: plain libc call.
        cvt(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })
    }

    pub(crate) fn epoll_ctl(
        epfd: c_int,
        op: c_int,
        fd: c_int,
        events: &mut EpollEvent,
    ) -> io::Result<()> {
        // SAFETY: `events` is a valid epoll event for the duration of the call.
        cvt(unsafe { libc::epoll_ctl(epfd, op, fd, events) })?;
        Ok(())
    }

    pub(crate) fn epoll_wait(
        epfd: c_int,
        events: &mut [EpollEvent],
        timeout_ms: c_int,
    ) -> io::Result<usize> {
        // SAFETY: the event slice is valid for `events.len()` writes.
        let ret = unsafe {
            libc::epoll_wait(epfd, events.as_mut_ptr(), events.len() as c_int, timeout_ms)
        };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }
}

#[cfg(feature = "fstack")]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::os::raw::c_ulong;

    // `linux_sockaddr` in ff_api.h is layout-compatible with `libc::sockaddr`.
    extern "C" {
        pub(crate) fn ff_init(argc: c_int, argv: *const *const libc::c_char) -> c_int;
        fn ff_stop_run();
        fn ff_cur_proc_id() -> c_int;
        pub(crate) fn ff_run(
            callback: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
            arg: *mut c_void,
        );

        fn ff_socket(domain: c_int, ty: c_int, proto: c_int) -> c_int;
        fn ff_ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        fn ff_bind(fd: c_int, addr: *const libc::sockaddr, len: socklen_t) -> c_int;
        fn ff_listen(fd: c_int, backlog: c_int) -> c_int;
        fn ff_accept(fd: c_int, addr: *mut libc::sockaddr, len: *mut socklen_t) -> c_int;
        fn ff_connect(fd: c_int, addr: *const libc::sockaddr, len: socklen_t) -> c_int;
        fn ff_getsockname(fd: c_int, addr: *mut libc::sockaddr, len: *mut socklen_t) -> c_int;
        fn ff_getpeername(fd: c_int, addr: *mut libc::sockaddr, len: *mut socklen_t) -> c_int;
        fn ff_setsockopt(
            fd: c_int,
            level: c_int,
            name: c_int,
            value: *const c_void,
            len: socklen_t,
        ) -> c_int;
        fn ff_getsockopt(
            fd: c_int,
            level: c_int,
            name: c_int,
            value: *mut c_void,
            len: *mut socklen_t,
        ) -> c_int;
        fn ff_recv(fd: c_int, buf: *mut c_void, len: usize, flags: c_int) -> isize;
        fn ff_send(fd: c_int, buf: *const c_void, len: usize, flags: c_int) -> isize;
        fn ff_recvfrom(
            fd: c_int,
            buf: *mut c_void,
            len: usize,
            flags: c_int,
            from: *mut libc::sockaddr,
            from_len: *mut socklen_t,
        ) -> isize;
        fn ff_sendto(
            fd: c_int,
            buf: *const c_void,
            len: usize,
            flags: c_int,
            to: *const libc::sockaddr,
            to_len: socklen_t,
        ) -> isize;
        fn ff_shutdown(fd: c_int, how: c_int) -> c_int;
        fn ff_close(fd: c_int) -> c_int;
        fn ff_epoll_create(size: c_int) -> c_int;
        fn ff_epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut EpollEvent) -> c_int;
        fn ff_epoll_wait(
            epfd: c_int,
            events: *mut EpollEvent,
            maxevents: c_int,
            timeout: c_int,
        ) -> c_int;
    }

    #[inline]
    fn cvt(ret: c_int) -> io::Result<c_int> {
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret)
        }
    }

    pub(crate) fn socket(domain: c_int, ty: c_int, proto: c_int) -> io::Result<c_int> {
        // SAFETY: plain FFI call.
        let fd = cvt(unsafe { ff_socket(domain, ty, proto) })?;
        set_nonblocking(fd)?;
        Ok(fd)
    }

    pub(crate) fn set_nonblocking(fd: c_int) -> io::Result<()> {
        let on: c_int = 1;
        // SAFETY: `FIONBIO` takes a pointer to an `int`.
        cvt(unsafe { ff_ioctl(fd, libc::FIONBIO as c_ulong, &on as *const c_int) })?;
        Ok(())
    }

    pub(crate) fn bind(fd: c_int, addr: SocketAddr) -> io::Result<()> {
        let (storage, len) = sockaddr_of(addr);
        // SAFETY: the storage is valid for the call.
        cvt(unsafe { ff_bind(fd, &storage as *const _ as *const libc::sockaddr, len) })?;
        Ok(())
    }

    pub(crate) fn listen(fd: c_int, backlog: c_int) -> io::Result<()> {
        // SAFETY: plain FFI call.
        cvt(unsafe { ff_listen(fd, backlog) })?;
        Ok(())
    }

    pub(crate) fn accept(fd: c_int) -> io::Result<(c_int, SocketAddr)> {
        // SAFETY: zeroed storage is valid.
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as socklen_t;
        // SAFETY: valid pointers for the call.
        let client =
            cvt(unsafe { ff_accept(fd, &mut storage as *mut _ as *mut libc::sockaddr, &mut len) })?;
        // F-Stack does not implement `accept4`; make the new fd non-blocking
        // explicitly.
        set_nonblocking(client)?;
        let peer = sockaddr_to(&storage)
            .unwrap_or_else(|| SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)));
        Ok((client, peer))
    }

    pub(crate) fn connect(fd: c_int, addr: SocketAddr) -> io::Result<()> {
        let (storage, len) = sockaddr_of(addr);
        // SAFETY: the storage is valid for the call.
        cvt(unsafe { ff_connect(fd, &storage as *const _ as *const libc::sockaddr, len) })?;
        Ok(())
    }

    pub(crate) fn getsockname(fd: c_int) -> io::Result<SocketAddr> {
        // SAFETY: zeroed storage is valid.
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as socklen_t;
        // SAFETY: valid pointers for the call.
        cvt(unsafe {
            ff_getsockname(fd, &mut storage as *mut _ as *mut libc::sockaddr, &mut len)
        })?;
        sockaddr_to(&storage).ok_or_else(|| io::Error::from(io::ErrorKind::Other))
    }

    pub(crate) fn getpeername(fd: c_int) -> io::Result<SocketAddr> {
        // SAFETY: zeroed storage is valid.
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as socklen_t;
        // SAFETY: valid pointers for the call.
        cvt(unsafe {
            ff_getpeername(fd, &mut storage as *mut _ as *mut libc::sockaddr, &mut len)
        })?;
        sockaddr_to(&storage).ok_or_else(|| io::Error::from(io::ErrorKind::Other))
    }

    pub(crate) fn setsockopt_int(
        fd: c_int,
        level: c_int,
        name: c_int,
        value: c_int,
    ) -> io::Result<()> {
        // SAFETY: the value pointer is valid for the given length.
        cvt(unsafe {
            ff_setsockopt(
                fd,
                level,
                name,
                &value as *const c_int as *const c_void,
                std::mem::size_of::<c_int>() as socklen_t,
            )
        })?;
        Ok(())
    }

    pub(crate) fn getsockopt_int(fd: c_int, level: c_int, name: c_int) -> io::Result<c_int> {
        let mut value: c_int = 0;
        let mut len = std::mem::size_of::<c_int>() as socklen_t;
        // SAFETY: valid pointers for the call.
        cvt(unsafe {
            ff_getsockopt(
                fd,
                level,
                name,
                &mut value as *mut c_int as *mut c_void,
                &mut len,
            )
        })?;
        Ok(value)
    }

    pub(crate) fn recv(fd: c_int, buf: *mut u8, len: usize, flags: c_int) -> io::Result<usize> {
        // SAFETY: the caller guarantees the buffer is valid for the call.
        let ret = unsafe { ff_recv(fd, buf as *mut c_void, len, flags) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    pub(crate) fn send(fd: c_int, buf: *const u8, len: usize) -> io::Result<usize> {
        // SAFETY: the caller guarantees the buffer is valid for the call.
        let ret = unsafe { ff_send(fd, buf as *const c_void, len, SEND_FLAGS) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    pub(crate) fn recv_from(
        fd: c_int,
        buf: *mut u8,
        len: usize,
    ) -> io::Result<(usize, Option<SocketAddr>)> {
        // SAFETY: zeroed storage is valid.
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut addr_len = std::mem::size_of::<libc::sockaddr_storage>() as socklen_t;
        // SAFETY: valid pointers for the call.
        let ret = unsafe {
            ff_recvfrom(
                fd,
                buf as *mut c_void,
                len,
                0,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut addr_len,
            )
        };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok((ret as usize, sockaddr_to(&storage)))
        }
    }

    pub(crate) fn send_to(
        fd: c_int,
        buf: *const u8,
        len: usize,
        addr: SocketAddr,
    ) -> io::Result<usize> {
        let (storage, addr_len) = sockaddr_of(addr);
        // SAFETY: valid pointers for the call.
        let ret = unsafe {
            ff_sendto(
                fd,
                buf as *const c_void,
                len,
                SEND_FLAGS,
                &storage as *const _ as *const libc::sockaddr,
                addr_len,
            )
        };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    pub(crate) fn shutdown(fd: c_int, how: c_int) -> io::Result<()> {
        // SAFETY: plain FFI call.
        cvt(unsafe { ff_shutdown(fd, how) })?;
        Ok(())
    }

    pub(crate) fn close(fd: c_int) {
        // SAFETY: the caller transfers ownership of `fd`.
        unsafe {
            ff_close(fd);
        }
    }

    pub(crate) fn epoll_create() -> io::Result<c_int> {
        // SAFETY: plain FFI call.
        cvt(unsafe { ff_epoll_create(0) })
    }

    pub(crate) fn epoll_ctl(
        epfd: c_int,
        op: c_int,
        fd: c_int,
        events: &mut EpollEvent,
    ) -> io::Result<()> {
        // SAFETY: `events` is valid for the duration of the call.
        cvt(unsafe { ff_epoll_ctl(epfd, op, fd, events) })?;
        Ok(())
    }

    pub(crate) fn epoll_wait(
        epfd: c_int,
        events: &mut [EpollEvent],
        timeout_ms: c_int,
    ) -> io::Result<usize> {
        // SAFETY: the slice is valid for `events.len()` writes.
        let ret =
            unsafe { ff_epoll_wait(epfd, events.as_mut_ptr(), events.len() as c_int, timeout_ms) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    /// Dense per-thread stack index (0..nb_threads) for native-mt.
    pub(crate) fn cur_proc_id() -> c_int {
        // SAFETY: F-Stack returns the calling lcore's dense proc id.
        unsafe { ff_cur_proc_id() }
    }

    /// Ask the F-Stack main loop to stop (thread-safe).
    pub(crate) fn stop_run() {
        // SAFETY: `ff_stop_run` only sets a stop flag in the F-Stack loop.
        unsafe { ff_stop_run() }
    }

    /// Initialise the F-Stack data plane with the given EAL arguments.
    ///
    /// Must be called on the thread that will later run [`ff_run`], which is
    /// exactly what the reactor thread does.
    pub(crate) fn init(argc: c_int, argv: *const *const libc::c_char) -> io::Result<()> {
        // SAFETY: `argv` is a valid, NULL-terminated array owned by the caller
        // for the duration of the call.
        cvt(unsafe { ff_init(argc, argv) })?;
        Ok(())
    }

    /// Run the F-Stack main loop; never returns (until `ff_stop_run`).
    ///
    /// # Safety
    /// The callback must be safe to call from the F-Stack main loop thread.
    pub(crate) unsafe fn run(
        callback: unsafe extern "C" fn(*mut c_void) -> c_int,
        arg: *mut c_void,
    ) {
        // SAFETY: the caller upholds the contract documented on this function:
        // the callback is only ever invoked from the F-Stack loop thread.
        unsafe { ff_run(Some(callback), arg) }
    }

    /// Turn `args` into a C string vector suitable for `ff_init`.
    pub(crate) fn cstrings(args: &[String]) -> io::Result<Vec<CString>> {
        args.iter()
            .map(|a| {
                CString::new(a.as_str()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
            })
            .collect()
    }
}

#[cfg(not(feature = "fstack"))]
pub(crate) use imp::{
    accept, bind, close, connect, epoll_create, epoll_ctl, epoll_wait, getpeername, getsockname,
    getsockopt_int, listen, recv, recv_from, send, send_to, setsockopt_int, shutdown, socket,
};
#[cfg(feature = "fstack")]
pub(crate) use imp::{
    accept, bind, close, connect, epoll_create, epoll_ctl, epoll_wait, getpeername, getsockname,
    getsockopt_int, listen, recv, recv_from, send, send_to, setsockopt_int, shutdown, socket,
};
#[cfg(feature = "fstack")]
pub(crate) use imp::{cstrings, cur_proc_id, init, run, stop_run};
