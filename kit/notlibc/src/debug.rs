//! Debug output helpers using raw syscalls.
//!
//! Output is gated behind the `debug-print` Cargo feature (disabled by default).

/// Write a string to stdout followed by a newline.
///
/// This is a no-op unless the `debug-print` feature is enabled.
#[cfg(feature = "debug-print")]
#[inline(always)]
pub fn puts(s: &str) {
    writes(s);
    writes("\n");
}

/// Write a string to stdout followed by a newline.
///
/// This is a no-op unless the `debug-print` feature is enabled.
#[cfg(not(feature = "debug-print"))]
#[inline(always)]
pub fn puts(_s: &str) {}

/// Write a string to stdout without a trailing newline.
///
/// This is a no-op unless the `debug-print` feature is enabled.
#[cfg(feature = "debug-print")]
#[inline(always)]
pub fn writes(s: &str) {
    const STDOUT: usize = 1;
    unsafe {
        crate::syscall::syscall3(
            crate::syscall_nr::x86_64::SYS_WRITE,
            STDOUT,
            s.as_ptr() as usize,
            s.len(),
        );
    }
}

/// Write a string to stdout without a trailing newline.
///
/// This is a no-op unless the `debug-print` feature is enabled.
#[cfg(not(feature = "debug-print"))]
#[inline(always)]
pub fn writes(_s: &str) {}

/// Write a `usize` value as lowercase hex digits to stdout.
///
/// No `0x` prefix is emitted; callers should use `writes("0x")` before this
/// if the prefix is desired.
///
/// This is a no-op unless the `debug-print` feature is enabled.
#[cfg(feature = "debug-print")]
#[inline(always)]
pub fn write_hex(v: usize) {
    const STDOUT: usize = 1;
    const HEX: &[u8] = b"0123456789abcdef";
    unsafe {
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
        crate::syscall::syscall3(
            crate::syscall_nr::x86_64::SYS_WRITE,
            STDOUT,
            buf.as_ptr().add(i) as usize,
            16 - i,
        );
    }
}

/// Write a `usize` value as lowercase hex digits to stdout.
///
/// No `0x` prefix is emitted; callers should use `writes("0x")` before this
/// if the prefix is desired.
///
/// This is a no-op unless the `debug-print` feature is enabled.
#[cfg(not(feature = "debug-print"))]
#[inline(always)]
pub fn write_hex(_v: usize) {}
