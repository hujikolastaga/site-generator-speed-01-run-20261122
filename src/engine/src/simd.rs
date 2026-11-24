// Runtime CPUID-dispatched AVX2 + baseline SSE2 SIMD byte scanners (x86_64)
// and scalar fallback for non-x86_64 targets.

use core::sync::atomic::{AtomicU8, Ordering};

// 0 = uninitialized, 1 = SSE2 only, 2 = AVX2 supported
static AVX2_STATE: AtomicU8 = AtomicU8::new(0);

#[inline(always)]
pub fn has_avx2() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        let st = AVX2_STATE.load(Ordering::Relaxed);
        if st != 0 {
            return st == 2;
        }
        detect_avx2()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

#[cfg(target_arch = "x86_64")]
#[cold]
#[inline(never)]
fn detect_avx2() -> bool {
    let ok = unsafe {
        let mut eax1: u32;
        let mut ecx1: u32;
        core::arch::asm!(
            "push rbx",
            "mov eax, 1",
            "xor ecx, ecx",
            "cpuid",
            "pop rbx",
            lateout("eax") eax1,
            lateout("ecx") ecx1,
            lateout("edx") _,
            options(nomem, preserves_flags)
        );
        let _ = eax1;
        // Bit 28 = AVX, Bit 27 = OSXSAVE
        if (ecx1 & ((1 << 27) | (1 << 28))) != ((1 << 27) | (1 << 28)) {
            false
        } else {
            let xcr0_lo: u32;
            core::arch::asm!(
                "xor ecx, ecx",
                "xgetbv",
                lateout("eax") xcr0_lo,
                lateout("edx") _,
                out("ecx") _,
                options(nomem, preserves_flags)
            );
            if (xcr0_lo & 0x6) != 0x6 {
                false
            } else {
                let mut ebx7: u32;
                core::arch::asm!(
                    "push rbx",
                    "mov eax, 7",
                    "xor ecx, ecx",
                    "cpuid",
                    "mov {out:e}, ebx",
                    "pop rbx",
                    out = out(reg) ebx7,
                    out("eax") _,
                    out("ecx") _,
                    out("edx") _,
                    options(nomem, preserves_flags)
                );
                // Bit 5 of EBX = AVX2
                (ebx7 & (1 << 5)) != 0
            }
        }
    };
    AVX2_STATE.store(if ok { 2 } else { 1 }, Ordering::Relaxed);
    ok
}

/// Find first index >= `start` in `b` matching `"` or `\` or `< 0x20` (control char).
#[inline(always)]
pub fn find_json_special(b: &[u8], start: usize) -> usize {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        if has_avx2() {
            return find_json_special_avx2(b, start);
        }
        return find_json_special_sse2(b, start);
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut i = start;
        while i < b.len() {
            let c = b[i];
            if c == b'"' || c == b'\\' || c < 0x20 {
                return i;
            }
            i += 1;
        }
        b.len()
    }
}

/// Find first index >= `start` in `b` matching any HTML-escape char (`&`, `<`, `>`, `"`, `'`) OR `< 0x20` OR `\`.
#[inline(always)]
pub fn find_html_json_special(b: &[u8], start: usize) -> usize {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        if has_avx2() {
            return find_html_json_special_avx2(b, start);
        }
        return find_html_json_special_sse2(b, start);
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut i = start;
        while i < b.len() {
            let c = b[i];
            if c == b'&' || c == b'<' || c == b'>' || c == b'"' || c == b'\'' || c == b'\\' || c < 0x20 {
                return i;
            }
            i += 1;
        }
        b.len()
    }
}

/// Find first index >= `start` in `b` matching HTML-escape char (`&`, `<`, `>`, `"`, `'`).
#[inline(always)]
pub fn find_html_special(b: &[u8], start: usize) -> usize {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        if has_avx2() {
            return find_html_special_avx2(b, start);
        }
        return find_html_special_sse2(b, start);
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut i = start;
        while i < b.len() {
            let c = b[i];
            if c == b'&' || c == b'<' || c == b'>' || c == b'"' || c == b'\'' {
                return i;
            }
            i += 1;
        }
        b.len()
    }
}

