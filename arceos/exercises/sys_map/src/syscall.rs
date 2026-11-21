#![allow(dead_code)]

use core::ffi::{c_void, c_char, c_int};
use axhal::arch::TrapFrame;
use axhal::trap::{register_trap_handler, SYSCALL};
use axerrno::LinuxError;
use axtask::current;
use axtask::TaskExtRef;
use axhal::paging::MappingFlags;
use arceos_posix_api as api;
use alloc::vec::Vec;
use memory_addr::{VirtAddr, VirtAddrRange, PAGE_SIZE_4K};

const SYS_IOCTL: usize = 29;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_WRITEV: usize = 66;
const SYS_EXIT: usize = 93;
const SYS_EXIT_GROUP: usize = 94;
const SYS_SET_TID_ADDRESS: usize = 96;
const SYS_MMAP: usize = 222;

const AT_FDCWD: i32 = -100;

/// Macro to generate syscall body
///
/// It will receive a function which return Result<_, LinuxError> and convert it to
/// the type which is specified by the caller.
#[macro_export]
macro_rules! syscall_body {
    ($fn: ident, $($stmt: tt)*) => {{
        #[allow(clippy::redundant_closure_call)]
        let res = (|| -> axerrno::LinuxResult<_> { $($stmt)* })();
        match res {
            Ok(_) | Err(axerrno::LinuxError::EAGAIN) => debug!(concat!(stringify!($fn), " => {:?}"),  res),
            Err(_) => info!(concat!(stringify!($fn), " => {:?}"), res),
        }
        match res {
            Ok(v) => v as _,
            Err(e) => {
                -e.code() as _
            }
        }
    }};
}

bitflags::bitflags! {
    #[derive(Debug)]
    /// permissions for sys_mmap
    ///
    /// See <https://github.com/bminor/glibc/blob/master/bits/mman.h>
    struct MmapProt: i32 {
        /// Page can be read.
        const PROT_READ = 1 << 0;
        /// Page can be written.
        const PROT_WRITE = 1 << 1;
        /// Page can be executed.
        const PROT_EXEC = 1 << 2;
    }
}

impl From<MmapProt> for MappingFlags {
    fn from(value: MmapProt) -> Self {
        let mut flags = MappingFlags::USER;
        if value.contains(MmapProt::PROT_READ) {
            flags |= MappingFlags::READ;
        }
        if value.contains(MmapProt::PROT_WRITE) {
            flags |= MappingFlags::WRITE;
        }
        if value.contains(MmapProt::PROT_EXEC) {
            flags |= MappingFlags::EXECUTE;
        }
        flags
    }
}

bitflags::bitflags! {
    #[derive(Debug)]
    /// flags for sys_mmap
    ///
    /// See <https://github.com/bminor/glibc/blob/master/bits/mman.h>
    struct MmapFlags: i32 {
        /// Share changes
        const MAP_SHARED = 1 << 0;
        /// Changes private; implemented here by an eager copy, not copy-on-write.
        const MAP_PRIVATE = 1 << 1;
        /// Map address must be exactly as requested, no matter whether it is available.
        const MAP_FIXED = 1 << 4;
        /// Don't use a file.
        const MAP_ANONYMOUS = 1 << 5;
        /// Don't check for reservations.
        const MAP_NORESERVE = 1 << 14;
        /// Allocation is for a stack.
        const MAP_STACK = 0x20000;
    }
}

#[register_trap_handler(SYSCALL)]
fn handle_syscall(tf: &TrapFrame, syscall_num: usize) -> isize {
    ax_println!("handle_syscall [{}] ...", syscall_num);
    let ret = match syscall_num {
         SYS_IOCTL => sys_ioctl(tf.arg0() as _, tf.arg1() as _, tf.arg2() as _) as _,
        SYS_SET_TID_ADDRESS => sys_set_tid_address(tf.arg0() as _),
        SYS_OPENAT => sys_openat(tf.arg0() as _, tf.arg1() as _, tf.arg2() as _, tf.arg3() as _),
        SYS_CLOSE => sys_close(tf.arg0() as _),
        SYS_READ => sys_read(tf.arg0() as _, tf.arg1() as _, tf.arg2() as _),
        SYS_WRITE => sys_write(tf.arg0() as _, tf.arg1() as _, tf.arg2() as _),
        SYS_WRITEV => sys_writev(tf.arg0() as _, tf.arg1() as _, tf.arg2() as _),
        SYS_EXIT_GROUP => {
            ax_println!("[SYS_EXIT_GROUP]: system is exiting ..");
            axtask::exit(tf.arg0() as _)
        },
        SYS_EXIT => {
            ax_println!("[SYS_EXIT]: system is exiting ..");
            axtask::exit(tf.arg0() as _)
        },
        SYS_MMAP => sys_mmap(
            tf.arg0() as _,
            tf.arg1() as _,
            tf.arg2() as _,
            tf.arg3() as _,
            tf.arg4() as _,
            tf.arg5() as _,
        ),
        _ => {
            ax_println!("Unimplemented syscall: {}", syscall_num);
            -LinuxError::ENOSYS.code() as _
        }
    };
    ret
}

