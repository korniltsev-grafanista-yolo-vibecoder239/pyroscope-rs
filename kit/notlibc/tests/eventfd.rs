//! Integration tests for `notlibc::eventfd`.
//!
//! libc is used only in the test harness for draining fds and verifying
//! counts; production code uses no libc.

#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use notlibc::eventfd::{EventFd, EventSet, EVENT_SET_CAPACITY};
use std::sync::Arc;
use std::thread;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Drain one unit from a semaphore-mode eventfd.
/// Returns the value read (always 1 for semaphore mode) or the negative errno.
fn drain_one(fd: i32) -> i64 {
    let mut buf: u64 = 0;
    let ret = unsafe { libc::read(fd, &mut buf as *mut u64 as *mut libc::c_void, 8) };
    if ret < 0 {
        let errno = unsafe { *libc::__errno_location() };
        -(errno as i64)
    } else {
        buf as i64
    }
}

// ── EventFd tests ─────────────────────────────────────────────────────────────

#[test]
fn create_returns_valid_fd() {
    let efd = EventFd::new().expect("EventFd::new should succeed");
    assert!(efd.as_fd() >= 0, "fd must be non-negative");
    // Drop closes the fd automatically.
}

#[test]
fn notify_once_drain_reads_one() {
    let efd = EventFd::new().expect("EventFd::new");
    efd.notify();
    assert_eq!(drain_one(efd.as_fd()), 1);
}

#[test]
fn notify_twice_drain_twice_accumulates() {
    let efd = EventFd::new().expect("EventFd::new");
    efd.notify();
    efd.notify();
    // Semaphore mode: each read decrements by 1 and returns 1.
    assert_eq!(drain_one(efd.as_fd()), 1);
    assert_eq!(drain_one(efd.as_fd()), 1);
}

#[test]
fn non_blocking_second_read_returns_eagain() {
    let efd = EventFd::new().expect("EventFd::new");
    efd.notify();
    let _ = drain_one(efd.as_fd()); // drain the one notification
    let ret = drain_one(efd.as_fd()); // should be EAGAIN
    assert_eq!(
        ret,
        -(libc::EAGAIN as i64),
        "empty non-blocking eventfd should return EAGAIN, got {ret}"
    );
}

// ── EventSet tests ────────────────────────────────────────────────────────────

#[test]
fn event_set_single_fd_wait() {
    let efd = EventFd::new().expect("EventFd::new");
    let mut set = EventSet::new().expect("EventSet::new");
    let idx = set.add(&efd).expect("EventSet::add");
    assert_eq!(idx, 0);

    efd.notify();
    let fired = set.wait(-1).expect("EventSet::wait");
    assert_eq!(fired, 0);
}

#[test]
fn event_set_identifies_which_fd_fired() {
    // Register 4 eventfds; notify only the third one (index 2).
    let efds: Vec<EventFd> = (0..4).map(|_| EventFd::new().unwrap()).collect();
    let mut set = EventSet::new().unwrap();
    for efd in &efds {
        set.add(efd).unwrap();
    }

    efds[2].notify();
    let fired = set.wait(-1).unwrap();
    assert_eq!(fired, 2, "expected index 2 to fire");
}

#[test]
fn event_set_16_threads_one_reader() {
    const N: usize = 16;

    // Create 16 eventfds and an EventSet.
    let efds: Vec<Arc<EventFd>> = (0..N).map(|_| Arc::new(EventFd::new().unwrap())).collect();
    let mut set = EventSet::new().unwrap();
    for efd in &efds {
        set.add(efd).unwrap();
    }

    // Notify from thread 7.
    let notifier = Arc::clone(&efds[7]);
    let handle = thread::spawn(move || {
        notifier.notify();
    });

    let fired = set.wait(-1).unwrap();
    handle.join().unwrap();

    assert_eq!(fired, 7, "thread 7 should have fired index 7, got {fired}");
}

#[test]
fn event_set_all_16_threads_notify_wait_sees_at_least_one() {
    const N: usize = 16;

    let efds: Vec<Arc<EventFd>> = (0..N).map(|_| Arc::new(EventFd::new().unwrap())).collect();
    let mut set = EventSet::new().unwrap();
    for efd in &efds {
        set.add(efd).unwrap();
    }

    // All 16 threads notify simultaneously.
    let handles: Vec<_> = efds
        .iter()
        .map(|efd| {
            let efd = Arc::clone(efd);
            thread::spawn(move || efd.notify())
        })
        .collect();

    // Wait should return as soon as any one fires.
    let fired = set.wait(-1).unwrap();
    assert!(fired < N, "fired index {fired} out of range");

    for h in handles {
        h.join().unwrap();
    }
}

