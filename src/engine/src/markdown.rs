use alloc::vec::Vec;
use crate::simd::{find_byte, find_inline_special};
use crate::text::{decode_utf8, escape_html_into, has_js_ws, is_blank_ht_sp, is_js_ws_cp, push_usize, slugify_into, trim_ascii_ht_sp};

pub struct TocItem {
    pub level: u8,
    pub id: Vec<u8>,
    pub html: Vec<u8>,
}

pub struct MdOutput {
    pub html: Vec<u8>,
    pub toc: Vec<TocItem>,
    pub excerpt: Vec<u8>,
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

#[inline]
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

#[inline]
fn is_hr(line: &[u8]) -> bool {
    line.len() >= 3
        && (&line[..3] == b"---" || &line[..3] == b"***")
        && is_blank_ht_sp(&line[3..])
}

#[inline]
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
    is_blank_ht_sp(line) || fence_open_lang(line).is_some() || heading_of(line).is_some() || is_hr(line)
}

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

pub fn render_inline_into<F>(s: &[u8], link_fn: &mut F, out: &mut Vec<u8>)
where
    F: FnMut(&[u8], &mut Vec<u8>),
{
    let mut i = 0usize;
    let len = s.len();
    let mut href_tmp: Vec<u8> = Vec::new();
    while i < len {
        let j = find_inline_special(s, i);
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
                    escape_html_into(&s[i + 1..i + 2], out);
                    i += 2;
                } else {
                    out.push(b'\\');
                    i += 1;
                }
            }
            b'`' => {
                let close = find_byte(s, i + 1, b'`');
                if close < len && close > i + 1 {
                    out.extend_from_slice(b"<code>");
                    escape_html_into(&s[i + 1..close], out);
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
                            render_inline_into(&s[i + 2..close], link_fn, out);
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
                        render_inline_into(&s[i + 1..close], link_fn, out);
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
                                link_fn(target, &mut href_tmp);
                                out.extend_from_slice(b"<a href=\"");
                                escape_html_into(&href_tmp, out);
                                out.extend_from_slice(b"\">");
                                render_inline_into(&s[i + 1..k], link_fn, out);
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
            _ => unreachable!(),
        }
    }
}

