// Baseline x86_64 SSE2 inlined SIMD byte scanners, bulk escapers, and word counter,
// with scalar fallback for non-x86_64 targets.

use crate::out::ByteSink;
use crate::text::{decode_utf8, is_js_ws_cp};

#[cfg(target_arch = "x86_64")]
#[inline(never)]
pub fn has_avx2() -> bool {
    // Diagnostic helper used by verify_win.exe; hot paths use zero-overhead inlined baseline SSE2.
    unsafe {
        let mut ecx1: u32;
        core::arch::asm!(
            "push rbx",
            "mov eax, 1",
            "xor ecx, ecx",
            "cpuid",
            "pop rbx",
            out("eax") _,
            lateout("ecx") ecx1,
            out("edx") _,
            options(nomem, preserves_flags)
        );
        if (ecx1 & ((1 << 27) | (1 << 28))) != ((1 << 27) | (1 << 28)) {
            return false;
        }
        let xcr0_lo: u32;
        core::arch::asm!(
            "xor ecx, ecx",
            "xgetbv",
            lateout("eax") xcr0_lo,
            out("edx") _,
            out("ecx") _,
            options(nomem, preserves_flags)
        );
        if (xcr0_lo & 0x6) != 0x6 {
            return false;
        }
        let ebx7: u32;
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
        (ebx7 & (1 << 5)) != 0
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub fn has_avx2() -> bool {
    false
}

/// Fast inlined SSE2 `memchr` for single byte `needle` from `start`.
#[inline(always)]
pub fn find_byte(b: &[u8], start: usize, needle: u8) -> usize {
    let len = b.len();
    let ptr = b.as_ptr();
    let mut i = start;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        if i + 16 <= len {
            let v = _mm_set1_epi8(needle as i8);
            while i + 16 <= len {
                let chunk = _mm_loadu_si128(ptr.add(i) as *const __m128i);
                let m = _mm_movemask_epi8(_mm_cmpeq_epi8(chunk, v)) as u32;
                if m != 0 {
                    return i + m.trailing_zeros() as usize;
                }
                i += 16;
            }
        }
    }
    while i < len {
        if unsafe { *ptr.add(i) } == needle {
            return i;
        }
        i += 1;
    }
    len
}

/// Find first index >= `start` in `b` matching `"` or `\` or `< 0x20` (control char).
#[inline(always)]
pub fn find_json_special(b: &[u8], start: usize) -> usize {
    let len = b.len();
    let ptr = b.as_ptr();
    let mut i = start;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        if i + 16 <= len {
            let vq = _mm_set1_epi8(b'"' as i8);
            let vs = _mm_set1_epi8(b'\\' as i8);
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
        }
    }
    while i < len {
        let c = unsafe { *ptr.add(i) };
        if c == b'"' || c == b'\\' || c < 0x20 {
            return i;
        }
        i += 1;
    }
    len
}

/// Find first index >= `start` in `b` matching HTML-escape char (`&`, `<`, `>`, `"`, `'`).
#[inline(always)]
pub fn find_html_special(b: &[u8], start: usize) -> usize {
    let len = b.len();
    let ptr = b.as_ptr();
    let mut i = start;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        if i + 16 <= len {
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
        }
    }
    while i < len {
        let c = unsafe { *ptr.add(i) };
        if c == b'&' || c == b'<' || c == b'>' || c == b'"' || c == b'\'' {
            return i;
        }
        i += 1;
    }
    len
}

/// Find first index >= `start` in `b` matching HTML-escape char (`&`, `<`, `>`, `"`, `'`) OR `\` OR `< 0x20`.
#[inline(always)]
pub fn find_html_json_special(b: &[u8], start: usize) -> usize {
    let len = b.len();
    let ptr = b.as_ptr();
    let mut i = start;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        if i + 16 <= len {
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
        }
    }
    while i < len {
        let c = unsafe { *ptr.add(i) };
        if c == b'&' || c == b'<' || c == b'>' || c == b'"' || c == b'\'' || c == b'\\' || c < 0x20 {
            return i;
        }
        i += 1;
    }
    len
}

