use alloc::vec::Vec;
use crate::simd::find_json_special;

#[derive(Copy, Clone)]
pub enum JsonStr<'a> {
    Borrowed(&'a [u8]),
    Pooled(u32, u32),
}

impl<'a> JsonStr<'a> {
    #[inline(always)]
    pub fn as_bytes<'b>(&self, pool: &'b [u8]) -> &'b [u8]
    where
        'a: 'b,
    {
        match *self {
            JsonStr::Borrowed(s) => s,
            JsonStr::Pooled(off, len) => unsafe {
                pool.get_unchecked(off as usize..(off as usize + len as usize))
            },
        }
    }
}

pub struct SiteConfig<'a> {
    pub title: JsonStr<'a>,
    pub base_url: JsonStr<'a>,
    pub per_page: usize,
    pub recent: usize,
}

pub struct InputTemplate<'a> {
    pub name: JsonStr<'a>,
    pub source: JsonStr<'a>,
}

pub struct InputFile<'a> {
    pub path: JsonStr<'a>,
    pub content: JsonStr<'a>,
    pub has_cr: bool,
}

pub struct Case<'a> {
    pub site: SiteConfig<'a>,
    pub templates: Vec<InputTemplate<'a>>,
    pub files: Vec<InputFile<'a>>,
    pub str_pool: Vec<u8>,
}

pub struct CasesParser<'a> {
    b: &'a [u8],
    pos: usize,
    done: bool,
}

