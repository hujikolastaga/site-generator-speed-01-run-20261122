use alloc::vec::Vec;
use crate::simd::{find_byte, find_inline_special, push_json_escape_sink};
use crate::text::{
    decode_utf8, escape_html_into, escape_html_then_json_into, has_js_ws, is_blank_ht_sp,
    is_js_ws_cp, push_usize, slugify_into, trim_ascii_ht_sp,
};

pub struct TocItem {
    pub level: u8,
    pub id: Vec<u8>,
    pub html: Vec<u8>,
}

pub struct MdScratch {
    pub line_ranges: Vec<(u32, u32)>,
    pub html: Vec<u8>,
    pub toc: Vec<TocItem>,
    pub excerpt_range: Option<(usize, usize)>,
    pub used_ids_pool: Vec<u8>,
    pub used_ids_ranges: Vec<(u32, u32)>,
    pub slug_buf: Vec<u8>,
    pub join_buf: Vec<u8>,
    pub item_buf: Vec<u8>,
    pub href_tmp: Vec<u8>,
}

impl MdScratch {
    pub fn new() -> Self {
        Self {
            line_ranges: Vec::with_capacity(128),
            html: Vec::with_capacity(16 * 1024),
            toc: Vec::with_capacity(16),
            excerpt_range: None,
            used_ids_pool: Vec::with_capacity(512),
            used_ids_ranges: Vec::with_capacity(24),
            slug_buf: Vec::with_capacity(72),
            join_buf: Vec::with_capacity(512),
            item_buf: Vec::with_capacity(512),
            href_tmp: Vec::with_capacity(128),
        }
    }

    #[inline]
    pub fn excerpt_slice(&self) -> &[u8] {
        match self.excerpt_range {
            Some((s, e)) => &self.html[s..e],
            None => b"",
        }
    }
}

#[inline(always)]
fn is_escapable(c: u8) -> bool {
    matches!(
        c,
        b'\\' | b'`' | b'*' | b'_' | b'[' | b']' | b'(' | b')' | b'#' | b'+' | b'-' | b'.' | b'!' | b'>'
    )
}

#[inline]
fn fence_open_lang(line: &[u8]) -> Option<&[u8]> {
    if line.len() < 3 || &line[..3] != b"```" {
        return None;
    }
    let mut i = 3;
    while i < line.len()
        && (line[i].is_ascii_alphanumeric() || matches!(line[i], b'_' | b'+' | b'-'))
    {
        i += 1;
    }
    if is_blank_ht_sp(&line[i..]) {
        Some(&line[3..i])
    } else {
        None
    }
}

#[inline(always)]
fn is_fence_close(line: &[u8]) -> bool {
    line.len() >= 3 && &line[..3] == b"```" && is_blank_ht_sp(&line[3..])
}

#[inline]
fn heading_of(line: &[u8]) -> Option<(u8, &[u8])> {
    let mut d = 0usize;
    while d < line.len() && d < 7 && line[d] == b'#' {
        d += 1;
    }
    if d == 0 || d > 6 || d >= line.len() || (line[d] != b' ' && line[d] != b'\t') {
        return None;
    }
    let mut i = d + 1;
    while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
        i += 1;
    }
    let mut end = line.len();
    while end > i && (line[end - 1] == b' ' || line[end - 1] == b'\t') {
        end -= 1;
    }
    if i >= end {
        None
    } else {
        Some((d as u8, &line[i..end]))
    }
}

#[inline(always)]
fn is_hr(line: &[u8]) -> bool {
    line.len() >= 3
        && (&line[..3] == b"---" || &line[..3] == b"***")
        && is_blank_ht_sp(&line[3..])
}

#[inline(always)]
fn bullet_rest(line: &[u8]) -> Option<&[u8]> {
    if line.len() >= 2 && (line[0] == b'-' || line[0] == b'*') && line[1] == b' ' {
        Some(&line[2..])
    } else {
        None
    }
}

#[inline]
fn ordered_rest(line: &[u8]) -> Option<(usize, &[u8])> {
    let mut d = 0usize;
    while d < line.len() && d < 10 && line[d].is_ascii_digit() {
        d += 1;
    }
    if d == 0 || d > 9 || d + 1 >= line.len() || line[d] != b'.' || line[d + 1] != b' ' {
        return None;
    }
    let mut n = 0usize;
    for &b in &line[..d] {
        n = n * 10 + (b - b'0') as usize;
    }
    Some((n, &line[d + 2..]))
}