// MAP_FIXED only accepts vacant ranges because AddrSpace::unmap retains area metadata.
fn sys_mmap(
    addr: *mut usize,
    length: usize,
    prot: i32,
    flags: i32,
    fd: i32,
    offset: isize,
) -> isize {
    syscall_body!(sys_mmap, {
        let prot = MmapProt::from_bits(prot).ok_or(LinuxError::EINVAL)?;
        let flags = MmapFlags::from_bits(flags).ok_or(LinuxError::EINVAL)?;
        let page_mask = PAGE_SIZE_4K - 1;
        if length == 0 || offset < 0 || (offset as usize & page_mask) != 0 {
            return Err(LinuxError::EINVAL);
        }
        let size = length
            .checked_add(page_mask)
            .ok_or(LinuxError::ENOMEM)?
            & !page_mask;
        let private = flags.contains(MmapFlags::MAP_PRIVATE);
        let shared = flags.contains(MmapFlags::MAP_SHARED);
        if private == shared {
            return Err(LinuxError::EINVAL);
        }
        if shared || flags.intersects(MmapFlags::MAP_NORESERVE | MmapFlags::MAP_STACK) {
            return Err(LinuxError::EOPNOTSUPP);
        }
        // A RISC-V leaf requires R or X; W without R is reserved.
        if prot.is_empty()
            || (prot.contains(MmapProt::PROT_WRITE) && !prot.contains(MmapProt::PROT_READ))
        {
            return Err(LinuxError::EOPNOTSUPP);
        }
        let anonymous = flags.contains(MmapFlags::MAP_ANONYMOUS);
        if anonymous {
            // The descriptor is ignored for anonymous mappings.
            if offset != 0 {
                return Err(LinuxError::EINVAL);
            }
        } else {
            (offset as usize)
                .checked_add(size)
                .filter(|end| *end <= isize::MAX as usize)
                .ok_or(LinuxError::EOVERFLOW)?;
        }

        let fixed = flags.contains(MmapFlags::MAP_FIXED);
        let requested = addr as usize; // Treat the user pointer only as an address.
        if fixed && requested & page_mask != 0 {
            return Err(LinuxError::EINVAL);
        }
        let curr = current();
        let mut aspace = curr.task_ext().aspace.lock();
        // Exclude page zero and addresses that would look like negative errno.
        let lower = aspace
            .base()
            .as_usize()
            .max(PAGE_SIZE_4K)
            .checked_add(page_mask)
            .ok_or(LinuxError::ENOMEM)?
            & !page_mask;
        let upper = aspace.end().as_usize().min(isize::MAX as usize) & !page_mask;
        if size > upper.checked_sub(lower).ok_or(LinuxError::ENOMEM)? {
            return Err(LinuxError::ENOMEM);
        }
        let hint = requested & !page_mask;
        let valid_hint = hint >= lower && hint.checked_add(size).is_some_and(|end| end <= upper);
        if fixed && !valid_hint {
            return Err(LinuxError::ENOMEM);
        }
        // A restricted search tests the exact hint without replacing any area.
        let at_hint = if valid_hint {
            aspace
                .find_free_area(
                    VirtAddr::from(hint),
                    size,
                    VirtAddrRange::from_start_size(VirtAddr::from(hint), size),
                )
                .filter(|start| start.as_usize() == hint)
        } else {
            None
        };
        let start = if fixed {
            at_hint.ok_or(LinuxError::EOPNOTSUPP)?
        } else {
            // Unavailable, NULL or out-of-range hints fall back to a user gap.
            at_hint
                .or_else(|| {
                    aspace.find_free_area(
                        VirtAddr::from(lower),
                        size,
                        VirtAddrRange::from_start_size(VirtAddr::from(lower), upper - lower),
                    )
                })
                .ok_or(LinuxError::ENOMEM)?
        };

        // Finish file I/O before adding an area that cannot be rolled back.
        let mut data = Vec::new();
        if !anonymous {
            data.try_reserve_exact(size)
                .map_err(|_| LinuxError::ENOMEM)?;
            data.resize(size, 0);
            api::read_file_at(fd, &mut data, offset as u64)?;
        }
        aspace
            .map_alloc(start, size, prot.into(), true)
            .map_err(|_| LinuxError::ENOMEM)?;
        // The populated backend can report success after skipping a failed frame allocation.
        for off in (0..size).step_by(PAGE_SIZE_4K) {
            aspace
                .page_table()
                .query(start + off)
                .map_err(|_| LinuxError::ENOMEM)?;
        }
        if !data.is_empty() {
            // The kernel initializes read-only user pages through its physical mapping.
            aspace.write(start, &data)?;
        }
        axhal::arch::flush_tlb(None);
        #[cfg(target_arch = "riscv64")]
        unsafe {
            core::arch::asm!("fence.i", options(nostack));
        }
        Ok(start.as_usize())
    })
}

fn sys_openat(dfd: c_int, fname: *const c_char, flags: c_int, mode: api::ctypes::mode_t) -> isize {
    assert_eq!(dfd, AT_FDCWD);
    api::sys_open(fname, flags, mode) as isize
}

fn sys_close(fd: i32) -> isize {
    api::sys_close(fd) as isize
}

fn sys_read(fd: i32, buf: *mut c_void, count: usize) -> isize {
    api::sys_read(fd, buf, count)
}

fn sys_write(fd: i32, buf: *const c_void, count: usize) -> isize {
    api::sys_write(fd, buf, count)
}

fn sys_writev(fd: i32, iov: *const api::ctypes::iovec, iocnt: i32) -> isize {
    unsafe { api::sys_writev(fd, iov, iocnt) }
}

fn sys_set_tid_address(tid_ptd: *const i32) -> isize {
    let curr = current();
    curr.task_ext().set_clear_child_tid(tid_ptd as _);
    curr.id().as_u64() as isize
}

fn sys_ioctl(_fd: i32, _op: usize, _argp: *mut c_void) -> i32 {
    ax_println!("Ignore SYS_IOCTL");
    0
}
