#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod imp {
    const SYS_MMAP: usize = 9;
    const SYS_MUNMAP: usize = 11;

    const PROT_READ: usize = 1;
    const PROT_WRITE: usize = 2;
    const MAP_PRIVATE: usize = 0x02;
    const MAP_ANONYMOUS: usize = 0x20;

    /// Allocates anonymous private mapping via SYS_mmap.
    pub fn safe_mmap(size: usize) -> Result<*mut u8, i32> {
        let ret: usize;
        unsafe {
            core::arch::asm!(
                "syscall",
                in("rax") SYS_MMAP,
                in("rdi") 0usize,                       // addr = NULL
                in("rsi") size,                          // length
                in("rdx") PROT_READ | PROT_WRITE,        // prot
                in("r10") MAP_PRIVATE | MAP_ANONYMOUS,   // flags
                in("r8") usize::MAX,                     // fd = -1
                in("r9") 0usize,                         // offset = 0
                lateout("rax") ret,
                out("rcx") _,
                out("r11") _,
            );
        }
        if ret >= usize::MAX - 4095 {
            Err(-(ret as i32))
        } else {
            Ok(ret as *mut u8)
        }
    }

    /// Unmaps a previously mapped region via SYS_munmap.
    pub fn safe_munmap(addr: *mut u8, size: usize) -> Result<(), i32> {
        let ret: usize;
        unsafe {
            core::arch::asm!(
                "syscall",
                in("rax") SYS_MUNMAP,
                in("rdi") addr as usize,
                in("rsi") size,
                lateout("rax") ret,
                out("rcx") _,
                out("r11") _,
            );
        }
        if ret >= usize::MAX - 4095 {
            Err(-(ret as i32))
        } else {
            Ok(())
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub use imp::{safe_mmap, safe_munmap};