#[inline]
fn is_continued(line: &[u8]) -> bool {
    if line.len() < 3 || line[0] != b' ' || line[1] != b' ' {
        return false;
    }
    let mut i = 2;
    while i < line.len() && line[i] == b' ' {
        i += 1;
    }
    if i >= line.len() {
        return false;
    }
    let (cp, _) = decode_utf8(line, i);
    !is_js_ws_cp(cp)
}

#[inline]
fn ends_paragraph(line: &[u8]) -> bool {
    is_blank_ht_sp(line)
        || fence_open_lang(line).is_some()
        || heading_of(line).is_some()
        || is_hr(line)
}

#[inline]
fn find_unescaped_byte(s: &[u8], needle: u8, from: usize) -> Option<usize> {
    let mut j = from;
    while j < s.len() {
        if s[j] == b'\\' && j + 1 < s.len() && is_escapable(s[j + 1]) {
            j += 2;
            continue;
        }
        if s[j] == needle {
            return Some(j);
        }
        j += 1;
    }
    None
}

#[inline]
fn find_unescaped_double_star(s: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j + 1 < s.len() {
        if s[j] == b'\\' && is_escapable(s[j + 1]) {
            j += 2;
            continue;
        }
        if s[j] == b'*' && s[j + 1] == b'*' {
            return Some(j);
        }
        j += 1;
    }
    None
}

#[inline]
fn find_single_star(s: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j < s.len() {
        let c = s[j];
        if c == b'\\' && j + 1 < s.len() && is_escapable(s[j + 1]) {
            j += 2;
            continue;
        }
        if c == b'*' {
            if j + 1 < s.len() && s[j + 1] == b'*' {
                j += 2;
                continue;
            }
            return Some(j);
        }
        j += 1;
    }
    None
}

pub fn render_inline_into<const JSON_MODE: bool, F>(
    s: &[u8],
    link_fn: &mut F,
    href_tmp: &mut Vec<u8>,
    out: &mut Vec<u8>,
) where
    F: FnMut(&[u8], &mut Vec<u8>),
{
    let mut i = 0usize;
    let len = s.len();
    while i < len {
        let j = find_inline_special::<JSON_MODE>(s, i);
        if j > i {
            out.extend_from_slice(&s[i..j]);
        }
        if j == len {
            break;
        }
        i = j;
        let c = s[i];
        match c {
            b'&' => {
                out.extend_from_slice(b"&amp;");
                i += 1;
            }
            b'<' => {
                out.extend_from_slice(b"&lt;");
                i += 1;
            }
            b'>' => {
                out.extend_from_slice(b"&gt;");
                i += 1;
            }
            b'"' => {
                out.extend_from_slice(b"&quot;");
                i += 1;
            }
            b'\'' => {
                out.extend_from_slice(b"&#39;");
                i += 1;
            }
            b'\\' => {
                if i + 1 < len && is_escapable(s[i + 1]) {
                    let nb = s[i + 1];
                    if nb == b'\\' {
                        out.extend_from_slice(if JSON_MODE { b"\\\\" } else { b"\\" });
                    } else if nb == b'>' {
                        out.extend_from_slice(b"&gt;");
                    } else {
                        out.push(nb);
                    }
                    i += 2;
                } else {
                    out.extend_from_slice(if JSON_MODE { b"\\\\" } else { b"\\" });
                    i += 1;
                }
            }
            b'`' => {
                let close = find_byte(s, i + 1, b'`');
                if close < len && close > i + 1 {
                    out.extend_from_slice(b"<code>");
                    if JSON_MODE {
                        escape_html_then_json_into(&s[i + 1..close], out);
                    } else {
                        escape_html_into(&s[i + 1..close], out);
                    }
                    out.extend_from_slice(b"</code>");
                    i = close + 1;
                } else {
                    out.push(b'`');
                    i += 1;
                }
            }
            b'*' => {
                if i + 1 < len && s[i + 1] == b'*' {
                    if let Some(close) = find_unescaped_double_star(s, i + 2) {
                        if close > i + 2 {
                            out.extend_from_slice(b"<strong>");
                            render_inline_into::<JSON_MODE, F>(
                                &s[i + 2..close],
                                link_fn,
                                href_tmp,
                                out,
                            );
                            out.extend_from_slice(b"</strong>");
                            i = close + 2;
                            continue;
                        }
                    }
                    out.extend_from_slice(b"**");
                    i += 2;
                } else if let Some(close) = find_single_star(s, i + 1) {
                    if close > i + 1 {
                        out.extend_from_slice(b"<em>");
                        render_inline_into::<JSON_MODE, F>(
                            &s[i + 1..close],
                            link_fn,
                            href_tmp,
                            out,
                        );
                        out.extend_from_slice(b"</em>");
                        i = close + 1;
                    } else {
                        out.push(b'*');
                        i += 1;
                    }
                } else {
                    out.push(b'*');
                    i += 1;
                }
            }
            b'[' => {
                if let Some(k) = find_unescaped_byte(s, b']', i + 1) {
                    if k + 1 < len && s[k + 1] == b'(' {
                        let m = find_byte(s, k + 2, b')');
                        if m < len {
                            let target = &s[k + 2..m];
                            if !target.is_empty() && !has_js_ws(target) {
                                href_tmp.clear();
                                link_fn(target, href_tmp);
                                if JSON_MODE {
                                    out.extend_from_slice(b"<a href=\\\"");
                                    escape_html_then_json_into(href_tmp, out);
                                    out.extend_from_slice(b"\\\">");
                                } else {
                                    out.extend_from_slice(b"<a href=\"");
                                    escape_html_into(href_tmp, out);
                                    out.extend_from_slice(b"\">");
                                }
                                render_inline_into::<JSON_MODE, F>(
                                    &s[i + 1..k],
                                    link_fn,
                                    href_tmp,
                                    out,
                                );
                                out.extend_from_slice(b"</a>");
                                i = m + 1;
                                continue;
                            }
                        }
                    }
                }
                out.push(b'[');
                i += 1;
            }
            ctrl if JSON_MODE && ctrl < 0x20 => {
                push_json_escape_sink(ctrl, out);
                i += 1;
            }
            _ => unreachable!(),
        }
    }
}

