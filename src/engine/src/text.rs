use alloc::vec::Vec;
use core::cmp::Ordering;
use crate::nfkd_table::{is_empty_nfkd, lookup_map};
use crate::out::ByteSink;
use crate::simd::{
    escape_html_sse2, escape_html_then_json_sse2, escape_json_sse2, find_html_json_special,
    find_json_special, words_of_sse2,
};

pub const SLUG_MAX: usize = 64;

const MONTHS: [&[u8]; 12] = [
    b"January", b"February", b"March", b"April", b"May", b"June",
    b"July", b"August", b"September", b"October", b"November", b"December",
];

const ASCII_SLUG: [u8; 128] = {
    let mut t = [0u8; 128];
    let mut c = b'0';
    while c <= b'9' {
        t[c as usize] = c;
        c += 1;
    }
    let mut c = b'a';
    while c <= b'z' {
        t[c as usize] = c;
        c += 1;
    }
    let mut c = b'A';
    while c <= b'Z' {
        t[c as usize] = c + 32;
        c += 1;
    }
    t
};

#[inline(always)]
pub fn decode_utf8(b: &[u8], i: usize) -> (u32, usize) {
    let b0 = b[i] as u32;
    if b0 < 0x80 {
        (b0, 1)
    } else if b0 < 0xE0 && i + 1 < b.len() {
        let cp = ((b0 & 0x1F) << 6) | (b[i + 1] as u32 & 0x3F);
        (cp, 2)
    } else if b0 < 0xF0 && i + 2 < b.len() {
        let cp = ((b0 & 0x0F) << 12)
            | ((b[i + 1] as u32 & 0x3F) << 6)
            | (b[i + 2] as u32 & 0x3F);
        (cp, 3)
    } else if i + 3 < b.len() {
        let cp = ((b0 & 0x07) << 18)
            | ((b[i + 1] as u32 & 0x3F) << 12)
            | ((b[i + 2] as u32 & 0x3F) << 6)
            | (b[i + 3] as u32 & 0x3F);
        (cp, 4)
    } else {
        (0xFFFD, 1)
    }
}

/// Fast 64-bit word hash for hash tables in `site.rs`.
#[inline]
pub fn fast_hash(bytes: &[u8]) -> u64 {
    let mut h = 0x9e3779b97f4a7c15u64 ^ ((bytes.len() as u64).wrapping_mul(0x517cc1b727220a95));
    let mut i = 0usize;
    let ptr = bytes.as_ptr();
    while i + 8 <= bytes.len() {
        let w = unsafe { (ptr.add(i) as *const u64).read_unaligned() };
        h = (h ^ w).wrapping_mul(0x517cc1b727220a95).rotate_left(13);
        i += 8;
    }
    while i < bytes.len() {
        h = (h ^ unsafe { *ptr.add(i) as u64 }).wrapping_mul(0x100000001b3);
        i += 1;
    }
    h ^ (h >> 33)
}

/// Compare two UTF-8 byte slices in JS UTF-16 code-unit order (`a < b ? -1 : a > b ? 1 : 0`).
#[inline]
pub fn cmp_utf16(a: &[u8], b: &[u8]) -> Ordering {
    let min_len = a.len().min(b.len());
    let mut i = 0;
    while i + 8 <= min_len {
        let wa = unsafe { (a.as_ptr().add(i) as *const u64).read_unaligned() };
        let wb = unsafe { (b.as_ptr().add(i) as *const u64).read_unaligned() };
        if wa != wb {
            break;
        }
        i += 8;
    }
    while i < min_len && a[i] == b[i] {
        i += 1;
    }
    if i == min_len {
        return a.len().cmp(&b.len());
    }
    if (a[i] & 0xC0) != 0x80 && (b[i] & 0xC0) != 0x80 && a[i] < 0xEE && b[i] < 0xEE {
        return a[i].cmp(&b[i]);
    }
    while i > 0 && (a[i] & 0xC0) == 0x80 {
        i -= 1;
    }
    let mut ia = i;
    let mut ib = i;
    while ia < a.len() && ib < b.len() {
        let (ca, la) = decode_utf8(a, ia);
        let (cb, lb) = decode_utf8(b, ib);
        ia += la;
        ib += lb;
        if ca == cb {
            continue;
        }
        let ha = if ca >= 0x10000 { 0xD800 + ((ca - 0x10000) >> 10) } else { ca };
        let hb = if cb >= 0x10000 { 0xD800 + ((cb - 0x10000) >> 10) } else { cb };
        if ha != hb {
            return ha.cmp(&hb);
        }
        let ta = if ca >= 0x10000 { 0xDC00 + ((ca - 0x10000) & 0x3FF) } else { ca };
        let tb = if cb >= 0x10000 { 0xDC00 + ((cb - 0x10000) & 0x3FF) } else { cb };
        return ta.cmp(&tb);
    }
    (a.len() - ia).cmp(&(b.len() - ib))
}

#[inline(always)]
pub fn is_js_ws_cp(cp: u32) -> bool {
    if cp < 0x80 {
        matches!(cp, 0x09..=0x0D | 0x20)
    } else {
        matches!(
            cp,
            0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
        )
    }
}