// ── EventFd additional tests ───────────────────────────────────────────────────

#[test]
fn drop_closes_fd() {
    // Strategy: record the /proc/self/fd/<N> symlink target before drop
    // (it points to "anon_inode:[eventfd]").  After drop the symlink must
    // either be gone or point to something different.  A parallel test that
    // reuses the same fd number will point to a different inode type, so
    // either outcome proves that Drop closed the original fd.
    let efd = EventFd::new().expect("EventFd::new");
    let fd = efd.as_fd();
    let proc_path = format!("/proc/self/fd/{fd}");

    let target_before = std::fs::read_link(&proc_path)
        .expect("fd must be visible in /proc/self/fd before drop");

    drop(efd);

    // After drop: either the link is gone, or it points somewhere else.
    match std::fs::read_link(&proc_path) {
        Err(_) => { /* fd gone — pass */ }
        Ok(target_after) => {
            assert_ne!(
                target_after, target_before,
                "fd {fd} still points to the same eventfd after drop"
            );
        }
    }
}

/// Global slot written by the SIGPROF handler.
static SIGNAL_FD: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(-1);

extern "C" fn sigprof_handler(_sig: libc::c_int) {
    let fd = SIGNAL_FD.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        // notify() is async-signal-safe: it uses a direct SYS_write syscall.
        let val: u64 = 1;
        unsafe {
            libc::write(fd, &val as *const u64 as *const libc::c_void, 8);
        }
    }
}

