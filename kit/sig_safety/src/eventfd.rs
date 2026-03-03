#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
use crate::syscall::{syscall2, syscall3};

/// SYS_write on x86-64.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
const SYS_WRITE: usize = 1;

/// SYS_eventfd2 on x86-64.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
const SYS_EVENTFD2: usize = 290;

/// `EFD_NONBLOCK | EFD_SEMAPHORE`
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
const EFD_FLAGS: usize = 0x800 | 0x1;

/// Creates a non-blocking semaphore-mode eventfd via `SYS_eventfd2`.
///
/// Returns `Ok(fd)` on success, `Err(errno)` on failure.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub fn eventfd_create() -> Result<i32, i32> {
    // SYS_eventfd2(initval=0, flags=EFD_NONBLOCK|EFD_SEMAPHORE)
    let ret = unsafe { syscall2(SYS_EVENTFD2, 0, EFD_FLAGS) };
    if ret >= 0 {
        Ok(ret as i32)
    } else {
        Err((-ret) as i32)
    }
}

/// Writes 1 to the eventfd counter to signal it via `SYS_write`.
///
/// Signal-safe: errors are intentionally ignored.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub fn eventfd_notify(fd: i32) {
    let val: u64 = 1;
    // SAFETY: `val` is a local on the stack; its address is valid for the
    // duration of the syscall.  We intentionally discard the return value
    // because this function is designed to be called from signal handlers
    // where there is nothing useful we can do on error.
    unsafe {
        syscall3(
            SYS_WRITE,
            fd as usize,
            &val as *const u64 as usize,
            8, // size_of::<u64>()
        );
    }
}
