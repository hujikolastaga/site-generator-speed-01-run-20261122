use alloc::borrow::Cow;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use crate::simd::find_byte;
use crate::text::{is_blank_ht_sp, push_usize, trim_js};

pub struct FrontMatter<'a> {
    pub title: Cow<'a, [u8]>,
    pub date: Option<&'a [u8]>,
    pub tags: Vec<&'a [u8]>,
    pub draft: bool,
    pub layout: Option<&'a [u8]>,
    pub slug: Option<&'a [u8]>,
    pub summary: Option<Cow<'a, [u8]>>,
    pub body: &'a [u8],
}

pub enum FmResult<'a> {
    Ok(FrontMatter<'a>),
    Err(String),
}

pub fn normalize_newlines<'a>(raw: &'a [u8]) -> Cow<'a, [u8]> {
    let r = find_byte(raw, 0, b'\r');
    if r == raw.len() {
        return Cow::Borrowed(raw);
    }
    let mut out = Vec::with_capacity(raw.len());
    out.extend_from_slice(&raw[..r]);
    let mut i = r;
    while i < raw.len() {
        if raw[i] == b'\r' {
            out.push(b'\n');
            if i + 1 < raw.len() && raw[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            let next_r = find_byte(raw, i, b'\r');
            out.extend_from_slice(&raw[i..next_r]);
            i = next_r;
        }
    }
    Cow::Owned(out)
}

