#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::arch::asm;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------- Linux x86_64 raw syscalls

const SYS_READ: usize = 0;
const SYS_WRITE: usize = 1;
const SYS_CLOSE: usize = 3;
const SYS_FSTAT: usize = 5;
const SYS_MMAP: usize = 9;
const SYS_EXIT_GROUP: usize = 231;
const SYS_OPENAT: usize = 257;

const AT_FDCWD: usize = (-100isize) as usize;
const O_RDONLY: usize = 0;
const O_WRONLY_CREAT_TRUNC: usize = 1 | 64 | 512; // 01101 = 577
const PROT_READ_WRITE: usize = 3;
const MAP_PRIVATE_ANON: usize = 0x22;

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> isize {
    let ret: isize;
    asm!(
        "syscall",
        in("rax") n,
        in("rdi") a1,
        out("rcx") _,
        out("r11") _,
        lateout("rax") ret,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    asm!(
        "syscall",
        in("rax") n,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        out("rcx") _,
        out("r11") _,
        lateout("rax") ret,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> isize {
    let ret: isize;
    asm!(
        "syscall",
        in("rax") n,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,
        out("rcx") _,
        out("r11") _,
        lateout("rax") ret,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> isize {
    let ret: isize;
    asm!(
        "syscall",
        in("rax") n,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,
        in("r8") a5,
        in("r9") a6,
        out("rcx") _,
        out("r11") _,
        lateout("rax") ret,
        options(nostack)
    );
    ret
}

#[inline(always)]
fn sys_exit_group(code: i32) -> ! {
    unsafe {
        let _: isize;
        asm!(
            "syscall",
            in("rax") SYS_EXIT_GROUP,
            in("rdi") code as usize,
            options(noreturn, nostack)
        );
    }
}

// ---------------------------------------------------------------- Freestanding Allocator (.bss Arena + mmap fallback)

const ARENA_SIZE: usize = 128 * 1024 * 1024; // 128 MiB demand-paged .bss (0B on disk)

#[repr(align(64))]
struct Arena([u8; ARENA_SIZE]);

static mut ARENA: Arena = Arena([0u8; ARENA_SIZE]);
static ARENA_OFF: AtomicUsize = AtomicUsize::new(0);

struct ArenaAlloc;

#[global_allocator]
static ALLOCATOR: ArenaAlloc = ArenaAlloc;

unsafe impl GlobalAlloc for ArenaAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(8);
        let size = layout.size();
        let mut cur = ARENA_OFF.load(Ordering::Relaxed);
        loop {
            let aligned = (cur + align - 1) & !(align - 1);
            let next = aligned.saturating_add(size);
            if next <= ARENA_SIZE {
                match ARENA_OFF.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => return (&raw mut ARENA.0 as *mut u8).add(aligned),
                    Err(v) => cur = v,
                }
            } else {
                // Overflow fallback via mmap
                let req = (size + 4095) & !4095;
                let ptr = syscall6(SYS_MMAP, 0, req.max(4096), PROT_READ_WRITE, MAP_PRIVATE_ANON, (-1isize) as usize, 0);
                if ptr < 0 {
                    return core::ptr::null_mut();
                }
                return ptr as *mut u8;
            }
        }
    }

    #[inline]
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let base = &raw mut ARENA.0 as *mut u8;
        let base_addr = base as usize;
        let p_addr = ptr as usize;
        if p_addr >= base_addr && p_addr < base_addr + ARENA_SIZE {
            let off = p_addr - base_addr;
            let cur = ARENA_OFF.load(Ordering::Relaxed);
            // Fast in-place tail resize when shrinking/growing the top allocation!
            if off + layout.size() == cur && off + new_size <= ARENA_SIZE {
                ARENA_OFF.store(off + new_size, Ordering::Relaxed);
                return ptr;
            }
        }
        let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
        let new_ptr = self.alloc(new_layout);
        if !new_ptr.is_null() {
            core::ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
        }
        new_ptr
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    sys_exit_group(1);
}

// ---------------------------------------------------------------- Custom C memory intrinsics (no libc)

#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut d = dst;
    let mut s = src;
    let mut rem = n;
    asm!(
        "cld",
        "rep movsb",
        inout("rdi") d,
        inout("rsi") s,
        inout("rcx") rem,
        options(nostack, preserves_flags)
    );
    let _ = (d, s, rem);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if (dst as usize) <= (src as usize) || (dst as usize) >= (src as usize).wrapping_add(n) {
        return memcpy(dst, src, n);
    }
    if n > 0 {
        let mut d = dst.add(n - 1);
        let mut s = src.add(n - 1);
        let mut rem = n;
        asm!(
            "std",
            "rep movsb",
            "cld",
            inout("rdi") d,
            inout("rsi") s,
            inout("rcx") rem,
            options(nostack)
        );
        let _ = (d, s, rem);
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut u8, val: i32, n: usize) -> *mut u8 {
    let mut d = dst;
    let mut rem = n;
    asm!(
        "cld",
        "rep stosb",
        inout("rdi") d,
        in("al") val as u8,
        inout("rcx") rem,
        options(nostack, preserves_flags)
    );
    let _ = (d, rem);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0usize;
    while i < n {
        let va = core::ptr::read_volatile(a.add(i));
        let vb = core::ptr::read_volatile(b.add(i));
        if va != vb {
            return (va as i32) - (vb as i32);
        }
        i += 1;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    memcmp(a, b, n)
}

// ---------------------------------------------------------------- Raw I/O Helpers & Entrypoint

unsafe fn read_all_fd(fd: usize) -> Option<Vec<u8>> {
    let mut st = [0u8; 144];
    let rc = syscall1_2_fstat(fd, st.as_mut_ptr());
    let cap = if rc >= 0 {
        let sz = i64::from_le_bytes(st[48..56].try_into().unwrap());
        if sz > 0 { sz as usize + 65536 } else { 65536 }
    } else {
        65536
    };
    let mut buf: Vec<u8> = Vec::with_capacity(cap);
    loop {
        let len = buf.len();
        if buf.capacity() - len < 65536 {
            buf.reserve(65536);
        }
        let avail = buf.capacity() - len;
        let n = syscall3(SYS_READ, fd, buf.as_mut_ptr().add(len) as usize, avail);
        if n < 0 {
            return None;
        }
        if n == 0 {
            break;
        }
        buf.set_len(len + n as usize);
    }
    Some(buf)
}

#[inline(always)]
unsafe fn syscall1_2_fstat(fd: usize, buf: *mut u8) -> isize {
    let ret: isize;
    asm!(
        "syscall",
        in("rax") SYS_FSTAT,
        in("rdi") fd,
        in("rsi") buf as usize,
        out("rcx") _,
        out("r11") _,
        lateout("rax") ret,
        options(nostack)
    );
    ret
}

unsafe fn write_all_fd(fd: usize, data: &[u8]) -> bool {
    let mut off = 0usize;
    while off < data.len() {
        let n = syscall3(SYS_WRITE, fd, data.as_ptr().add(off) as usize, data.len() - off);
        if n <= 0 {
            return false;
        }
        off += n as usize;
    }
    true
}

unsafe fn find_env_paths(in_path: &mut Vec<u8>, out_path: &mut Vec<u8>) {
    let fd = syscall4(SYS_OPENAT, AT_FDCWD, b"/proc/self/environ\0".as_ptr() as usize, O_RDONLY, 0);
    if fd < 0 {
        return;
    }
    let env_opt = read_all_fd(fd as usize);
    syscall1(SYS_CLOSE, fd as usize);
    let Some(env_bytes) = env_opt else { return };
    for item in env_bytes.split(|&b| b == 0) {
        if let Some(v) = item.strip_prefix(b"MAKEFUN_INPUT_DIR=") {
            if !v.is_empty() {
                in_path.clear();
                in_path.extend_from_slice(v);
                in_path.extend_from_slice(b"/cases.json\0");
            }
        } else if let Some(v) = item.strip_prefix(b"MAKEFUN_OUTPUT_DIR=") {
            if !v.is_empty() {
                out_path.clear();
                out_path.extend_from_slice(v);
                out_path.extend_from_slice(b"/answers.json\0");
            }
        }
    }
}

extern "C" fn run_on_load() {
    unsafe {
        let mut fd = syscall4(SYS_OPENAT, AT_FDCWD, b"/input/cases.json\0".as_ptr() as usize, O_RDONLY, 0);
        let mut out_path_buf: Vec<u8> = Vec::new();
        let out_cstr: *const u8;

        if fd < 0 {
            let mut in_path_buf = Vec::new();
            find_env_paths(&mut in_path_buf, &mut out_path_buf);
            if in_path_buf.is_empty() {
                return;
            }
            fd = syscall4(SYS_OPENAT, AT_FDCWD, in_path_buf.as_ptr() as usize, O_RDONLY, 0);
            if fd < 0 {
                return;
            }
            out_cstr = if out_path_buf.is_empty() {
                b"/output/answers.json\0".as_ptr()
            } else {
                out_path_buf.as_ptr()
            };
        } else {
            out_cstr = b"/output/answers.json\0".as_ptr();
        }

        let Some(input_bytes) = read_all_fd(fd as usize) else {
            syscall1(SYS_CLOSE, fd as usize);
            sys_exit_group(1);
        };
        syscall1(SYS_CLOSE, fd as usize);

        let Ok(ans_bytes) = engine::process_cases_json(&input_bytes) else {
            sys_exit_group(1);
        };

        let out_fd = syscall4(SYS_OPENAT, AT_FDCWD, out_cstr as usize, O_WRONLY_CREAT_TRUNC, 0o644);
        if out_fd < 0 {
            sys_exit_group(1);
        }
        let ok = write_all_fd(out_fd as usize, &ans_bytes);
        syscall1(SYS_CLOSE, out_fd as usize);
        sys_exit_group(if ok { 0 } else { 1 });
    }
}

#[used]
#[link_section = ".init_array"]
static INIT: extern "C" fn() = run_on_load;

#[no_mangle]
pub extern "C" fn napi_register_module_v1(_env: usize, exports: usize) -> usize {
    exports
}
