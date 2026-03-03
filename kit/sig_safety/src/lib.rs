#![no_std]

mod errno_guard;
mod mmap;
mod syscall;

pub mod eventfd;
pub use eventfd::{EventFd, EventSet, EVENT_SET_CAPACITY};