/// Find first index >= `start` in `b` matching Markdown inline trigger OR HTML escape char:
/// `\`, `` ` ``, `*`, `[`, `&`, `<`, `>`, `"`, `'` (and optionally `< 0x20` when `JSON_MODE` is true).
#[inline(always)]
pub fn find_inline_special<const JSON_MODE: bool>(b: &[u8], start: usize) -> usize {
    let len = b.len();
    let ptr = b.as_ptr();
    let mut i = start;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        if i + 16 <= len {
            let vbs = _mm_set1_epi8(b'\\' as i8);
            let vbt = _mm_set1_epi8(b'`' as i8);
            let vst = _mm_set1_epi8(b'*' as i8);
            let vbr = _mm_set1_epi8(b'[' as i8);
            let va = _mm_set1_epi8(b'&' as i8);
            let vl = _mm_set1_epi8(b'<' as i8);
            let vg = _mm_set1_epi8(b'>' as i8);
            let vq = _mm_set1_epi8(b'"' as i8);
            let vp = _mm_set1_epi8(b'\'' as i8);
            let x80 = _mm_set1_epi8(-128i8);
            let lim = _mm_set1_epi8((0x20u8 ^ 0x80u8) as i8);
            while i + 16 <= len {
                let c = _mm_loadu_si128(ptr.add(i) as *const __m128i);
                let a = _mm_or_si128(_mm_cmpeq_epi8(c, vbs), _mm_cmpeq_epi8(c, vbt));
                let b2 = _mm_or_si128(_mm_cmpeq_epi8(c, vst), _mm_cmpeq_epi8(c, vbr));
                let c2 = _mm_or_si128(_mm_cmpeq_epi8(c, va), _mm_cmpeq_epi8(c, vl));
                let d = _mm_or_si128(_mm_cmpeq_epi8(c, vg), _mm_cmpeq_epi8(c, vq));
                let mut all = _mm_or_si128(
                    _mm_or_si128(a, b2),
                    _mm_or_si128(_mm_or_si128(c2, d), _mm_cmpeq_epi8(c, vp)),
                );
                if JSON_MODE {
                    let lt = _mm_cmpgt_epi8(lim, _mm_xor_si128(c, x80));
                    all = _mm_or_si128(all, lt);
                }
                let m = _mm_movemask_epi8(all) as u32;
                if m != 0 {
                    return i + m.trailing_zeros() as usize;
                }
                i += 16;
            }
        }
    }
    while i < len {
        let c = unsafe { *ptr.add(i) };
        if matches!(c, b'\\' | b'`' | b'*' | b'[' | b'&' | b'<' | b'>' | b'"' | b'\'')
            || (JSON_MODE && c < 0x20)
        {
            return i;
        }
        i += 1;
    }
    len
}

#[inline(always)]
pub fn push_json_escape_sink<S: ByteSink>(c: u8, out: &mut S) {
    match c {
        b'"' => out.write_slice(b"\\\""),
        b'\\' => out.write_slice(b"\\\\"),
        0x08 => out.write_slice(b"\\b"),
        0x09 => out.write_slice(b"\\t"),
        0x0A => out.write_slice(b"\\n"),
        0x0C => out.write_slice(b"\\f"),
        0x0D => out.write_slice(b"\\r"),
        _ => {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let esc = [
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[(c >> 4) as usize],
                HEX[(c & 0xF) as usize],
            ];
            out.write_slice(&esc);
        }
    }
}