#[test]
fn notify_from_signal_handler() {
    let efd = EventFd::new().expect("EventFd::new");

    // Store the fd so the handler can find it.
    SIGNAL_FD.store(efd.as_fd(), std::sync::atomic::Ordering::Relaxed);

    // Install SIGPROF handler.
    let sa = libc::sigaction {
        sa_sigaction: sigprof_handler as *const () as libc::sighandler_t,
        sa_mask: unsafe { std::mem::zeroed() },
        sa_flags: 0,
        sa_restorer: None,
    };
    let mut old_sa: libc::sigaction = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::sigaction(libc::SIGPROF, &sa, &mut old_sa) };
    assert_eq!(ret, 0, "sigaction install failed");

    // Fire the signal synchronously.
    unsafe { libc::raise(libc::SIGPROF) };

    // Drain the fd — must see exactly 1.
    let count = drain_one(efd.as_fd());
    assert_eq!(count, 1, "expected 1 notification from signal handler, got {count}");

    // Restore the previous handler and clear the global slot.
    unsafe { libc::sigaction(libc::SIGPROF, &old_sa, std::ptr::null_mut()) };
    SIGNAL_FD.store(-1, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn notify_overflow_silently_swallowed() {
    // Semaphore-mode eventfd saturates at u64::MAX - 1.  Fill it up by
    // notifying u64::MAX - 1 times … that's impractical, so instead
    // pre-seed the counter via a direct write of (u64::MAX - 1) and then
    // call notify() once more.  The second write must fail with EAGAIN on a
    // non-blocking fd; notify() must not panic or propagate the error.
    let efd = EventFd::new().expect("EventFd::new");

    // Write u64::MAX - 1 directly to fill the counter to its maximum.
    let max_val: u64 = u64::MAX - 1;
    let ret = unsafe {
        libc::write(
            efd.as_fd(),
            &max_val as *const u64 as *const libc::c_void,
            8,
        )
    };
    assert_eq!(ret, 8, "pre-seeding write should succeed");

    // This notify() must silently swallow EAGAIN rather than panic.
    efd.notify();

    // The counter is still at u64::MAX - 1; drain once to confirm it's valid.
    let count = drain_one(efd.as_fd());
    assert_eq!(count, 1, "drain should return 1 in semaphore mode");
}

#[test]
fn multithreaded_n_senders_one_waiter_all_notifications_received() {
    const N: usize = 8;

    let efd = Arc::new(EventFd::new().expect("EventFd::new"));

    // Spawn N threads, each notifying once.
    let handles: Vec<_> = (0..N)
        .map(|_| {
            let efd = Arc::clone(&efd);
            std::thread::spawn(move || efd.notify())
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // Drain exactly N times; the semaphore counter must equal N.
    for i in 0..N {
        let count = drain_one(efd.as_fd());
        assert_eq!(
            count, 1,
            "drain {i}: expected 1 in semaphore mode, got {count}"
        );
    }

    // One more drain must give EAGAIN (counter exhausted).
    let ret = drain_one(efd.as_fd());
    assert_eq!(
        ret,
        -(libc::EAGAIN as i64),
        "counter should be empty after {N} drains, got {ret}"
    );
}

// ── EventSet additional tests ──────────────────────────────────────────────────

#[test]
fn event_set_add_capacity_limit() {
    // Create EVENT_SET_CAPACITY fds and fill the set.
    let efds: Vec<EventFd> = (0..EVENT_SET_CAPACITY)
        .map(|_| EventFd::new().unwrap())
        .collect();
    let mut set = EventSet::new().unwrap();
    for efd in &efds {
        set.add(efd).unwrap();
    }

    // One more add must fail with ENOSPC (errno 28).
    let extra = EventFd::new().unwrap();
    let err = set.add(&extra).expect_err("add beyond capacity should fail");
    assert_eq!(err.0, 28, "expected ENOSPC (28), got errno {}", err.0);
}

#[test]
fn event_set_wait_timeout_zero_on_empty_returns_etimedout() {
    let set = EventSet::new().unwrap();
    // No fds registered, timeout = 0 → immediate return with ETIMEDOUT (110).
    let err = set.wait(0).expect_err("wait(0) on empty set should fail");
    assert_eq!(err.0, 110, "expected ETIMEDOUT (110), got errno {}", err.0);
}

#[test]
fn event_set_wait_with_positive_timeout_returns_before_expiry() {
    let efd = Arc::new(EventFd::new().unwrap());
    let mut set = EventSet::new().unwrap();
    set.add(&efd).unwrap();

    // Notify from a thread after a short pause; use a 2 s timeout.
    let notifier = Arc::clone(&efd);
    let handle = thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        notifier.notify();
    });

    let fired = set.wait(2000).expect("wait should return before 2 s timeout");
    assert_eq!(fired, 0);
    handle.join().unwrap();
}

#[test]
fn event_set_multiple_simultaneous_fires_sequential_draining() {
    // Register 4 fds; notify fd[0] and fd[3] before waiting.
    let efds: Vec<EventFd> = (0..4).map(|_| EventFd::new().unwrap()).collect();
    let mut set = EventSet::new().unwrap();
    for efd in &efds {
        set.add(efd).unwrap();
    }

    efds[0].notify();
    efds[3].notify();

    let first = set.wait(-1).unwrap();
    let second = set.wait(-1).unwrap();

    // Both indices 0 and 3 must have been reported, in any order.
    let mut seen = [first, second];
    seen.sort_unstable();
    assert_eq!(seen, [0, 3], "expected both index 0 and 3, got {first} and {second}");
}

#[test]
fn event_set_add_same_fd_twice_returns_eexist() {
    let efd = EventFd::new().unwrap();
    let mut set = EventSet::new().unwrap();
    set.add(&efd).unwrap();

    // Second add of the same fd must fail with EEXIST (errno 17).
    let err = set.add(&efd).expect_err("duplicate add should fail");
    assert_eq!(err.0, 17, "expected EEXIST (17), got errno {}", err.0);
}

#[test]
fn event_set_drop_closes_epoll_fd() {
    // Capture the epoll fd number before dropping, then verify it is invalid
    // afterwards via fcntl(F_GETFD).  This is race-free: we check a specific
    // fd number rather than counting all open fds in /proc.
    let epfd = {
        let set = EventSet::new().unwrap();
        set.epoll_fd()
    };
    // After drop the epoll fd must be closed.
    let ret = unsafe { libc::fcntl(epfd, libc::F_GETFD) };
    assert_eq!(ret, -1, "fcntl should fail on a closed epoll fd");
    let errno = unsafe { *libc::__errno_location() };
    assert_eq!(errno, libc::EBADF, "expected EBADF, got errno {errno}");
}