#[inline]
fn days_in(y: u32, m: u32) -> u32 {
    match m {
        2 => if (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0) { 29 } else { 28 },
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn parse_string_val<'a>(raw: &'a [u8]) -> Result<Cow<'a, [u8]>, &'static str> {
    if raw.starts_with(b"\"") {
        if raw.len() < 2 || !raw.ends_with(b"\"") {
            return Err("a quoted string is not closed");
        }
        let inner = &raw[1..raw.len() - 1];
        if !inner.contains(&b'\\') && !inner.contains(&b'"') {
            return Ok(Cow::Borrowed(inner));
        }
        let mut out = Vec::with_capacity(inner.len());
        let mut i = 0;
        while i < inner.len() {
            let c = inner[i];
            if c == b'\\' {
                if i + 1 >= inner.len() {
                    return Err("a quoted string holds an escape other than \\\" or \\\\");
                }
                let n = inner[i + 1];
                if n != b'"' && n != b'\\' {
                    return Err("a quoted string holds an escape other than \\\" or \\\\");
                }
                out.push(n);
                i += 2;
            } else if c == b'"' {
                return Err("a quoted string holds a stray quote");
            } else {
                out.push(c);
                i += 1;
            }
        }
        return Ok(Cow::Owned(out));
    }
    if raw.starts_with(b"[") {
        return Err("a bare string opens on [");
    }
    Ok(Cow::Borrowed(raw))
}

#[inline]
fn is_valid_date(raw: &[u8]) -> bool {
    if raw.len() != 10 || raw[4] != b'-' || raw[7] != b'-' {
        return false;
    }
    for (idx, &b) in raw.iter().enumerate() {
        if idx == 4 || idx == 7 {
            continue;
        }
        if !b.is_ascii_digit() {
            return false;
        }
    }
    let y = (raw[0] - b'0') as u32 * 1000
        + (raw[1] - b'0') as u32 * 100
        + (raw[2] - b'0') as u32 * 10
        + (raw[3] - b'0') as u32;
    let m = (raw[5] - b'0') as u32 * 10 + (raw[6] - b'0') as u32;
    let d = (raw[8] - b'0') as u32 * 10 + (raw[9] - b'0') as u32;
    (1970..=2099).contains(&y) && (1..=12).contains(&m) && (1..=days_in(y, m)).contains(&d)
}

#[inline]
fn is_valid_name(raw: &[u8]) -> bool {
    !raw.is_empty() && raw.iter().all(|&b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

#[inline]
fn is_valid_slug(raw: &[u8]) -> bool {
    if raw.is_empty() || raw.len() > 64 {
        return false;
    }
    let mut prev_hyphen = true;
    for &b in raw {
        if matches!(b, b'a'..=b'z' | b'0'..=b'9') {
            prev_hyphen = false;
        } else if b == b'-' {
            if prev_hyphen {
                return false;
            }
            prev_hyphen = true;
        } else {
            return false;
        }
    }
    !prev_hyphen
}

pub fn parse_front_matter<'a>(text: &'a [u8]) -> FmResult<'a> {
    let first_nl = find_byte(text, 0, b'\n');
    let first_line = &text[..first_nl];
    if first_line != b"---" {
        return FmResult::Err(String::from("no front matter"));
    }
    if first_nl == text.len() {
        return FmResult::Err(String::from("the front matter is not closed"));
    }
    let mut pos = first_nl + 1;

    // Locate closing '---' line first (note: parseFrontMatter finds `close` BEFORE validating lines).
    let mut close_start = None;
    let mut close_nl = 0usize;
    let mut scan = pos;
    loop {
        let nl = find_byte(text, scan, b'\n');
        if &text[scan..nl] == b"---" {
            close_start = Some(scan);
            close_nl = nl;
            break;
        }
        if nl == text.len() {
            break;
        }
        scan = nl + 1;
    }
    let close_start = match close_start {
        Some(c) => c,
        None => return FmResult::Err(String::from("the front matter is not closed")),
    };

    let mut title: Option<Cow<'a, [u8]>> = None;
    let mut date: Option<&'a [u8]> = None;
    let mut tags: Option<Vec<&'a [u8]>> = None;
    let mut draft: Option<bool> = None;
    let mut layout: Option<&'a [u8]> = None;
    let mut slug: Option<&'a [u8]> = None;
    let mut summary: Option<Cow<'a, [u8]>> = None;

    let mut line_num = 1usize; // opening '---' was line index 0; first inside line is i = 1 -> line 2
    while pos < close_start {
        let nl = find_byte(text, pos, b'\n');
        let line = &text[pos..nl];
        pos = nl + 1;
        line_num += 1;

        if is_blank_ht_sp(line) || line.starts_with(b"#") {
            continue;
        }

        // Match /^([a-z_]+):(?: (.*))?$/
        let mut key_end = 0;
        while key_end < line.len() && (matches!(line[key_end], b'a'..=b'z') || line[key_end] == b'_') {
            key_end += 1;
        }
        if key_end == 0 || key_end >= line.len() || line[key_end] != b':' {
            let mut msg = Vec::with_capacity(40);
            msg.extend_from_slice(b"line ");
            push_usize(line_num, &mut msg);
            msg.extend_from_slice(b" is not \"key: value\"");
            return FmResult::Err(unsafe { String::from_utf8_unchecked(msg) });
        }
        let key = &line[..key_end];
        let raw_slice = if key_end + 1 == line.len() {
            &b""[..]
        } else if line[key_end + 1] == b' ' {
            &line[key_end + 2..]
        } else {
            let mut msg = Vec::with_capacity(40);
            msg.extend_from_slice(b"line ");
            push_usize(line_num, &mut msg);
            msg.extend_from_slice(b" is not \"key: value\"");
            return FmResult::Err(unsafe { String::from_utf8_unchecked(msg) });
        };
        let mut raw_end = raw_slice.len();
        while raw_end > 0 && (raw_slice[raw_end - 1] == b' ' || raw_slice[raw_end - 1] == b'\t') {
            raw_end -= 1;
        }
        let raw = &raw_slice[..raw_end];

        enum KeyId { Title, Date, Tags, Draft, Layout, Slug, Summary }
        let kid = match key {
            b"title" => KeyId::Title,
            b"date" => KeyId::Date,
            b"tags" => KeyId::Tags,
            b"draft" => KeyId::Draft,
            b"layout" => KeyId::Layout,
            b"slug" => KeyId::Slug,
            b"summary" => KeyId::Summary,
            _ => {
                let kstr = core::str::from_utf8(key).unwrap_or("");
                return FmResult::Err(format!("unknown key \"{}\"", kstr));
            }
        };

        let seen = match kid {
            KeyId::Title => title.is_some(),
            KeyId::Date => date.is_some(),
            KeyId::Tags => tags.is_some(),
            KeyId::Draft => draft.is_some(),
            KeyId::Layout => layout.is_some(),
            KeyId::Slug => slug.is_some(),
            KeyId::Summary => summary.is_some(),
        };
        if seen {
            let kstr = core::str::from_utf8(key).unwrap_or("");
            return FmResult::Err(format!("key \"{}\" is given twice", kstr));
        }
        if raw.is_empty() {
            let kstr = core::str::from_utf8(key).unwrap_or("");
            return FmResult::Err(format!("key \"{}\" has no value", kstr));
        }

        match kid {
            KeyId::Title => match parse_string_val(raw) {
                Ok(v) => title = Some(v),
                Err(e) => return FmResult::Err(format!("key \"title\": {}", e)),
            },
            KeyId::Summary => match parse_string_val(raw) {
                Ok(v) => summary = Some(v),
                Err(e) => return FmResult::Err(format!("key \"summary\": {}", e)),
            },
            KeyId::Date => {
                if !is_valid_date(raw) {
                    return FmResult::Err(String::from("key \"date\": not a date from 1970-01-01 to 2099-12-31 (YYYY-MM-DD)"));
                }
                date = Some(raw);
            }
            KeyId::Tags => {
                if !raw.starts_with(b"[") || !raw.ends_with(b"]") {
                    return FmResult::Err(String::from("key \"tags\": not a list [a, b]"));
                }
                let inner = &raw[1..raw.len() - 1];
                if trim_js(inner).is_empty() {
                    tags = Some(Vec::new());
                } else {
                    let mut parsed = Vec::with_capacity(6);
                    for part in inner.split(|&c| c == b',') {
                        let t = trim_js(part);
                        if t.is_empty() || t.iter().any(|&b| matches!(b, b'"' | b'[' | b']')) {
                            return FmResult::Err(String::from("key \"tags\": an empty tag, or one holding \" [ or ]"));
                        }
                        parsed.push(t);
                    }
                    tags = Some(parsed);
                }
            }
            KeyId::Draft => {
                if raw == b"true" {
                    draft = Some(true);
                } else if raw == b"false" {
                    draft = Some(false);
                } else {
                    return FmResult::Err(String::from("key \"draft\": true or false"));
                }
            }
            KeyId::Layout => {
                if !is_valid_name(raw) {
                    return FmResult::Err(String::from("key \"layout\": not a template name"));
                }
                layout = Some(raw);
            }
            KeyId::Slug => {
                if !is_valid_slug(raw) {
                    return FmResult::Err(String::from("key \"slug\": not a slug"));
                }
                slug = Some(raw);
            }
        }
    }

    let title = match title {
        Some(t) => t,
        None => return FmResult::Err(String::from("no title")),
    };

    let body = if close_nl < text.len() {
        &text[close_nl + 1..]
    } else {
        &b""[..]
    };

    FmResult::Ok(FrontMatter {
        title,
        date,
        tags: tags.unwrap_or_default(),
        draft: draft.unwrap_or(false),
        layout,
        slug,
        summary,
        body,
    })
}