/// Find first index >= `start` in `b` matching Markdown inline trigger OR HTML escape char:
/// `\`, `` ` ``, `*`, `[`, `&`, `<`, `>`, `"`, `'`.
#[inline(always)]
pub fn find_inline_special(b: &[u8], start: usize) -> usize {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        if has_avx2() {
            return find_inline_special_avx2(b, start);
        }
        return find_inline_special_sse2(b, start);
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut i = start;
        while i < b.len() {
            let c = b[i];
            if matches!(c, b'\\' | b'`' | b'*' | b'[' | b'&' | b'<' | b'>' | b'"' | b'\'') {
                return i;
            }
            i += 1;
        }
        b.len()
    }
}

/// Fast SIMD `memchr` for single byte `needle` from `start`.
#[inline(always)]
pub fn find_byte(b: &[u8], start: usize, needle: u8) -> usize {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        if has_avx2() {
            return find_byte_avx2(b, start, needle);
        }
        return find_byte_sse2(b, start, needle);
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut i = start;
        while i < b.len() {
            if b[i] == needle {
                return i;
            }
            i += 1;
        }
        b.len()
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn find_byte_sse2(b: &[u8], mut i: usize, needle: u8) -> usize {
    use core::arch::x86_64::*;
    let len = b.len();
    let ptr = b.as_ptr();
    let v = _mm_set1_epi8(needle as i8);
    while i + 16 <= len {
        let chunk = _mm_loadu_si128(ptr.add(i) as *const __m128i);
        let m = _mm_movemask_epi8(_mm_cmpeq_epi8(chunk, v)) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 16;
    }
    while i < len {
        if *ptr.add(i) == needle {
            return i;
        }
        i += 1;
    }
    len
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn find_byte_avx2(b: &[u8], mut i: usize, needle: u8) -> usize {
    use core::arch::x86_64::*;
    let len = b.len();
    let ptr = b.as_ptr();
    let v = _mm256_set1_epi8(needle as i8);
    while i + 32 <= len {
        let chunk = _mm256_loadu_si256(ptr.add(i) as *const __m256i);
        let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(chunk, v)) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 32;
    }
    find_byte_sse2(b, i, needle)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn find_json_special_sse2(b: &[u8], mut i: usize) -> usize {
    use core::arch::x86_64::*;
    let len = b.len();
    let ptr = b.as_ptr();
    let vq = _mm_set1_epi8(b'"' as i8);
    let vs = _mm_set1_epi8(b'\\' as i8);
    // Unsigned < 0x20: (c ^ 0x80) < (0x20 ^ 0x80) = -96 signed, i.e. 0x9F signed > c^0x80 => cmpgt(-96, c^0x80)
    let x80 = _mm_set1_epi8(-128i8);
    let lim = _mm_set1_epi8((0x20u8 ^ 0x80u8) as i8);
    while i + 16 <= len {
        let c = _mm_loadu_si128(ptr.add(i) as *const __m128i);
        let eq = _mm_or_si128(_mm_cmpeq_epi8(c, vq), _mm_cmpeq_epi8(c, vs));
        let lt = _mm_cmpgt_epi8(lim, _mm_xor_si128(c, x80));
        let m = _mm_movemask_epi8(_mm_or_si128(eq, lt)) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 16;
    }
    while i < len {
        let c = *ptr.add(i);
        if c == b'"' || c == b'\\' || c < 0x20 {
            return i;
        }
        i += 1;
    }
    len
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn find_json_special_avx2(b: &[u8], mut i: usize) -> usize {
    use core::arch::x86_64::*;
    let len = b.len();
    let ptr = b.as_ptr();
    let vq = _mm256_set1_epi8(b'"' as i8);
    let vs = _mm256_set1_epi8(b'\\' as i8);
    let x80 = _mm256_set1_epi8(-128i8);
    let lim = _mm256_set1_epi8((0x20u8 ^ 0x80u8) as i8);
    while i + 32 <= len {
        let c = _mm256_loadu_si256(ptr.add(i) as *const __m256i);
        let eq = _mm256_or_si256(_mm256_cmpeq_epi8(c, vq), _mm256_cmpeq_epi8(c, vs));
        let lt = _mm256_cmpgt_epi8(lim, _mm256_xor_si256(c, x80));
        let m = _mm256_movemask_epi8(_mm256_or_si256(eq, lt)) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 32;
    }
    find_json_special_sse2(b, i)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn find_html_special_sse2(b: &[u8], mut i: usize) -> usize {
    use core::arch::x86_64::*;
    let len = b.len();
    let ptr = b.as_ptr();
    let va = _mm_set1_epi8(b'&' as i8);
    let vl = _mm_set1_epi8(b'<' as i8);
    let vg = _mm_set1_epi8(b'>' as i8);
    let vq = _mm_set1_epi8(b'"' as i8);
    let vp = _mm_set1_epi8(b'\'' as i8);
    while i + 16 <= len {
        let c = _mm_loadu_si128(ptr.add(i) as *const __m128i);
        let m1 = _mm_or_si128(_mm_cmpeq_epi8(c, va), _mm_cmpeq_epi8(c, vl));
        let m2 = _mm_or_si128(_mm_cmpeq_epi8(c, vg), _mm_cmpeq_epi8(c, vq));
        let m3 = _mm_or_si128(_mm_cmpeq_epi8(c, vp), _mm_or_si128(m1, m2));
        let m = _mm_movemask_epi8(m3) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 16;
    }
    while i < len {
        let c = *ptr.add(i);
        if c == b'&' || c == b'<' || c == b'>' || c == b'"' || c == b'\'' {
            return i;
        }
        i += 1;
    }
    len
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn find_html_special_avx2(b: &[u8], mut i: usize) -> usize {
    use core::arch::x86_64::*;
    let len = b.len();
    let ptr = b.as_ptr();
    let va = _mm256_set1_epi8(b'&' as i8);
    let vl = _mm256_set1_epi8(b'<' as i8);
    let vg = _mm256_set1_epi8(b'>' as i8);
    let vq = _mm256_set1_epi8(b'"' as i8);
    let vp = _mm256_set1_epi8(b'\'' as i8);
    while i + 32 <= len {
        let c = _mm256_loadu_si256(ptr.add(i) as *const __m256i);
        let m1 = _mm256_or_si256(_mm256_cmpeq_epi8(c, va), _mm256_cmpeq_epi8(c, vl));
        let m2 = _mm256_or_si256(_mm256_cmpeq_epi8(c, vg), _mm256_cmpeq_epi8(c, vq));
        let m3 = _mm256_or_si256(_mm256_cmpeq_epi8(c, vp), _mm256_or_si256(m1, m2));
        let m = _mm256_movemask_epi8(m3) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 32;
    }
    find_html_special_sse2(b, i)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn find_html_json_special_sse2(b: &[u8], mut i: usize) -> usize {
    use core::arch::x86_64::*;
    let len = b.len();
    let ptr = b.as_ptr();
    let va = _mm_set1_epi8(b'&' as i8);
    let vl = _mm_set1_epi8(b'<' as i8);
    let vg = _mm_set1_epi8(b'>' as i8);
    let vq = _mm_set1_epi8(b'"' as i8);
    let vp = _mm_set1_epi8(b'\'' as i8);
    let vs = _mm_set1_epi8(b'\\' as i8);
    let x80 = _mm_set1_epi8(-128i8);
    let lim = _mm_set1_epi8((0x20u8 ^ 0x80u8) as i8);
    while i + 16 <= len {
        let c = _mm_loadu_si128(ptr.add(i) as *const __m128i);
        let m1 = _mm_or_si128(_mm_cmpeq_epi8(c, va), _mm_cmpeq_epi8(c, vl));
        let m2 = _mm_or_si128(_mm_cmpeq_epi8(c, vg), _mm_cmpeq_epi8(c, vq));
        let m3 = _mm_or_si128(_mm_cmpeq_epi8(c, vp), _mm_cmpeq_epi8(c, vs));
        let lt = _mm_cmpgt_epi8(lim, _mm_xor_si128(c, x80));
        let all = _mm_or_si128(_mm_or_si128(m1, m2), _mm_or_si128(m3, lt));
        let m = _mm_movemask_epi8(all) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 16;
    }
    while i < len {
        let c = *ptr.add(i);
        if c == b'&' || c == b'<' || c == b'>' || c == b'"' || c == b'\'' || c == b'\\' || c < 0x20 {
            return i;
        }
        i += 1;
    }
    len
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn find_html_json_special_avx2(b: &[u8], mut i: usize) -> usize {
    use core::arch::x86_64::*;
    let len = b.len();
    let ptr = b.as_ptr();
    let va = _mm256_set1_epi8(b'&' as i8);
    let vl = _mm256_set1_epi8(b'<' as i8);
    let vg = _mm256_set1_epi8(b'>' as i8);
    let vq = _mm256_set1_epi8(b'"' as i8);
    let vp = _mm256_set1_epi8(b'\'' as i8);
    let vs = _mm256_set1_epi8(b'\\' as i8);
    let x80 = _mm256_set1_epi8(-128i8);
    let lim = _mm256_set1_epi8((0x20u8 ^ 0x80u8) as i8);
    while i + 32 <= len {
        let c = _mm256_loadu_si256(ptr.add(i) as *const __m256i);
        let m1 = _mm256_or_si256(_mm256_cmpeq_epi8(c, va), _mm256_cmpeq_epi8(c, vl));
        let m2 = _mm256_or_si256(_mm256_cmpeq_epi8(c, vg), _mm256_cmpeq_epi8(c, vq));
        let m3 = _mm256_or_si256(_mm256_cmpeq_epi8(c, vp), _mm256_cmpeq_epi8(c, vs));
        let lt = _mm256_cmpgt_epi8(lim, _mm256_xor_si256(c, x80));
        let all = _mm256_or_si256(_mm256_or_si256(m1, m2), _mm256_or_si256(m3, lt));
        let m = _mm256_movemask_epi8(all) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 32;
    }
    find_html_json_special_sse2(b, i)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn find_inline_special_sse2(b: &[u8], mut i: usize) -> usize {
    use core::arch::x86_64::*;
    let len = b.len();
    let ptr = b.as_ptr();
    let vbs = _mm_set1_epi8(b'\\' as i8);
    let vbt = _mm_set1_epi8(b'`' as i8);
    let vst = _mm_set1_epi8(b'*' as i8);
    let vbr = _mm_set1_epi8(b'[' as i8);
    let va = _mm_set1_epi8(b'&' as i8);
    let vl = _mm_set1_epi8(b'<' as i8);
    let vg = _mm_set1_epi8(b'>' as i8);
    let vq = _mm_set1_epi8(b'"' as i8);
    let vp = _mm_set1_epi8(b'\'' as i8);
    while i + 16 <= len {
        let c = _mm_loadu_si128(ptr.add(i) as *const __m128i);
        let a = _mm_or_si128(_mm_cmpeq_epi8(c, vbs), _mm_cmpeq_epi8(c, vbt));
        let b2 = _mm_or_si128(_mm_cmpeq_epi8(c, vst), _mm_cmpeq_epi8(c, vbr));
        let c2 = _mm_or_si128(_mm_cmpeq_epi8(c, va), _mm_cmpeq_epi8(c, vl));
        let d = _mm_or_si128(_mm_cmpeq_epi8(c, vg), _mm_cmpeq_epi8(c, vq));
        let all = _mm_or_si128(_mm_or_si128(a, b2), _mm_or_si128(_mm_or_si128(c2, d), _mm_cmpeq_epi8(c, vp)));
        let m = _mm_movemask_epi8(all) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 16;
    }
    while i < len {
        let c = *ptr.add(i);
        if matches!(c, b'\\' | b'`' | b'*' | b'[' | b'&' | b'<' | b'>' | b'"' | b'\'') {
            return i;
        }
        i += 1;
    }
    len
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn find_inline_special_avx2(b: &[u8], mut i: usize) -> usize {
    use core::arch::x86_64::*;
    let len = b.len();
    let ptr = b.as_ptr();
    let vbs = _mm256_set1_epi8(b'\\' as i8);
    let vbt = _mm256_set1_epi8(b'`' as i8);
    let vst = _mm256_set1_epi8(b'*' as i8);
    let vbr = _mm256_set1_epi8(b'[' as i8);
    let va = _mm256_set1_epi8(b'&' as i8);
    let vl = _mm256_set1_epi8(b'<' as i8);
    let vg = _mm256_set1_epi8(b'>' as i8);
    let vq = _mm256_set1_epi8(b'"' as i8);
    let vp = _mm256_set1_epi8(b'\'' as i8);
    while i + 32 <= len {
        let c = _mm256_loadu_si256(ptr.add(i) as *const __m256i);
        let a = _mm256_or_si256(_mm256_cmpeq_epi8(c, vbs), _mm256_cmpeq_epi8(c, vbt));
        let b2 = _mm256_or_si256(_mm256_cmpeq_epi8(c, vst), _mm256_cmpeq_epi8(c, vbr));
        let c2 = _mm256_or_si256(_mm256_cmpeq_epi8(c, va), _mm256_cmpeq_epi8(c, vl));
        let d = _mm256_or_si256(_mm256_cmpeq_epi8(c, vg), _mm256_cmpeq_epi8(c, vq));
        let all = _mm256_or_si256(_mm256_or_si256(a, b2), _mm256_or_si256(_mm256_or_si256(c2, d), _mm256_cmpeq_epi8(c, vp)));
        let m = _mm256_movemask_epi8(all) as u32;
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 32;
    }
    find_inline_special_sse2(b, i)
}