pub fn render_markdown<F>(src: &[u8], link_fn: &mut F) -> MdOutput
where
    F: FnMut(&[u8], &mut Vec<u8>),
{
    // Collect line byte ranges directly from `src` (already normalized to `\n`).
    let mut lines: Vec<&[u8]> = Vec::with_capacity(64);
    let mut p = 0usize;
    loop {
        let nl = find_byte(src, p, b'\n');
        lines.push(&src[p..nl]);
        if nl == src.len() {
            break;
        }
        p = nl + 1;
    }

    let mut html = Vec::with_capacity(src.len() * 2 + 64);
    let mut toc = Vec::with_capacity(12);
    let mut excerpt: Vec<u8> = Vec::new();
    let mut has_excerpt = false;

    let mut used_ids: Vec<Vec<u8>> = Vec::with_capacity(16);
    let mut slug_buf: Vec<u8> = Vec::with_capacity(72);
    let mut join_buf: Vec<u8> = Vec::with_capacity(256);
    let mut inline_buf: Vec<u8> = Vec::with_capacity(256);

    let mut i = 0usize;
    let nlines = lines.len();
    while i < nlines {
        let line = lines[i];
        if is_blank_ht_sp(line) {
            i += 1;
            continue;
        }
        if let Some(lang) = fence_open_lang(line) {
            i += 1;
            html.extend_from_slice(b"<pre><code");
            if !lang.is_empty() {
                html.extend_from_slice(b" class=\"language-");
                escape_html_into(lang, &mut html);
                html.push(b'"');
            }
            html.push(b'>');
            while i < nlines && !is_fence_close(lines[i]) {
                escape_html_into(lines[i], &mut html);
                html.push(b'\n');
                i += 1;
            }
            i += 1;
            html.extend_from_slice(b"</code></pre>\n");
            continue;
        }
        if let Some((depth, text)) = heading_of(line) {
            slug_buf.clear();
            slugify_into(text, &mut slug_buf);
            let base_len = slug_buf.len();
            let mut n = 2usize;
            while used_ids.iter().any(|u| u.as_slice() == slug_buf.as_slice()) {
                slug_buf.truncate(base_len);
                slug_buf.push(b'-');
                push_usize(n, &mut slug_buf);
                n += 1;
            }
            let id_vec = slug_buf.clone();
            used_ids.push(id_vec.clone());

            inline_buf.clear();
            render_inline_into(text, link_fn, &mut inline_buf);

            html.extend_from_slice(b"<h");
            html.push(b'0' + depth);
            html.extend_from_slice(b" id=\"");
            html.extend_from_slice(&id_vec);
            html.extend_from_slice(b"\">");
            html.extend_from_slice(&inline_buf);
            html.extend_from_slice(b"</h");
            html.push(b'0' + depth);
            html.extend_from_slice(b">\n");

            if depth == 2 || depth == 3 {
                toc.push(TocItem {
                    level: depth,
                    id: id_vec,
                    html: inline_buf.clone(),
                });
            }
            i += 1;
            continue;
        }
        if is_hr(line) {
            html.extend_from_slice(b"<hr>\n");
            i += 1;
            continue;
        }
        if line.starts_with(b">") {
            join_buf.clear();
            let mut any = false;
            while i < nlines && lines[i].starts_with(b">") {
                let mut l = &lines[i][1..];
                if l.starts_with(b" ") {
                    l = &l[1..];
                }
                let trimmed = trim_ascii_ht_sp(l);
                if !trimmed.is_empty() {
                    if any {
                        join_buf.push(b'\n');
                    }
                    join_buf.extend_from_slice(trimmed);
                    any = true;
                }
                i += 1;
            }
            if !any {
                html.extend_from_slice(b"<blockquote>\n</blockquote>\n");
            } else {
                html.extend_from_slice(b"<blockquote>\n<p>");
                render_inline_into(&join_buf, link_fn, &mut html);
                html.extend_from_slice(b"</p>\n</blockquote>\n");
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
                    html.extend_from_slice(b"<ol start=\"");
                    push_usize(start, &mut html);
                    html.extend_from_slice(b"\">\n");
                } else {
                    html.extend_from_slice(b"<ol>\n");
                }
            } else {
                html.extend_from_slice(b"<ul>\n");
            }

            let mut item_buf: Vec<u8> = Vec::new();
            let mut has_item = false;
            while i < nlines {
                let cur = lines[i];
                let matched = if is_ordered {
                    ordered_rest(cur).map(|x| x.1)
                } else {
                    bullet_rest(cur)
                };
                if let Some(rest) = matched {
                    if has_item {
                        html.extend_from_slice(b"<li>");
                        render_inline_into(&item_buf, link_fn, &mut html);
                        html.extend_from_slice(b"</li>\n");
                    }
                    item_buf.clear();
                    item_buf.extend_from_slice(trim_ascii_ht_sp(rest));
                    has_item = true;
                    i += 1;
                } else if is_continued(cur) {
                    item_buf.push(b'\n');
                    item_buf.extend_from_slice(trim_ascii_ht_sp(cur));
                    i += 1;
                } else {
                    break;
                }
            }
            if has_item {
                html.extend_from_slice(b"<li>");
                render_inline_into(&item_buf, link_fn, &mut html);
                html.extend_from_slice(b"</li>\n");
            }
            html.extend_from_slice(if is_ordered { b"</ol>\n" } else { b"</ul>\n" });
            continue;
        }

        // Paragraph
        join_buf.clear();
        join_buf.extend_from_slice(trim_ascii_ht_sp(line));
        i += 1;
        while i < nlines && !ends_paragraph(lines[i]) {
            join_buf.push(b'\n');
            join_buf.extend_from_slice(trim_ascii_ht_sp(lines[i]));
            i += 1;
        }
        inline_buf.clear();
        render_inline_into(&join_buf, link_fn, &mut inline_buf);
        if !has_excerpt {
            excerpt = inline_buf.clone();
            has_excerpt = true;
        }
        html.extend_from_slice(b"<p>");
        html.extend_from_slice(&inline_buf);
        html.extend_from_slice(b"</p>\n");
    }

    MdOutput { html, toc, excerpt }
}
