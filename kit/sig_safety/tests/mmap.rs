#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod tests {
    use sig_safety::mmap::{safe_mmap, safe_munmap};

    const PAGE_SIZE: usize = 4096;

    #[test]
    fn test_mmap_write_read() {
        let ptr = safe_mmap(PAGE_SIZE).expect("mmap should succeed");
        assert!(!ptr.is_null());

        // Write a pattern and read it back, verifying PROT_READ|PROT_WRITE works.
        let pattern: &[u8] = b"hello sig_safety";
        unsafe {
            core::ptr::copy_nonoverlapping(pattern.as_ptr(), ptr, pattern.len());
        }
        let mut buf = [0u8; 16];
        unsafe {
            core::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), buf.len());
        }
        assert_eq!(&buf, pattern);

        safe_munmap(ptr, PAGE_SIZE).expect("munmap should succeed");
    }

    #[test]
    fn test_munmap_ok() {
        let ptr = safe_mmap(PAGE_SIZE).expect("mmap should succeed");
        let result = safe_munmap(ptr, PAGE_SIZE);
        assert!(result.is_ok());
    }

    #[test]
    fn test_mmap_size_zero_returns_err() {
        // mmap(NULL, 0, ...) returns EINVAL (22) on Linux.
        let result = safe_mmap(0);
        assert!(result.is_err());
        let errno = result.unwrap_err();
        // Cross-check with libc.
        let expected = libc::EINVAL;
        assert_eq!(errno, expected, "expected EINVAL ({expected}), got {errno}");
    }
}