/// Bulk SSE2 mask-draining JSON string escaper.
#[inline]
pub fn escape_json_sse2<S: ByteSink>(s: &[u8], out: &mut S) {
    let len = s.len();
    let ptr = s.as_ptr();
    let mut i = 0usize;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        if len >= 16 {
            let vq = _mm_set1_epi8(b'"' as i8);
            let vs = _mm_set1_epi8(b'\\' as i8);
            let x80 = _mm_set1_epi8(-128i8);
            let lim = _mm_set1_epi8((0x20u8 ^ 0x80u8) as i8);
            while i + 16 <= len {
                let c = _mm_loadu_si128(ptr.add(i) as *const __m128i);
                let eq = _mm_or_si128(_mm_cmpeq_epi8(c, vq), _mm_cmpeq_epi8(c, vs));
                let lt = _mm_cmpgt_epi8(lim, _mm_xor_si128(c, x80));
                let mut m = _mm_movemask_epi8(_mm_or_si128(eq, lt)) as u32;
                if m == 0 {
                    out.write_slice(core::slice::from_raw_parts(ptr.add(i), 16));
                } else {
                    let mut cur = 0usize;
                    while m != 0 {
                        let tz = m.trailing_zeros() as usize;
                        if tz > cur {
                            out.write_slice(core::slice::from_raw_parts(ptr.add(i + cur), tz - cur));
                        }
                        push_json_escape_sink(*ptr.add(i + tz), out);
                        cur = tz + 1;
                        m &= m - 1;
                    }
                    if cur < 16 {
                        out.write_slice(core::slice::from_raw_parts(ptr.add(i + cur), 16 - cur));
                    }
                }
                i += 16;
            }
        }
    }
    let mut run_start = i;
    while i < len {
        let c = unsafe { *ptr.add(i) };
        if c == b'"' || c == b'\\' || c < 0x20 {
            if i > run_start {
                out.write_slice(unsafe { core::slice::from_raw_parts(ptr.add(run_start), i - run_start) });
            }
            push_json_escape_sink(c, out);
            run_start = i + 1;
        }
        i += 1;
    }
    if len > run_start {
        out.write_slice(unsafe { core::slice::from_raw_parts(ptr.add(run_start), len - run_start) });
    }
}

/// Bulk SSE2 mask-draining HTML escaper.
#[inline]
pub fn escape_html_sse2<S: ByteSink>(s: &[u8], out: &mut S) {
    let len = s.len();
    let ptr = s.as_ptr();
    let mut i = 0usize;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        if len >= 16 {
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
                let mut m = _mm_movemask_epi8(m3) as u32;
                if m == 0 {
                    out.write_slice(core::slice::from_raw_parts(ptr.add(i), 16));
                } else {
                    let mut cur = 0usize;
                    while m != 0 {
                        let tz = m.trailing_zeros() as usize;
                        if tz > cur {
                            out.write_slice(core::slice::from_raw_parts(ptr.add(i + cur), tz - cur));
                        }
                        match *ptr.add(i + tz) {
                            b'&' => out.write_slice(b"&amp;"),
                            b'<' => out.write_slice(b"&lt;"),
                            b'>' => out.write_slice(b"&gt;"),
                            b'"' => out.write_slice(b"&quot;"),
                            _ => out.write_slice(b"&#39;"),
                        }
                        cur = tz + 1;
                        m &= m - 1;
                    }
                    if cur < 16 {
                        out.write_slice(core::slice::from_raw_parts(ptr.add(i + cur), 16 - cur));
                    }
                }
                i += 16;
            }
        }
    }
    let mut run_start = i;
    while i < len {
        let c = unsafe { *ptr.add(i) };
        if c == b'&' || c == b'<' || c == b'>' || c == b'"' || c == b'\'' {
            if i > run_start {
                out.write_slice(unsafe { core::slice::from_raw_parts(ptr.add(run_start), i - run_start) });
            }
            match c {
                b'&' => out.write_slice(b"&amp;"),
                b'<' => out.write_slice(b"&lt;"),
                b'>' => out.write_slice(b"&gt;"),
                b'"' => out.write_slice(b"&quot;"),
                _ => out.write_slice(b"&#39;"),
            }
            run_start = i + 1;
        }
        i += 1;
    }
    if len > run_start {
        out.write_slice(unsafe { core::slice::from_raw_parts(ptr.add(run_start), len - run_start) });
    }
}

