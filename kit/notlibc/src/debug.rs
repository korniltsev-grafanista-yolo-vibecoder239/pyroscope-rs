//! Debug output helpers using raw syscalls.
//!
//! Output is gated behind the `debug-print` Cargo feature (disabled by default).

#[cfg(all(feature = "debug-print", target_arch = "x86_64", target_os = "linux"))]
use crate::syscall_nr::x86_64::SYS_WRITE;
#[cfg(all(feature = "debug-print", target_arch = "x86_64", target_os = "linux"))]
const STDOUT: usize = 1;

/// Write a string to stdout followed by a newline.
///
/// This is a no-op unless the `debug-print` feature is enabled.
#[inline(always)]
pub fn puts(s: &str) {
    #[cfg(feature = "debug-print")]
    {
        writes(s);
        writes("\n");
    }
    #[cfg(not(feature = "debug-print"))]
    let _ = s;
}

/// Write a string to stdout without a trailing newline.
///
/// This is a no-op unless the `debug-print` feature is enabled.
#[inline(always)]
pub fn writes(s: &str) {
    #[cfg(all(feature = "debug-print", target_arch = "x86_64", target_os = "linux"))]
    unsafe {
        crate::syscall::syscall3(SYS_WRITE, STDOUT, s.as_ptr() as usize, s.len());
    }
    #[cfg(not(all(feature = "debug-print", target_arch = "x86_64", target_os = "linux")))]
    let _ = s;
}

/// Write a `usize` value as lowercase hex digits to stdout.
///
/// No `0x` prefix is emitted; callers should use `writes("0x")` before this
/// if the prefix is desired.
///
/// This is a no-op unless the `debug-print` feature is enabled.
#[inline(always)]
pub fn write_hex(v: usize) {
    #[cfg(all(feature = "debug-print", target_arch = "x86_64", target_os = "linux"))]
    unsafe {
        const HEX: &[u8] = b"0123456789abcdef";
        let mut buf = [0u8; 16];
        let mut i = 16usize;
        let mut n = v;
        loop {
            i -= 1;
            buf[i] = HEX[n & 0xf];
            n >>= 4;
            if n == 0 {
                break;
            }
        }
        crate::syscall::syscall3(SYS_WRITE, STDOUT, buf.as_ptr().add(i) as usize, 16 - i);
    }
    #[cfg(not(all(feature = "debug-print", target_arch = "x86_64", target_os = "linux")))]
    let _ = v;
}
