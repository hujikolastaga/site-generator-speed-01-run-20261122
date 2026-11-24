use alloc::borrow::Cow;
use alloc::vec::Vec;
use crate::simd::find_json_special;

pub struct SiteConfig<'a> {
    pub title: Cow<'a, [u8]>,
    pub base_url: Cow<'a, [u8]>,
    pub per_page: usize,
    pub recent: usize,
}

pub struct InputTemplate<'a> {
    pub name: Cow<'a, [u8]>,
    pub source: Cow<'a, [u8]>,
}

pub struct InputFile<'a> {
    pub path: Cow<'a, [u8]>,
    pub content: Cow<'a, [u8]>,
}

pub struct Case<'a> {
    pub site: SiteConfig<'a>,
    pub templates: Vec<InputTemplate<'a>>,
    pub files: Vec<InputFile<'a>>,
}

struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    #[inline]
    fn skip_ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\n' | b'\r' | b'\t') {
            self.pos += 1;
        }
    }

    #[inline]
    fn expect(&mut self, ch: u8) -> Result<(), ()> {
        self.skip_ws();
        if self.pos < self.b.len() && self.b[self.pos] == ch {
            self.pos += 1;
            Ok(())
        } else {
            Err(())
        }
    }

    fn parse_str(&mut self) -> Result<Cow<'a, [u8]>, ()> {
        self.skip_ws();
        if self.pos >= self.b.len() || self.b[self.pos] != b'"' {
            return Err(());
        }
        self.pos += 1;
        let start = self.pos;
        let j = find_json_special(self.b, start);
        if j < self.b.len() && self.b[j] == b'"' {
            self.pos = j + 1;
            return Ok(Cow::Borrowed(&self.b[start..j]));
        }
        // Slow path with escapes
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&self.b[start..j]);
        self.pos = j;
        while self.pos < self.b.len() {
            let c = self.b[self.pos];
            if c == b'"' {
                self.pos += 1;
                return Ok(Cow::Owned(out));
            }
            if c == b'\\' {
                self.pos += 1;
                if self.pos >= self.b.len() {
                    return Err(());
                }
                let esc = self.b[self.pos];
                self.pos += 1;
                match esc {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0C),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
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
                        push_utf8(cp, &mut out);
                    }
                    _ => return Err(()),
                }
                let next_j = find_json_special(self.b, self.pos);
                if next_j > self.pos {
                    out.extend_from_slice(&self.b[self.pos..next_j]);
                    self.pos = next_j;
                }
            } else {
                return Err(());
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
            // Maybe float like 10.0?
            return Err(());
        }
        // Skip optional fraction/exponent just in case
        while self.pos < self.b.len() && matches!(self.b[self.pos], b'.' | b'e' | b'E' | b'+' | b'-' | b'0'..=b'9') {
            self.pos += 1;
        }
        Ok(val)
    }

    fn skip_value(&mut self) -> Result<(), ()> {
        self.skip_ws();
        if self.pos >= self.b.len() {
            return Err(());
        }
        match self.b[self.pos] {
            b'"' => {
                let _ = self.parse_str()?;
                Ok(())
            }
            b'{' => {
                self.pos += 1;
                self.skip_ws();
                if self.pos < self.b.len() && self.b[self.pos] == b'}' {
                    self.pos += 1;
                    return Ok(());
                }
                loop {
                    let _ = self.parse_str()?;
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
                while self.pos < self.b.len() && !matches!(self.b[self.pos], b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t') {
                    self.pos += 1;
                }
                Ok(())
            }
        }
    }

    fn parse_site(&mut self) -> Result<SiteConfig<'a>, ()> {
        self.expect(b'{')?;
        let mut title = Cow::Borrowed(&b""[..]);
        let mut base_url = Cow::Borrowed(&b""[..]);
        let mut per_page = 10;
        let mut recent = 5;
        self.skip_ws();
        if self.pos < self.b.len() && self.b[self.pos] == b'}' {
            self.pos += 1;
            return Ok(SiteConfig { title, base_url, per_page, recent });
        }
        loop {
            let k = self.parse_str()?;
            self.expect(b':')?;
            match k.as_ref() {
                b"title" => title = self.parse_str()?,
                b"base_url" => base_url = self.parse_str()?,
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

    fn parse_templates(&mut self) -> Result<Vec<InputTemplate<'a>>, ()> {
        self.expect(b'{')?;
        let mut out = Vec::with_capacity(32);
        self.skip_ws();
        if self.pos < self.b.len() && self.b[self.pos] == b'}' {
            self.pos += 1;
            return Ok(out);
        }
        loop {
            let name = self.parse_str()?;
            self.expect(b':')?;
            let source = self.parse_str()?;
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

    fn parse_files(&mut self) -> Result<Vec<InputFile<'a>>, ()> {
        self.expect(b'[')?;
        let mut out = Vec::with_capacity(620);
        self.skip_ws();
        if self.pos < self.b.len() && self.b[self.pos] == b']' {
            self.pos += 1;
            return Ok(out);
        }
        loop {
            self.expect(b'{')?;
            let mut path = Cow::Borrowed(&b""[..]);
            let mut content = Cow::Borrowed(&b""[..]);
            self.skip_ws();
            if !(self.pos < self.b.len() && self.b[self.pos] == b'}') {
                loop {
                    let k = self.parse_str()?;
                    self.expect(b':')?;
                    match k.as_ref() {
                        b"path" => path = self.parse_str()?,
                        b"content" => content = self.parse_str()?,
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
            out.push(InputFile { path, content });
            self.skip_ws();
            if self.pos < self.b.len() && self.b[self.pos] == b',' {
                self.pos += 1;
            } else {
                self.expect(b']')?;
                return Ok(out);
            }
        }
    }

    fn parse_input(&mut self) -> Result<Case<'a>, ()> {
        self.expect(b'{')?;
        let mut site = None;
        let mut templates = Vec::new();
        let mut files = Vec::new();
        self.skip_ws();
        if !(self.pos < self.b.len() && self.b[self.pos] == b'}') {
            loop {
                let k = self.parse_str()?;
                self.expect(b':')?;
                match k.as_ref() {
                    b"site" => site = Some(self.parse_site()?),
                    b"templates" => templates = self.parse_templates()?,
                    b"files" => files = self.parse_files()?,
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
        Ok(Case {
            site: site.ok_or(())?,
            templates,
            files,
        })
    }

    fn parse_case_entry(&mut self) -> Result<Case<'a>, ()> {
        self.expect(b'{')?;
        let mut input = None;
        self.skip_ws();
        if !(self.pos < self.b.len() && self.b[self.pos] == b'}') {
            loop {
                let k = self.parse_str()?;
                self.expect(b':')?;
                if k.as_ref() == b"input" {
                    input = Some(self.parse_input()?);
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
        input.ok_or(())
    }
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

pub fn parse_cases_json<'a>(bytes: &'a [u8]) -> Result<Vec<Case<'a>>, ()> {
    let mut p = Parser { b: bytes, pos: 0 };
    p.expect(b'[')?;
    let mut cases = Vec::with_capacity(4);
    p.skip_ws();
    if p.pos < p.b.len() && p.b[p.pos] == b']' {
        return Ok(cases);
    }
    loop {
        cases.push(p.parse_case_entry()?);
        p.skip_ws();
        if p.pos < p.b.len() && p.b[p.pos] == b',' {
            p.pos += 1;
        } else {
            p.expect(b']')?;
            return Ok(cases);
        }
    }
}