pub fn render_markdown_into<const JSON_MODE: bool, F>(
    src: &[u8],
    link_fn: &mut F,
    scratch: &mut MdScratch,
) where
    F: FnMut(&[u8], &mut Vec<u8>),
{
    scratch.line_ranges.clear();
    scratch.html.clear();
    scratch.html.reserve(src.len() * 2 + 128);
    scratch.toc.clear();
    scratch.excerpt_range = None;
    scratch.used_ids_pool.clear();
    scratch.used_ids_ranges.clear();

    // Scan all newlines using SSE2 in one pass.
    let len = src.len();
    let ptr = src.as_ptr();
    let mut line_start = 0usize;
    let mut scan = 0usize;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        if len >= 16 {
            let vnl = _mm_set1_epi8(b'\n' as i8);
            while scan + 16 <= len {
                let c = _mm_loadu_si128(ptr.add(scan) as *const __m128i);
                let mut m = _mm_movemask_epi8(_mm_cmpeq_epi8(c, vnl)) as u32;
                while m != 0 {
                    let tz = m.trailing_zeros() as usize;
                    let nl_pos = scan + tz;
                    scratch.line_ranges.push((line_start as u32, nl_pos as u32));
                    line_start = nl_pos + 1;
                    m &= m - 1;
                }
                scan += 16;
            }
        }
    }
    while scan < len {
        if unsafe { *ptr.add(scan) } == b'\n' {
            scratch.line_ranges.push((line_start as u32, scan as u32));
            line_start = scan + 1;
        }
        scan += 1;
    }
    scratch.line_ranges.push((line_start as u32, len as u32));

    let nlines = scratch.line_ranges.len();
    let ranges = &scratch.line_ranges;
    let mut i = 0usize;

    #[inline(always)]
    fn line_slice<'s>(src: &'s [u8], ranges: &[(u32, u32)], idx: usize) -> &'s [u8] {
        let (a, b) = unsafe { *ranges.get_unchecked(idx) };
        unsafe { src.get_unchecked(a as usize..b as usize) }
    }

    while i < nlines {
        let line = line_slice(src, ranges, i);
        if is_blank_ht_sp(line) {
            i += 1;
            continue;
        }
        if let Some(lang) = fence_open_lang(line) {
            i += 1;
            scratch.html.extend_from_slice(b"<pre><code");
            if !lang.is_empty() {
                if JSON_MODE {
                    scratch.html.extend_from_slice(b" class=\\\"language-");
                    escape_html_then_json_into(lang, &mut scratch.html);
                    scratch.html.extend_from_slice(b"\\\"");
                } else {
                    scratch.html.extend_from_slice(b" class=\"language-");
                    escape_html_into(lang, &mut scratch.html);
                    scratch.html.push(b'"');
                }
            }
            scratch.html.push(b'>');
            while i < nlines {
                let l = line_slice(src, ranges, i);
                if is_fence_close(l) {
                    break;
                }
                if JSON_MODE {
                    escape_html_then_json_into(l, &mut scratch.html);
                    scratch.html.extend_from_slice(b"\\n");
                } else {
                    escape_html_into(l, &mut scratch.html);
                    scratch.html.push(b'\n');
                }
                i += 1;
            }
            i += 1;
            scratch
                .html
                .extend_from_slice(if JSON_MODE { b"</code></pre>\\n" } else { b"</code></pre>\n" });
            continue;
        }
        if let Some((depth, text)) = heading_of(line) {
            scratch.slug_buf.clear();
            slugify_into(text, &mut scratch.slug_buf);
            let base_len = scratch.slug_buf.len();
            let mut n = 2usize;
            while scratch
                .used_ids_ranges
                .iter()
                .any(|&(s, e)| &scratch.used_ids_pool[s as usize..e as usize] == scratch.slug_buf.as_slice())
            {
                scratch.slug_buf.truncate(base_len);
                scratch.slug_buf.push(b'-');
                push_usize(n, &mut scratch.slug_buf);
                n += 1;
            }
            let pool_start = scratch.used_ids_pool.len() as u32;
            scratch.used_ids_pool.extend_from_slice(&scratch.slug_buf);
            let pool_end = scratch.used_ids_pool.len() as u32;
            scratch.used_ids_ranges.push((pool_start, pool_end));

            scratch.html.extend_from_slice(b"<h");
            scratch.html.push(b'0' + depth);
            scratch
                .html
                .extend_from_slice(if JSON_MODE { b" id=\\\"" } else { b" id=\"" });
            scratch.html.extend_from_slice(&scratch.slug_buf);
            scratch
                .html
                .extend_from_slice(if JSON_MODE { b"\\\">" } else { b"\">" });

            let h_start = scratch.html.len();
            render_inline_into::<JSON_MODE, F>(
                text,
                link_fn,
                &mut scratch.href_tmp,
                &mut scratch.html,
            );
            let h_end = scratch.html.len();

            scratch.html.extend_from_slice(b"</h");
            scratch.html.push(b'0' + depth);
            scratch
                .html
                .extend_from_slice(if JSON_MODE { b">\\n" } else { b">\n" });

            if depth == 2 || depth == 3 {
                scratch.toc.push(TocItem {
                    level: depth,
                    id: scratch.slug_buf.clone(),
                    html: scratch.html[h_start..h_end].to_vec(),
                });
            }
            i += 1;
            continue;
        }
        if is_hr(line) {
            scratch
                .html
                .extend_from_slice(if JSON_MODE { b"<hr>\\n" } else { b"<hr>\n" });
            i += 1;
            continue;
        }
        if line.starts_with(b">") {
            scratch.join_buf.clear();
            let mut any = false;
            while i < nlines {
                let cur = line_slice(src, ranges, i);
                if !cur.starts_with(b">") {
                    break;
                }
                let mut l = &cur[1..];
                if l.starts_with(b" ") {
                    l = &l[1..];
                }
                let trimmed = trim_ascii_ht_sp(l);
                if !trimmed.is_empty() {
                    if any {
                        scratch.join_buf.push(b'\n');
                    }
                    scratch.join_buf.extend_from_slice(trimmed);
                    any = true;
                }
                i += 1;
            }
            if !any {
                scratch.html.extend_from_slice(if JSON_MODE {
                    b"<blockquote>\\n</blockquote>\\n"
                } else {
                    b"<blockquote>\n</blockquote>\n"
                });
            } else {
                scratch.html.extend_from_slice(if JSON_MODE {
                    b"<blockquote>\\n<p>"
                } else {
                    b"<blockquote>\n<p>"
                });
                render_inline_into::<JSON_MODE, F>(
                    &scratch.join_buf,
                    link_fn,
                    &mut scratch.href_tmp,
                    &mut scratch.html,
                );
                scratch.html.extend_from_slice(if JSON_MODE {
                    b"</p>\\n</blockquote>\\n"
                } else {
                    b"</p>\n</blockquote>\n"
                });
            }
            continue;
        }

        let is_bullet = bullet_rest(line).is_some();
        let ord0 = if !is_bullet { ordered_rest(line) } else { None };
        if is_bullet || ord0.is_some() {
            let is_ordered = ord0.is_some();
            if is_ordered {
                let start = ord0.unwrap().0;
                if start != 1 {
                    scratch
                        .html
                        .extend_from_slice(if JSON_MODE { b"<ol start=\\\"" } else { b"<ol start=\"" });
                    push_usize(start, &mut scratch.html);
                    scratch
                        .html
                        .extend_from_slice(if JSON_MODE { b"\\\">\\n" } else { b"\">\n" });
                } else {
                    scratch
                        .html
                        .extend_from_slice(if JSON_MODE { b"<ol>\\n" } else { b"<ol>\n" });
                }
            } else {
                scratch
                    .html
                    .extend_from_slice(if JSON_MODE { b"<ul>\\n" } else { b"<ul>\n" });
            }

            while i < nlines {
                let cur = line_slice(src, ranges, i);
                let matched = if is_ordered {
                    ordered_rest(cur).map(|x| x.1)
                } else {
                    bullet_rest(cur)
                };
                if let Some(rest) = matched {
                    let first_trimmed = trim_ascii_ht_sp(rest);
                    i += 1;
                    if i < nlines && is_continued(line_slice(src, ranges, i)) {
                        scratch.item_buf.clear();
                        scratch.item_buf.extend_from_slice(first_trimmed);
                        while i < nlines {
                            let c2 = line_slice(src, ranges, i);
                            if !is_continued(c2) {
                                break;
                            }
                            scratch.item_buf.push(b'\n');
                            scratch.item_buf.extend_from_slice(trim_ascii_ht_sp(c2));
                            i += 1;
                        }
                        scratch.html.extend_from_slice(b"<li>");
                        render_inline_into::<JSON_MODE, F>(
                            &scratch.item_buf,
                            link_fn,
                            &mut scratch.href_tmp,
                            &mut scratch.html,
                        );
                        scratch
                            .html
                            .extend_from_slice(if JSON_MODE { b"</li>\\n" } else { b"</li>\n" });
                    } else {
                        scratch.html.extend_from_slice(b"<li>");
                        render_inline_into::<JSON_MODE, F>(
                            first_trimmed,
                            link_fn,
                            &mut scratch.href_tmp,
                            &mut scratch.html,
                        );
                        scratch
                            .html
                            .extend_from_slice(if JSON_MODE { b"</li>\\n" } else { b"</li>\n" });
                    }
                } else {
                    break;
                }
            }
            scratch.html.extend_from_slice(match (is_ordered, JSON_MODE) {
                (true, true) => b"</ol>\\n",
                (true, false) => b"</ol>\n",
                (false, true) => b"</ul>\\n",
                (false, false) => b"</ul>\n",
            });
            continue;
        }

        // Paragraph
        let first_trimmed = trim_ascii_ht_sp(line);
        i += 1;
        if i < nlines && !ends_paragraph(line_slice(src, ranges, i)) {
            scratch.join_buf.clear();
            scratch.join_buf.extend_from_slice(first_trimmed);
            while i < nlines {
                let nl = line_slice(src, ranges, i);
                if ends_paragraph(nl) {
                    break;
                }
                scratch.join_buf.push(b'\n');
                scratch.join_buf.extend_from_slice(trim_ascii_ht_sp(nl));
                i += 1;
            }
            scratch.html.extend_from_slice(b"<p>");
            let ex_start = scratch.html.len();
            render_inline_into::<JSON_MODE, F>(
                &scratch.join_buf,
                link_fn,
                &mut scratch.href_tmp,
                &mut scratch.html,
            );
            let ex_end = scratch.html.len();
            if scratch.excerpt_range.is_none() {
                scratch.excerpt_range = Some((ex_start, ex_end));
            }
            scratch
                .html
                .extend_from_slice(if JSON_MODE { b"</p>\\n" } else { b"</p>\n" });
        } else {
            scratch.html.extend_from_slice(b"<p>");
            let ex_start = scratch.html.len();
            render_inline_into::<JSON_MODE, F>(
                first_trimmed,
                link_fn,
                &mut scratch.href_tmp,
                &mut scratch.html,
            );
            let ex_end = scratch.html.len();
            if scratch.excerpt_range.is_none() {
                scratch.excerpt_range = Some((ex_start, ex_end));
            }
            scratch
                .html
                .extend_from_slice(if JSON_MODE { b"</p>\\n" } else { b"</p>\n" });
        }
    }
}