#[inline]
pub fn has_js_ws(b: &[u8]) -> bool {
    let mut i = 0;
    while i < b.len() {
        if b[i] < 0x80 {
            if matches!(b[i], b'\t'..=b'\r' | b' ') {
                return true;
            }
            i += 1;
        } else {
            let (cp, l) = decode_utf8(b, i);
            if is_js_ws_cp(cp) {
                return true;
            }
            i += l;
        }
    }
    false
}

#[inline]
pub fn trim_js(b: &[u8]) -> &[u8] {
    let mut s = 0;
    while s < b.len() {
        if b[s] < 0x80 {
            if matches!(b[s], b'\t'..=b'\r' | b' ') {
                s += 1;
            } else {
                break;
            }
        } else {
            let (cp, l) = decode_utf8(b, s);
            if is_js_ws_cp(cp) {
                s += l;
            } else {
                break;
            }
        }
    }
    let mut e = b.len();
    while e > s {
        let b0 = b[e - 1];
        if b0 < 0x80 {
            if matches!(b0, b'\t'..=b'\r' | b' ') {
                e -= 1;
            } else {
                break;
            }
        } else {
            let mut cs = e - 1;
            while cs > s && (b[cs] & 0xC0) == 0x80 {
                cs -= 1;
            }
            let (cp, l) = decode_utf8(b, cs);
            if cs + l == e && is_js_ws_cp(cp) {
                e = cs;
            } else {
                break;
            }
        }
    }
    &b[s..e]
}

#[inline]
pub fn trim_ascii_ht_sp(b: &[u8]) -> &[u8] {
    let mut s = 0;
    while s < b.len() && (b[s] == b' ' || b[s] == b'\t') {
        s += 1;
    }
    let mut e = b.len();
    while e > s && (b[e - 1] == b' ' || b[e - 1] == b'\t') {
        e -= 1;
    }
    &b[s..e]
}

#[inline]
pub fn is_blank_ht_sp(b: &[u8]) -> bool {
    let mut i = 0;
    let len = b.len();
    while i < len {
        let c = b[i];
        if c != b' ' && c != b'\t' {
            return false;
        }
        i += 1;
    }
    true
}

/// Count words matching JS `text.match(/\S+/g)?.length ?? 0`.
#[inline]
pub fn words_of(text: &[u8]) -> usize {
    words_of_sse2(text)
}

#[inline(always)]
pub fn needs_html_or_json_escape(s: &[u8]) -> bool {
    find_html_json_special(s, 0) < s.len()
}

#[inline(always)]
pub fn needs_json_escape(s: &[u8]) -> bool {
    find_json_special(s, 0) < s.len()
}

#[inline]
pub fn escape_html_into<S: ByteSink>(s: &[u8], out: &mut S) {
    escape_html_sse2(s, out);
}

#[inline]
pub fn escape_json_into<S: ByteSink>(s: &[u8], out: &mut S) {
    escape_json_sse2(s, out);
}

#[inline]
pub fn escape_html_then_json_into<S: ByteSink>(s: &[u8], out: &mut S) {
    escape_html_then_json_sse2(s, out);
}

/// Exact Node 22 slugify matching `vendor/makefun-site/lib/text.js`.
pub fn slugify_into(text: &[u8], out: &mut Vec<u8>) {
    let base = out.len();
    let mut gap = false;
    let mut i = 0;
    while i < text.len() && out.len() - base <= SLUG_MAX {
        let c = text[i];
        if c < 0x80 {
            let mapped = ASCII_SLUG[c as usize];
            if mapped != 0 {
                if gap {
                    out.push(b'-');
                }
                gap = false;
                out.push(mapped);
            } else {
                gap = out.len() > base;
            }
            i += 1;
        } else {
            let (cp, l) = decode_utf8(text, i);
            i += l;
            if is_empty_nfkd(cp) {
                continue;
            }
            if let Some((flags, bytes)) = lookup_map(cp) {
                if (flags & 1) != 0 && out.len() > base {
                    gap = true;
                }
                if gap {
                    out.push(b'-');
                }
                out.extend_from_slice(bytes);
                gap = (flags & 2) != 0;
            } else {
                gap = out.len() > base;
            }
        }
    }
    if out.len() - base > SLUG_MAX {
        out.truncate(base + SLUG_MAX);
    }
    while out.len() > base && out[out.len() - 1] == b'-' {
        out.pop();
    }
    if out.len() == base {
        out.extend_from_slice(b"untitled");
    }
}

pub fn slugify_vec(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len().min(SLUG_MAX + 4).max(8));
    slugify_into(text, &mut out);
    out
}

/// Format `"YYYY-MM-DD"` as `"D Month YYYY"`.
#[inline]
pub fn long_date_into(date: &[u8], out: &mut Vec<u8>) {
    if date.len() < 10 {
        return;
    }
    let m = (date[5] - b'0') * 10 + (date[6] - b'0');
    let d = (date[8] - b'0') * 10 + (date[9] - b'0');
    push_usize(d as usize, out);
    out.push(b' ');
    if (1..=12).contains(&m) {
        out.extend_from_slice(MONTHS[(m - 1) as usize]);
    }
    out.push(b' ');
    out.extend_from_slice(&date[0..4]);
}

#[inline]
pub fn push_usize<S: ByteSink>(mut n: usize, out: &mut S) {
    let mut buf = [0u8; 20];
    let mut idx = 20;
    if n == 0 {
        out.push(b'0');
        return;
    }
    while n > 0 {
        idx -= 1;
        buf[idx] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.write_slice(&buf[idx..]);
}