#[inline]
fn push_utf8(cp: u32, out: &mut Vec<u8>) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.push(0xC0 | (cp >> 6) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else if cp < 0x10000 {
        out.push(0xE0 | (cp >> 12) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else {
        out.push(0xF0 | (cp >> 18) as u8);
        out.push(0x80 | ((cp >> 12) & 0x3F) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    }
}

impl<'a> CasesParser<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, ()> {
        let mut p = Self {
            b: bytes,
            pos: 0,
            done: false,
        };
        p.expect(b'[')?;
        p.skip_ws();
        if p.pos < p.b.len() && p.b[p.pos] == b']' {
            p.pos += 1;
            p.done = true;
        }
        Ok(p)
    }

    #[inline(always)]
    fn skip_ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\n' | b'\r' | b'\t') {
            self.pos += 1;
        }
    }

    #[inline(always)]
    fn expect(&mut self, ch: u8) -> Result<(), ()> {
        self.skip_ws();
        if self.pos < self.b.len() && self.b[self.pos] == ch {
            self.pos += 1;
            Ok(())
        } else {
            Err(())
        }
    }

    #[inline]
    fn parse_str(&mut self, pool: &mut Vec<u8>) -> Result<(JsonStr<'a>, bool), ()> {
        self.skip_ws();
        if self.pos >= self.b.len() || self.b[self.pos] != b'"' {
            return Err(());
        }
        self.pos += 1;
        let start = self.pos;
        let j = find_json_special(self.b, start);
        if j < self.b.len() && self.b[j] == b'"' {
            self.pos = j + 1;
            return Ok((JsonStr::Borrowed(&self.b[start..j]), false));
        }

        // Slow path: decode escaped string directly into `pool`.
        let pool_off = pool.len();
        if j > start {
            pool.extend_from_slice(&self.b[start..j]);
        }
        self.pos = j;
        let mut has_cr = false;

        while self.pos < self.b.len() {
            let c = self.b[self.pos];
            if c == b'"' {
                self.pos += 1;
                let pool_len = pool.len() - pool_off;
                return Ok((JsonStr::Pooled(pool_off as u32, pool_len as u32), has_cr));
            }
            if c != b'\\' {
                return Err(());
            }
            self.pos += 1;
            if self.pos >= self.b.len() {
                return Err(());
            }
            let esc = self.b[self.pos];
            self.pos += 1;
            match esc {
                b'n' => pool.push(b'\n'),
                b'"' => pool.push(b'"'),
                b'\\' => pool.push(b'\\'),
                b't' => pool.push(b'\t'),
                b'r' => {
                    has_cr = true;
                    pool.push(b'\r');
                }
                b'/' => pool.push(b'/'),
                b'b' => pool.push(0x08),
                b'f' => pool.push(0x0C),
                b'u' => {
                    let u1 = self.parse_hex4()?;
                    let cp = if (0xD800..=0xDBFF).contains(&u1)
                        && self.pos + 6 <= self.b.len()
                        && self.b[self.pos] == b'\\'
                        && self.b[self.pos + 1] == b'u'
                    {
                        self.pos += 2;
                        let u2 = self.parse_hex4()?;
                        if (0xDC00..=0xDFFF).contains(&u2) {
                            0x10000 + (((u1 as u32 - 0xD800) << 10) | (u2 as u32 - 0xDC00))
                        } else {
                            0xFFFD
                        }
                    } else if (0xD800..=0xDFFF).contains(&u1) {
                        0xFFFD
                    } else {
                        u1 as u32
                    };
                    if cp == 0x0D {
                        has_cr = true;
                    }
                    push_utf8(cp, pool);
                }
                _ => return Err(()),
            }

            // Fused SSE2 load + scan + overlapping store into `pool`.
            #[cfg(target_arch = "x86_64")]
            unsafe {
                use core::arch::x86_64::*;
                let ptr = self.b.as_ptr();
                let vq = _mm_set1_epi8(b'"' as i8);
                let vs = _mm_set1_epi8(b'\\' as i8);
                let x80 = _mm_set1_epi8(-128i8);
                let lim = _mm_set1_epi8((0x20u8 ^ 0x80u8) as i8);
                while self.pos + 16 <= self.b.len() {
                    let v = _mm_loadu_si128(ptr.add(self.pos) as *const __m128i);
                    let eq = _mm_or_si128(_mm_cmpeq_epi8(v, vq), _mm_cmpeq_epi8(v, vs));
                    let lt = _mm_cmpgt_epi8(lim, _mm_xor_si128(v, x80));
                    let m = _mm_movemask_epi8(_mm_or_si128(eq, lt)) as u32;
                    let cur_len = pool.len();
                    if pool.capacity() - cur_len < 32 {
                        pool.reserve(4096);
                    }
                    _mm_storeu_si128(pool.as_mut_ptr().add(cur_len) as *mut __m128i, v);
                    if m == 0 {
                        pool.set_len(cur_len + 16);
                        self.pos += 16;
                    } else {
                        let tz = m.trailing_zeros() as usize;
                        pool.set_len(cur_len + tz);
                        self.pos += tz;
                        break;
                    }
                }
            }
            while self.pos < self.b.len() {
                let bch = self.b[self.pos];
                if bch == b'"' || bch == b'\\' || bch < 0x20 {
                    break;
                }
                pool.push(bch);
                self.pos += 1;
            }
        }
        Err(())
    }

    fn parse_hex4(&mut self) -> Result<u16, ()> {
        if self.pos + 4 > self.b.len() {
            return Err(());
        }
        let mut v = 0u16;
        for k in 0..4 {
            let d = match self.b[self.pos + k] {
                b'0'..=b'9' => self.b[self.pos + k] - b'0',
                b'a'..=b'f' => self.b[self.pos + k] - b'a' + 10,
                b'A'..=b'F' => self.b[self.pos + k] - b'A' + 10,
                _ => return Err(()),
            };
            v = (v << 4) | (d as u16);
        }
        self.pos += 4;
        Ok(v)
    }

    fn parse_usize(&mut self) -> Result<usize, ()> {
        self.skip_ws();
        let start = self.pos;
        let mut val = 0usize;
        while self.pos < self.b.len() && self.b[self.pos].is_ascii_digit() {
            val = val.saturating_mul(10).saturating_add((self.b[self.pos] - b'0') as usize);
            self.pos += 1;
        }
        if self.pos == start {
            return Err(());
        }
        while self.pos < self.b.len()
            && matches!(self.b[self.pos], b'.' | b'e' | b'E' | b'+' | b'-' | b'0'..=b'9')
        {
            self.pos += 1;
        }
        Ok(val)
    }

    fn skip_str(&mut self) -> Result<(), ()> {
        self.skip_ws();
        if self.pos >= self.b.len() || self.b[self.pos] != b'"' {
            return Err(());
        }
        self.pos += 1;
        while self.pos < self.b.len() {
            let j = find_json_special(self.b, self.pos);
            if j >= self.b.len() {
                return Err(());
            }
            match self.b[j] {
                b'"' => {
                    self.pos = j + 1;
                    return Ok(());
                }
                b'\\' => {
                    self.pos = j + 2;
                    if self.pos > self.b.len() {
                        return Err(());
                    }
                }
                _ => return Err(()),
            }
        }
        Err(())
    }

    fn skip_value(&mut self) -> Result<(), ()> {
        self.skip_ws();
        if self.pos >= self.b.len() {
            return Err(());
        }
        match self.b[self.pos] {
            b'"' => self.skip_str(),
            b'{' => {
                self.pos += 1;
                self.skip_ws();
                if self.pos < self.b.len() && self.b[self.pos] == b'}' {
                    self.pos += 1;
                    return Ok(());
                }
                loop {
                    self.skip_str()?;
                    self.expect(b':')?;
                    self.skip_value()?;
                    self.skip_ws();
                    if self.pos < self.b.len() && self.b[self.pos] == b',' {
                        self.pos += 1;
                    } else {
                        self.expect(b'}')?;
                        return Ok(());
                    }
                }
            }
            b'[' => {
                self.pos += 1;
                self.skip_ws();
                if self.pos < self.b.len() && self.b[self.pos] == b']' {
                    self.pos += 1;
                    return Ok(());
                }
                loop {
                    self.skip_value()?;
                    self.skip_ws();
                    if self.pos < self.b.len() && self.b[self.pos] == b',' {
                        self.pos += 1;
                    } else {
                        self.expect(b']')?;
                        return Ok(());
                    }
                }
            }
            _ => {
                while self.pos < self.b.len()
                    && !matches!(self.b[self.pos], b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t')
                {
                    self.pos += 1;
                }
                Ok(())
            }
        }
    }

    fn parse_site(&mut self, pool: &mut Vec<u8>) -> Result<SiteConfig<'a>, ()> {
        self.expect(b'{')?;
        let mut title = JsonStr::Borrowed(&b""[..]);
        let mut base_url = JsonStr::Borrowed(&b""[..]);
        let mut per_page = 10;
        let mut recent = 5;
        self.skip_ws();
        if self.pos < self.b.len() && self.b[self.pos] == b'}' {
            self.pos += 1;
            return Ok(SiteConfig { title, base_url, per_page, recent });
        }
        loop {
            let (k, _) = self.parse_str(pool)?;
            self.expect(b':')?;
            match k.as_bytes(pool) {
                b"title" => title = self.parse_str(pool)?.0,
                b"base_url" => base_url = self.parse_str(pool)?.0,
                b"per_page" => per_page = self.parse_usize()?,
                b"recent" => recent = self.parse_usize()?,
                _ => self.skip_value()?,
            }
            self.skip_ws();
            if self.pos < self.b.len() && self.b[self.pos] == b',' {
                self.pos += 1;
            } else {
                self.expect(b'}')?;
                return Ok(SiteConfig { title, base_url, per_page, recent });
            }
        }
    }

    fn parse_templates(&mut self, pool: &mut Vec<u8>) -> Result<Vec<InputTemplate<'a>>, ()> {
        self.expect(b'{')?;
        let mut out = Vec::with_capacity(32);
        self.skip_ws();
        if self.pos < self.b.len() && self.b[self.pos] == b'}' {
            self.pos += 1;
            return Ok(out);
        }
        loop {
            let (name, _) = self.parse_str(pool)?;
            self.expect(b':')?;
            let (source, _) = self.parse_str(pool)?;
            out.push(InputTemplate { name, source });
            self.skip_ws();
            if self.pos < self.b.len() && self.b[self.pos] == b',' {
                self.pos += 1;
            } else {
                self.expect(b'}')?;
                return Ok(out);
            }
        }
    }

    fn parse_files(&mut self, pool: &mut Vec<u8>) -> Result<Vec<InputFile<'a>>, ()> {
        self.expect(b'[')?;
        let mut out = Vec::with_capacity(620);
        self.skip_ws();
        if self.pos < self.b.len() && self.b[self.pos] == b']' {
            self.pos += 1;
            return Ok(out);
        }
        loop {
            self.expect(b'{')?;
            let mut path = JsonStr::Borrowed(&b""[..]);
            let mut content = JsonStr::Borrowed(&b""[..]);
            let mut has_cr = false;
            self.skip_ws();
            if !(self.pos < self.b.len() && self.b[self.pos] == b'}') {
                loop {
                    let (k, _) = self.parse_str(pool)?;
                    self.expect(b':')?;
                    match k.as_bytes(pool) {
                        b"path" => path = self.parse_str(pool)?.0,
                        b"content" => {
                            let (c, cr) = self.parse_str(pool)?;
                            content = c;
                            has_cr = cr;
                        }
                        _ => self.skip_value()?,
                    }
                    self.skip_ws();
                    if self.pos < self.b.len() && self.b[self.pos] == b',' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            }
            self.expect(b'}')?;
            out.push(InputFile { path, content, has_cr });
            self.skip_ws();
            if self.pos < self.b.len() && self.b[self.pos] == b',' {
                self.pos += 1;
            } else {
                self.expect(b']')?;
                return Ok(out);
            }
        }
    }

    fn parse_input(&mut self, pool: &mut Vec<u8>) -> Result<(SiteConfig<'a>, Vec<InputTemplate<'a>>, Vec<InputFile<'a>>), ()> {
        self.expect(b'{')?;
        let mut site = None;
        let mut templates = Vec::new();
        let mut files = Vec::new();
        self.skip_ws();
        if !(self.pos < self.b.len() && self.b[self.pos] == b'}') {
            loop {
                let (k, _) = self.parse_str(pool)?;
                self.expect(b':')?;
                match k.as_bytes(pool) {
                    b"site" => site = Some(self.parse_site(pool)?),
                    b"templates" => templates = self.parse_templates(pool)?,
                    b"files" => files = self.parse_files(pool)?,
                    _ => self.skip_value()?,
                }
                self.skip_ws();
                if self.pos < self.b.len() && self.b[self.pos] == b',' {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        self.expect(b'}')?;
        Ok((site.ok_or(())?, templates, files))
    }

    pub fn next_case(&mut self) -> Result<Option<Case<'a>>, ()> {
        if self.done {
            return Ok(None);
        }
        let rem = self.b.len() - self.pos;
        let mut str_pool = Vec::with_capacity(rem.saturating_add(128));
        self.expect(b'{')?;
        let mut input = None;
        self.skip_ws();
        if !(self.pos < self.b.len() && self.b[self.pos] == b'}') {
            loop {
                let (k, _) = self.parse_str(&mut str_pool)?;
                self.expect(b':')?;
                if k.as_bytes(&str_pool) == b"input" {
                    input = Some(self.parse_input(&mut str_pool)?);
                } else {
                    self.skip_value()?;
                }
                self.skip_ws();
                if self.pos < self.b.len() && self.b[self.pos] == b',' {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        self.expect(b'}')?;
        self.skip_ws();
        if self.pos < self.b.len() && self.b[self.pos] == b',' {
            self.pos += 1;
        } else {
            self.expect(b']')?;
            self.done = true;
        }
        let (site, templates, files) = input.ok_or(())?;
        Ok(Some(Case {
            site,
            templates,
            files,
            str_pool,
        }))
    }
}

pub fn parse_cases_json<'a>(bytes: &'a [u8]) -> Result<Vec<Case<'a>>, ()> {
    let mut parser = CasesParser::new(bytes)?;
    let mut cases = Vec::with_capacity(4);
    while let Some(c) = parser.next_case()? {
        cases.push(c);
    }
    Ok(cases)
}