/// Bulk SSE2 mask-draining fused HTML+JSON escaper.
#[inline]
pub fn escape_html_then_json_sse2<S: ByteSink>(s: &[u8], out: &mut S) {
    let len = s.len();
    let ptr = s.as_ptr();
    let mut i = 0usize;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        if len >= 16 {
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
                let mut m = _mm_movemask_epi8(all) as u32;
                if m == 0 {
                    out.write_slice(core::slice::from_raw_parts(ptr.add(i), 16));
                } else {
                    let mut cur = 0usize;
                    while m != 0 {
                        let tz = m.trailing_zeros() as usize;
                        if tz > cur {
                            out.write_slice(core::slice::from_raw_parts(ptr.add(i + cur), tz - cur));
                        }
                        match *ptr.add(i + tz) {
                            b'&' => out.write_slice(b"&amp;"),
                            b'<' => out.write_slice(b"&lt;"),
                            b'>' => out.write_slice(b"&gt;"),
                            b'"' => out.write_slice(b"&quot;"),
                            b'\'' => out.write_slice(b"&#39;"),
                            bch => push_json_escape_sink(bch, out),
                        }
                        cur = tz + 1;
                        m &= m - 1;
                    }
                    if cur < 16 {
                        out.write_slice(core::slice::from_raw_parts(ptr.add(i + cur), 16 - cur));
                    }
                }
                i += 16;
            }
        }
    }
    let mut run_start = i;
    while i < len {
        let c = unsafe { *ptr.add(i) };
        if c == b'&' || c == b'<' || c == b'>' || c == b'"' || c == b'\'' || c == b'\\' || c < 0x20 {
            if i > run_start {
                out.write_slice(unsafe { core::slice::from_raw_parts(ptr.add(run_start), i - run_start) });
            }
            match c {
                b'&' => out.write_slice(b"&amp;"),
                b'<' => out.write_slice(b"&lt;"),
                b'>' => out.write_slice(b"&gt;"),
                b'"' => out.write_slice(b"&quot;"),
                b'\'' => out.write_slice(b"&#39;"),
                bch => push_json_escape_sink(bch, out),
            }
            run_start = i + 1;
        }
        i += 1;
    }
    if len > run_start {
        out.write_slice(unsafe { core::slice::from_raw_parts(ptr.add(run_start), len - run_start) });
    }
}

/// Exact JS `text.match(/\S+/g)?.length ?? 0` using SSE2 on pure-ASCII 16-byte chunks
/// and exact UTF-8 fallback whenever any high bit is set in a chunk.
#[inline]
pub fn words_of_sse2(text: &[u8]) -> usize {
    let len = text.len();
    let ptr = text.as_ptr();
    let mut count = 0usize;
    let mut in_word: u32 = 0;
    let mut i = 0usize;

    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        if len >= 16 {
            let vsp = _mm_set1_epi8(b' ' as i8);
            let v8 = _mm_set1_epi8(8i8);
            let v14 = _mm_set1_epi8(14i8);
            while i + 16 <= len {
                let c = _mm_loadu_si128(ptr.add(i) as *const __m128i);
                if _mm_movemask_epi8(c) == 0 {
                    let is_sp = _mm_cmpeq_epi8(c, vsp);
                    let is_ctrl = _mm_and_si128(_mm_cmpgt_epi8(c, v8), _mm_cmpgt_epi8(v14, c));
                    let ws_mask = _mm_movemask_epi8(_mm_or_si128(is_sp, is_ctrl)) as u32;
                    let non_ws = (!ws_mask) & 0xFFFF;
                    let starts = non_ws & !((non_ws << 1) | in_word);
                    count += starts.count_ones() as usize;
                    in_word = (non_ws >> 15) & 1;
                    i += 16;
                } else {
                    let chunk_end = i + 16;
                    while i < chunk_end {
                        let b0 = *ptr.add(i);
                        if b0 < 0x80 {
                            let ws = b0 == b' ' || (b0 >= 0x09 && b0 <= 0x0D);
                            if ws {
                                in_word = 0;
                            } else if in_word == 0 {
                                in_word = 1;
                                count += 1;
                            }
                            i += 1;
                        } else {
                            let (cp, l) = decode_utf8(text, i);
                            if is_js_ws_cp(cp) {
                                in_word = 0;
                            } else if in_word == 0 {
                                in_word = 1;
                                count += 1;
                            }
                            i += l;
                        }
                    }
                }
            }
        }
    }

    while i < len {
        let b0 = unsafe { *ptr.add(i) };
        if b0 < 0x80 {
            let ws = b0 == b' ' || (b0 >= 0x09 && b0 <= 0x0D);
            if ws {
                in_word = 0;
            } else if in_word == 0 {
                in_word = 1;
                count += 1;
            }
            i += 1;
        } else {
            let (cp, l) = decode_utf8(text, i);
            if is_js_ws_cp(cp) {
                in_word = 0;
            } else if in_word == 0 {
                in_word = 1;
                count += 1;
            }
            i += l;
        }
    }
    count
}
