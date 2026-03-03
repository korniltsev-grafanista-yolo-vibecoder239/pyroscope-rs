//! Integration tests for `sig_safety::eventfd`.
//!
//! These tests use libc only in the test harness (dev-dependency) to drain
//! the eventfd and verify counts; production code uses no libc.

#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use sig_safety::eventfd::{eventfd_create, eventfd_notify};

/// Drain one unit from a semaphore-mode eventfd.
/// Returns the value read (always 1 for semaphore mode) or the negative errno.
unsafe fn drain_one(fd: i32) -> i64 {
    let mut buf: u64 = 0;
    let ret = unsafe { libc::read(fd, &mut buf as *mut u64 as *mut libc::c_void, 8) };
    if ret < 0 {
        // Return negative errno so callers can detect EAGAIN.
        unsafe { -((*libc::__errno_location()) as i64) }
    } else {
        buf as i64
    }
}

/// Close fd unconditionally; used in test teardown.
unsafe fn close(fd: i32) {
    unsafe { libc::close(fd) };
}

#[test]
fn create_returns_valid_fd() {
    let fd = eventfd_create().expect("eventfd_create should succeed");
    assert!(fd >= 0, "fd must be non-negative, got {fd}");
    unsafe { close(fd) };
}

#[test]
fn notify_once_drain_reads_one() {
    let fd = eventfd_create().expect("eventfd_create");
    eventfd_notify(fd);
    let val = unsafe { drain_one(fd) };
    assert_eq!(val, 1, "semaphore drain should return 1");
    unsafe { close(fd) };
}

#[test]
fn notify_twice_drain_twice_accumulates() {
    let fd = eventfd_create().expect("eventfd_create");
    eventfd_notify(fd);
    eventfd_notify(fd);
    // Semaphore mode: each read returns 1 and decrements by 1.
    let first = unsafe { drain_one(fd) };
    let second = unsafe { drain_one(fd) };
    assert_eq!(first, 1, "first drain should return 1");
    assert_eq!(second, 1, "second drain should return 1");
    unsafe { close(fd) };
}

#[test]
fn non_blocking_second_read_returns_eagain() {
    let fd = eventfd_create().expect("eventfd_create");
    eventfd_notify(fd);
    // Drain the single notification.
    let _ = unsafe { drain_one(fd) };
    // A second read on an empty non-blocking eventfd must return EAGAIN.
    let ret = unsafe { drain_one(fd) };
    assert_eq!(
        ret,
        -(libc::EAGAIN as i64),
        "second read on empty non-blocking eventfd should be EAGAIN, got {ret}"
    );
    unsafe { close(fd) };
}
