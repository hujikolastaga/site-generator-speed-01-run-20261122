use alloc::borrow::Cow;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use crate::frontmatter::{normalize_newlines, parse_front_matter, FmResult};
use crate::json::Case;
use crate::markdown::{render_markdown, TocItem};
use crate::simd::find_byte;
use crate::template::{fold_site_ops, inline_template, parse_template, render_ops, CardCache, Node, Op, Scope, Val, ValueSource};
use crate::text::{cmp_utf16, escape_json_into, long_date_into, push_usize, slugify_vec, words_of};

const RESERVED: [&[u8]; 3] = [b"posts", b"tags", b"page"];

// Open-addressed FNV-1a 64-bit hash map/set for no_std speed without external dependencies.
#[inline]
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

struct StrMap<'a> {
    slots: Vec<Option<(u64, &'a [u8], usize)>>,
    mask: usize,
}

impl<'a> StrMap<'a> {
    fn with_capacity(n: usize) -> Self {
        let cap = (n.max(8) * 2).next_power_of_two();
        Self {
            slots: vec![None; cap],
            mask: cap - 1,
        }
    }

    fn insert(&mut self, key: &'a [u8], val: usize) {
        let h = fnv1a(key);
        let mut idx = (h as usize) & self.mask;
        loop {
            match self.slots[idx] {
                None => {
                    self.slots[idx] = Some((h, key, val));
                    return;
                }
                Some((eh, ek, _)) if eh == h && ek == key => {
                    self.slots[idx] = Some((h, key, val));
                    return;
                }
                _ => idx = (idx + 1) & self.mask,
            }
        }
    }

    #[inline]
    fn get(&self, key: &[u8]) -> Option<usize> {
        let h = fnv1a(key);
        let mut idx = (h as usize) & self.mask;
        loop {
            match self.slots[idx] {
                None => return None,
                Some((eh, ek, v)) if eh == h && ek == key => return Some(v),
                _ => idx = (idx + 1) & self.mask,
            }
        }
    }
}

struct OwnedStrSet {
    items: Vec<Vec<u8>>,
    slots: Vec<Option<(u64, usize)>>,
    mask: usize,
}

impl OwnedStrSet {
    fn with_capacity(n: usize) -> Self {
        let cap = (n.max(8) * 2).next_power_of_two();
        Self {
            items: Vec::with_capacity(n),
            slots: vec![None; cap],
            mask: cap - 1,
        }
    }

    fn maybe_grow(&mut self) {
        if (self.items.len() + 1) * 2 > self.slots.len() {
            let new_cap = self.slots.len() * 2;
            let new_mask = new_cap - 1;
            let mut new_slots = vec![None; new_cap];
            for (idx_item, item) in self.items.iter().enumerate() {
                let h = fnv1a(item);
                let mut i = (h as usize) & new_mask;
                while new_slots[i].is_some() {
                    i = (i + 1) & new_mask;
                }
                new_slots[i] = Some((h, idx_item));
            }
            self.slots = new_slots;
            self.mask = new_mask;
        }
    }

    fn insert(&mut self, val: Vec<u8>) -> bool {
        self.maybe_grow();
        let h = fnv1a(&val);
        let mut idx = (h as usize) & self.mask;
        loop {
            match self.slots[idx] {
                None => {
                    let id = self.items.len();
                    self.items.push(val);
                    self.slots[idx] = Some((h, id));
                    return true;
                }
                Some((eh, id)) if eh == h && self.items[id] == val => return false,
                _ => idx = (idx + 1) & self.mask,
            }
        }
    }

    fn contains(&self, val: &[u8]) -> bool {
        let h = fnv1a(val);
        let mut idx = (h as usize) & self.mask;
        loop {
            match self.slots[idx] {
                None => return false,
                Some((eh, id)) if eh == h && self.items[id] == val => return true,
                _ => idx = (idx + 1) & self.mask,
            }
        }
    }
}

#[inline]
fn dir_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&c| c == b'/') {
        Some(pos) => &path[..pos],
        None => b"",
    }
}

fn resolve_rel_path(dir: &[u8], rel: &[u8], out: &mut Vec<u8>) -> bool {
    out.clear();
    let mut seg_ends: Vec<usize> = Vec::with_capacity(8);
    if !dir.is_empty() {
        for part in dir.split(|&c| c == b'/') {
            if !out.is_empty() {
                out.push(b'/');
            }
            out.extend_from_slice(part);
            seg_ends.push(out.len());
        }
    }
    for seg in rel.split(|&c| c == b'/') {
        if seg.is_empty() || seg == b"." {
            continue;
        }
        if seg == b".." {
            if seg_ends.is_empty() {
                return false;
            }
            seg_ends.pop();
            let new_len = *seg_ends.last().unwrap_or(&0);
            out.truncate(new_len);
        } else {
            if !seg_ends.is_empty() {
                out.push(b'/');
            }
            out.extend_from_slice(seg);
            seg_ends.push(out.len());
        }
    }
    true
}

#[inline]
fn has_uri_scheme(target: &[u8]) -> bool {
    if target.is_empty() || !target[0].is_ascii_alphabetic() {
        return false;
    }
    let mut i = 1usize;
    while i < target.len()
        && (target[i].is_ascii_alphanumeric() || matches!(target[i], b'+' | b'.' | b'-'))
    {
        i += 1;
    }
    i < target.len() && target[i] == b':'
}

// ---------------------------------------------------------------- Template Value Adapters

struct SiteVal<'a> {
    title: &'a [u8],
    base_url: &'a [u8],
}

impl<'a> ValueSource for SiteVal<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"title" => Some(Val::Str(self.title)),
            b"base_url" => Some(Val::Str(self.base_url)),
            _ => None,
        }
    }
}

struct TagSummaryVal<'a> {
    name: &'a [u8],
    slug: &'a [u8],
    url: &'a [u8],
    count: usize,
}

impl<'a> ValueSource for TagSummaryVal<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"name" => Some(Val::Str(self.name)),
            b"slug" => Some(Val::Str(self.slug)),
            b"url" => Some(Val::Str(self.url)),
            b"count" => Some(Val::Num(self.count)),
            _ => None,
        }
    }
}

struct CardTagVal<'a> {
    name: &'a [u8],
    slug: &'a [u8],
    url: &'a [u8],
}

impl<'a> ValueSource for CardTagVal<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"name" => Some(Val::Str(self.name)),
            b"slug" => Some(Val::Str(self.slug)),
            b"url" => Some(Val::Str(self.url)),
            _ => None,
        }
    }
}

struct CardVal<'a> {
    id: usize,
    title: &'a [u8],
    url: &'a [u8],
    date: &'a [u8],
    date_long: &'a [u8],
    summary: &'a [u8],
    excerpt: &'a [u8],
    reading_minutes: usize,
    tag_refs: &'a [&'a dyn ValueSource],
}

impl<'a> ValueSource for CardVal<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"title" => Some(Val::Str(self.title)),
            b"url" => Some(Val::Str(self.url)),
            b"date" => Some(Val::Str(self.date)),
            b"date_long" => Some(Val::Str(self.date_long)),
            b"summary" => Some(Val::Str(self.summary)),
            b"excerpt" => Some(Val::Str(self.excerpt)),
            b"reading_minutes" => Some(Val::Num(self.reading_minutes)),
            b"tags" => Some(Val::Arr(self.tag_refs)),
            _ => None,
        }
    }
    #[inline]
    fn post_id(&self) -> Option<usize> {
        Some(self.id)
    }
}

struct TocVal<'a> {
    level: usize,
    id: &'a [u8],
    html: &'a [u8],
}

impl<'a> ValueSource for TocVal<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"level" => Some(Val::Num(self.level)),
            b"id" => Some(Val::Str(self.id)),
            b"html" => Some(Val::Str(self.html)),
            _ => None,
        }
    }
}

struct LinkVal<'a> {
    title: &'a [u8],
    url: &'a [u8],
    date: &'a [u8],
    date_long: &'a [u8],
}

impl<'a> ValueSource for LinkVal<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"title" => Some(Val::Str(self.title)),
            b"url" => Some(Val::Str(self.url)),
            b"date" => Some(Val::Str(self.date)),
            b"date_long" => Some(Val::Str(self.date_long)),
            _ => None,
        }
    }
}

struct PageVal<'a> {
    card: &'a CardVal<'a>,
    slug: &'a [u8],
    content: &'a [u8],
    toc: &'a [&'a dyn ValueSource],
    newer: Option<&'a LinkVal<'a>>,
    older: Option<&'a LinkVal<'a>>,
    is_post: bool,
}

impl<'a> ValueSource for PageVal<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"slug" => Some(Val::Str(self.slug)),
            b"content" => Some(Val::Str(self.content)),
            b"toc" => Some(Val::Arr(self.toc)),
            b"newer" => Some(match self.newer {
                Some(l) => Val::Obj(l),
                None => Val::Null,
            }),
            b"older" => Some(match self.older {
                Some(l) => Val::Obj(l),
                None => Val::Null,
            }),
            b"is_post" => Some(Val::Bool(self.is_post)),
            _ => self.card.prop(key),
        }
    }
}

struct PagerVal<'a> {
    number: usize,
    total: usize,
    url: &'a [u8],
    newer_url: &'a [u8],
    older_url: &'a [u8],
}

impl<'a> ValueSource for PagerVal<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"number" => Some(Val::Num(self.number)),
            b"total" => Some(Val::Num(self.total)),
            b"url" => Some(Val::Str(self.url)),
            b"newer_url" => Some(Val::Str(self.newer_url)),
            b"older_url" => Some(Val::Str(self.older_url)),
            _ => None,
        }
    }
}

struct TagFullVal<'a> {
    name: &'a [u8],
    slug: &'a [u8],
    url: &'a [u8],
    count: usize,
    posts: &'a [&'a dyn ValueSource],
}

impl<'a> ValueSource for TagFullVal<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"name" => Some(Val::Str(self.name)),
            b"slug" => Some(Val::Str(self.slug)),
            b"url" => Some(Val::Str(self.url)),
            b"count" => Some(Val::Num(self.count)),
            b"posts" => Some(Val::Arr(self.posts)),
            _ => None,
        }
    }
}

struct GlobalsScope<'a> {
    site: &'a SiteVal<'a>,
    tags: &'a [&'a dyn ValueSource],
    recent: &'a [&'a dyn ValueSource],
}

impl<'a> ValueSource for GlobalsScope<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"site" => Some(Val::Obj(self.site)),
            b"tags" => Some(Val::Arr(self.tags)),
            b"recent" => Some(Val::Arr(self.recent)),
            _ => None,
        }
    }
}

struct PageScope<'a> {
    page: &'a PageVal<'a>,
}
impl<'a> ValueSource for PageScope<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        if key == b"page" { Some(Val::Obj(self.page)) } else { None }
    }
}

struct IndexScope<'a> {
    posts: &'a [&'a dyn ValueSource],
    pager: &'a PagerVal<'a>,
}
impl<'a> ValueSource for IndexScope<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"posts" => Some(Val::Arr(self.posts)),
            b"pager" => Some(Val::Obj(self.pager)),
            _ => None,
        }
    }
}

struct TagScope<'a> {
    tag: &'a TagFullVal<'a>,
}
impl<'a> ValueSource for TagScope<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        if key == b"tag" { Some(Val::Obj(self.tag)) } else { None }
    }
}

// ---------------------------------------------------------------- JSON Output Emitter

#[inline]
fn emit_kv_pre_json(out: &mut Vec<u8>, first: &mut bool, key: &[u8], pre_json_val: &[u8]) {
    if !*first {
        out.push(b',');
    }
    *first = false;
    out.push(b'"');
    escape_json_into(key, out);
    out.extend_from_slice(b"\":\"");
    out.extend_from_slice(pre_json_val);
    out.push(b'"');
}

#[inline]
fn emit_kv_raw(out: &mut Vec<u8>, first: &mut bool, key: &[u8], raw_val: &[u8]) {
    if !*first {
        out.push(b',');
    }
    *first = false;
    out.push(b'"');
    escape_json_into(key, out);
    out.extend_from_slice(b"\":\"");
    escape_json_into(raw_val, out);
    out.push(b'"');
}

// ---------------------------------------------------------------- Site Pipeline

pub fn build_site_into_json<'a>(case: &'a Case<'a>, out_json: &mut Vec<u8>) {
    // 1. Parse templates sorted by name (UTF-16 code units).
    let mut tpl_order: Vec<usize> = (0..case.templates.len()).collect();
    tpl_order.sort_by(|&a, &b| cmp_utf16(&case.templates[a].name, &case.templates[b].name));

    let mut parsed_tpls: Vec<(&[u8], Vec<Node>)> = Vec::with_capacity(tpl_order.len());
    let mut report_lines: Vec<String> = Vec::new();
    for idx in tpl_order {
        let t = &case.templates[idx];
        match parse_template(&t.name, &t.source) {
            Ok(nodes) => parsed_tpls.push((&t.name, nodes)),
            Err(msg) => report_lines.push(msg),
        }
    }
    let get_tpl = |name: &[u8]| -> Option<&[Node]> {
        parsed_tpls.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_slice())
    };

    let mut base: &[u8] = &case.site.base_url;
    while base.ends_with(b"/") {
        base = &base[..base.len() - 1];
    }

    // 2. Partition & sort input files by path (UTF-16 code units).
    let mut file_order: Vec<usize> = (0..case.files.len()).collect();
    file_order.sort_by(|&a, &b| cmp_utf16(&case.files[a].path, &case.files[b].path));

    struct StaticItem<'a> {
        path: &'a [u8],
        content: &'a [u8],
    }

    let mut statics: Vec<StaticItem<'a>> = Vec::with_capacity(16);
    let mut norm_texts: Vec<Cow<'a, [u8]>> = Vec::with_capacity(file_order.len());
    let mut md_raw_indices: Vec<usize> = Vec::with_capacity(file_order.len());

    for idx in file_order {
        let f = &case.files[idx];
        if f.path.ends_with(b".md") {
            norm_texts.push(normalize_newlines(&f.content));
            md_raw_indices.push(idx);
        } else {
            statics.push(StaticItem {
                path: &f.path,
                content: &f.content,
            });
        }
    }

    struct SourceItem<'a, 'b> {
        title: Cow<'a, [u8]>,
        date: Option<&'b [u8]>,
        date_long: Vec<u8>,
        raw_tags: Vec<&'b [u8]>,
        layout_idx: usize,
        slug: Vec<u8>,
        out_dir: Vec<u8>,
        url: Vec<u8>,
        summary: Cow<'a, [u8]>,
        body_words: usize,
        html: Vec<u8>,
        toc: Vec<TocItem>,
        excerpt: Vec<u8>,
        is_post: bool,
        post_rank: usize,
        newer: Option<usize>,
        older: Option<usize>,
        tag_indices: Vec<usize>,
        page_own_tags: Vec<(Vec<u8>, Vec<u8>, Option<usize>)>,
    }

    let mut sources: Vec<SourceItem<'a, '_>> = Vec::with_capacity(norm_texts.len());

    // 3. Parse front matter (borrowing into norm_texts!).
    // Note: keep a temporary vec of FM results aligned with `norm_texts` or build sources referencing `norm_texts`.
    // Wait: we can compute words_of + drop unused rejected files immediately; kept sources point to `norm_texts[k]`.
    struct FmParsed<'a, 'b> {
        src_path: &'a [u8],
        title: Cow<'b, [u8]>,
        date: Option<&'b [u8]>,
        raw_tags: Vec<&'b [u8]>,
        layout_idx: usize,
        is_post: bool,
        slug_override: Option<&'b [u8]>,
        summary: Option<Cow<'b, [u8]>>,
        body: &'b [u8],
    }
    let mut fm_kept: Vec<FmParsed<'a, '_>> = Vec::with_capacity(norm_texts.len());
    for (k, &orig_idx) in md_raw_indices.iter().enumerate() {
        let path = case.files[orig_idx].path.as_ref();
        let text = norm_texts[k].as_ref();
        let path_str = core::str::from_utf8(path).unwrap_or("");
        let fm = match parse_front_matter(text) {
            FmResult::Ok(v) => v,
            FmResult::Err(err) => {
                report_lines.push(format!("refused {}: {}", path_str, err));
                continue;
            }
        };
        let is_post = path.starts_with(b"posts/");
        if path.starts_with(b"tags/") {
            report_lines.push(format!("refused {}: tags/ is the generator's own", path_str));
            continue;
        }
        if path.starts_with(b"page/") {
            report_lines.push(format!("refused {}: page/ is the generator's own", path_str));
            continue;
        }
        if is_post && fm.date.is_none() {
            report_lines.push(format!("refused {}: a post needs a date", path_str));
            continue;
        }
        let layout_name = fm.layout.unwrap_or(if is_post { b"post" } else { b"page" });
        let layout_idx = match parsed_tpls.iter().position(|(n, _)| *n == layout_name) {
            Some(li) => li,
            None => {
                let lstr = core::str::from_utf8(layout_name).unwrap_or("");
                report_lines.push(format!("refused {}: layout \"{}\" is no template that parses", path_str, lstr));
                continue;
            }
        };
        if fm.draft {
            report_lines.push(format!("draft {}", path_str));
            continue;
        }
        fm_kept.push(FmParsed {
            src_path: path,
            title: fm.title,
            date: fm.date,
            raw_tags: fm.tags,
            layout_idx,
            is_post,
            slug_override: fm.slug,
            summary: fm.summary,
            body: fm.body,
        });
    }

    // 4. Permalinks (per-directory unique slugs).
    let mut dir_sets: Vec<(&[u8], OwnedStrSet)> = Vec::with_capacity(12);
    let mut root_set = OwnedStrSet::with_capacity(64);
    for r in RESERVED {
        root_set.insert(r.to_vec());
    }
    dir_sets.push((b"", root_set));

    let mut by_source = StrMap::with_capacity(fm_kept.len());
    for (si, f) in fm_kept.iter().enumerate() {
        let dir = if f.is_post { &b"posts"[..] } else { dir_of(f.src_path) };
        let set_pos = match dir_sets.iter().position(|(d, _)| *d == dir) {
            Some(p) => p,
            None => {
                dir_sets.push((dir, OwnedStrSet::with_capacity(32)));
                dir_sets.len() - 1
            }
        };
        let mut slug = match f.slug_override {
            Some(so) => so.to_vec(),
            None => slugify_vec(&f.title),
        };
        let base_len = slug.len();
        let taken = &mut dir_sets[set_pos].1;
        let mut n = 2usize;
        while taken.contains(&slug) {
            slug.truncate(base_len);
            slug.push(b'-');
            push_usize(n, &mut slug);
            n += 1;
        }
        taken.insert(slug.clone());

        let mut out_dir = Vec::with_capacity(dir.len() + 1 + slug.len());
        if dir.is_empty() {
            out_dir.extend_from_slice(&slug);
        } else {
            out_dir.extend_from_slice(dir);
            out_dir.push(b'/');
            out_dir.extend_from_slice(&slug);
        }
        let mut url = Vec::with_capacity(base.len() + 2 + out_dir.len());
        url.extend_from_slice(base);
        url.push(b'/');
        url.extend_from_slice(&out_dir);
        url.push(b'/');

        let mut date_long = Vec::new();
        if let Some(d) = f.date {
            date_long.reserve(20);
            long_date_into(d, &mut date_long);
        }
        let body_words = words_of(f.body);

        by_source.insert(f.src_path, si);
        sources.push(SourceItem {
            title: Cow::Owned(f.title.to_vec()),
            date: f.date,
            date_long,
            raw_tags: f.raw_tags.clone(),
            layout_idx: f.layout_idx,
            slug,
            out_dir,
            url,
            summary: f.summary.as_ref().map(|c| Cow::Owned(c.to_vec())).unwrap_or(Cow::Borrowed(b"")),
            body_words,
            html: Vec::new(),
            toc: Vec::new(),
            excerpt: Vec::new(),
            is_post: f.is_post,
            post_rank: 0,
            newer: None,
            older: None,
            tag_indices: Vec::new(),
            page_own_tags: Vec::new(),
        });
    }

    // 5. Markdown rendering & link resolution.
    let mut static_map = StrMap::with_capacity(statics.len());
    for (si, st) in statics.iter().enumerate() {
        static_map.insert(st.path, si);
    }
    let mut broken_links = OwnedStrSet::with_capacity(64);
    let mut rel_buf = Vec::with_capacity(128);

    // Collect URLs separately so link closure doesn't conflict with mutably updating sources[si].
    let md_outputs: Vec<_> = {
        let urls: Vec<&[u8]> = sources.iter().map(|s| s.url.as_slice()).collect();
        let mut outs = Vec::with_capacity(fm_kept.len());
        for f in &fm_kept {
            let dir = dir_of(f.src_path);
            let mut link_fn = |target: &[u8], out: &mut Vec<u8>| {
                if has_uri_scheme(target) || target.starts_with(b"/") || target.starts_with(b"#") {
                    out.extend_from_slice(target);
                    return;
                }
                let hash_pos = find_byte(target, 0, b'#');
                let (rel, frag) = if hash_pos < target.len() {
                    (&target[..hash_pos], &target[hash_pos..])
                } else {
                    (target, &b""[..])
                };
                let ok = !rel.is_empty() && resolve_rel_path(dir, rel, &mut rel_buf);
                if ok {
                    if let Some(target_si) = by_source.get(&rel_buf) {
                        out.extend_from_slice(urls[target_si]);
                        out.extend_from_slice(frag);
                        return;
                    }
                    if static_map.get(&rel_buf).is_some() {
                        out.extend_from_slice(base);
                        out.push(b'/');
                        out.extend_from_slice(&rel_buf);
                        out.extend_from_slice(frag);
                        return;
                    }
                }
                let mut bmsg = Vec::with_capacity(14 + f.src_path.len() + 4 + target.len());
                bmsg.extend_from_slice(b"broken-link ");
                bmsg.extend_from_slice(f.src_path);
                bmsg.extend_from_slice(b" -> ");
                bmsg.extend_from_slice(target);
                broken_links.insert(bmsg);
                out.extend_from_slice(target);
            };
            outs.push(render_markdown(f.body, &mut link_fn));
        }
        outs
    };
    for (si, md) in md_outputs.into_iter().enumerate() {
        sources[si].html = md.html;
        sources[si].toc = md.toc;
        sources[si].excerpt = md.excerpt;
    }

    // 6. Collections: posts sorted newest date first, then slug ascending (code units).
    let mut posts: Vec<usize> = sources
        .iter()
        .enumerate()
        .filter_map(|(i, s)| if s.is_post { Some(i) } else { None })
        .collect();
    posts.sort_by(|&a, &b| {
        let da = sources[a].date.unwrap_or(b"");
        let db = sources[b].date.unwrap_or(b"");
        cmp_utf16(db, da).then_with(|| cmp_utf16(&sources[a].slug, &sources[b].slug))
    });
    for (k, &si) in posts.iter().enumerate() {
        sources[si].post_rank = k;
        sources[si].newer = if k == 0 { None } else { Some(posts[k - 1]) };
        sources[si].older = if k + 1 == posts.len() { None } else { Some(posts[k + 1]) };
    }

    // 7. Tags: grouped by slug in posts' collections order, then ordered by count desc & slug asc.
    struct TagEntry<'a> {
        name: &'a [u8],
        slug: Vec<u8>,
        url: Vec<u8>,
        posts: Vec<usize>, // stores `post_rank` (index in `posts`)!
    }
    let mut tag_entries: Vec<TagEntry<'_>> = Vec::with_capacity(64);
    struct TagSlugIndex {
        slugs: Vec<Vec<u8>>,
        slots: Vec<Option<(u64, usize)>>,
        mask: usize,
    }
    impl TagSlugIndex {
        fn new(n: usize) -> Self {
            let cap = (n.max(16) * 2).next_power_of_two();
            Self { slugs: Vec::with_capacity(n), slots: vec![None; cap], mask: cap - 1 }
        }
        fn get(&self, slug: &[u8]) -> Option<usize> {
            let h = fnv1a(slug);
            let mut idx = (h as usize) & self.mask;
            loop {
                match self.slots[idx] {
                    None => return None,
                    Some((eh, id)) if eh == h && self.slugs[id] == slug => return Some(id),
                    _ => idx = (idx + 1) & self.mask,
                }
            }
        }
        fn get_or_insert(&mut self, slug: Vec<u8>) -> (usize, bool) {
            if (self.slugs.len() + 1) * 2 > self.slots.len() {
                let nc = self.slots.len() * 2;
                let nm = nc - 1;
                let mut ns = vec![None; nc];
                for (id, s) in self.slugs.iter().enumerate() {
                    let h = fnv1a(s);
                    let mut i = (h as usize) & nm;
                    while ns[i].is_some() { i = (i + 1) & nm; }
                    ns[i] = Some((h, id));
                }
                self.slots = ns;
                self.mask = nm;
            }
            let h = fnv1a(&slug);
            let mut idx = (h as usize) & self.mask;
            loop {
                match self.slots[idx] {
                    None => {
                        let id = self.slugs.len();
                        self.slugs.push(slug);
                        self.slots[idx] = Some((h, id));
                        return (id, true);
                    }
                    Some((eh, id)) if eh == h && self.slugs[id] == slug => return (id, false),
                    _ => idx = (idx + 1) & self.mask,
                }
            }
        }
    }

    let mut ts_idx = TagSlugIndex::new(64);
    for (post_rank, &si) in posts.iter().enumerate() {
        let mut seen_this_post: Vec<usize> = Vec::with_capacity(8);
        let raw_list = core::mem::take(&mut sources[si].raw_tags);
        for name in &raw_list {
            let slug = slugify_vec(name);
            let (tid, inserted) = ts_idx.get_or_insert(slug);
            if inserted {
                let s_ref = &ts_idx.slugs[tid];
                let mut turl = Vec::with_capacity(base.len() + 7 + s_ref.len());
                turl.extend_from_slice(base);
                turl.extend_from_slice(b"/tags/");
                turl.extend_from_slice(s_ref);
                turl.push(b'/');
                tag_entries.push(TagEntry {
                    name,
                    slug: s_ref.clone(),
                    url: turl,
                    posts: Vec::with_capacity(16),
                });
            }
            if !seen_this_post.contains(&tid) {
                seen_this_post.push(tid);
                tag_entries[tid].posts.push(post_rank);
            }
        }
        sources[si].tag_indices = seen_this_post;
        sources[si].raw_tags = raw_list;
    }

    // Non-post sources' tags (`ownTags`, matched against post tags by slug, else url="" and own name).
    for si in 0..sources.len() {
        if sources[si].is_post {
            continue;
        }
        let mut seen_slugs: Vec<Vec<u8>> = Vec::with_capacity(6);
        let mut own = Vec::with_capacity(6);
        for &name in &sources[si].raw_tags {
            let slug = slugify_vec(name);
            if seen_slugs.iter().any(|x| x == &slug) {
                continue;
            }
            seen_slugs.push(slug.clone());
            if let Some(tid) = ts_idx.get(&slug) {
                own.push((tag_entries[tid].name.to_vec(), tag_entries[tid].slug.clone(), Some(tid)));
            } else {
                own.push((name.to_vec(), slug, None));
            }
        }
        sources[si].page_own_tags = own;
    }

    let mut tag_order: Vec<usize> = (0..tag_entries.len()).collect();
    tag_order.sort_by(|&a, &b| {
        tag_entries[b]
            .posts
            .len()
            .cmp(&tag_entries[a].posts.len())
            .then_with(|| cmp_utf16(&tag_entries[a].slug, &tag_entries[b].slug))
    });

    // 8. Build template value objects and compile + site-fold templates!
    let site_val = SiteVal {
        title: &case.site.title,
        base_url: base,
    };

    let card_tag_vals: Vec<CardTagVal<'_>> = tag_entries
        .iter()
        .map(|te| CardTagVal {
            name: te.name,
            slug: &te.slug,
            url: &te.url,
        })
        .collect();

    let all_tag_summaries: Vec<TagSummaryVal<'_>> = tag_order
        .iter()
        .map(|&tid| {
            let te = &tag_entries[tid];
            TagSummaryVal {
                name: te.name,
                slug: &te.slug,
                url: &te.url,
                count: te.posts.len(),
            }
        })
        .collect();
    let all_tag_refs: Vec<&dyn ValueSource> = all_tag_summaries.iter().map(|t| t as &dyn ValueSource).collect();

    // Per-source CardTagVal slices:
    let mut source_tag_storage: Vec<Vec<CardTagVal<'_>>> = Vec::with_capacity(sources.len());
    for s in &sources {
        if s.is_post {
            source_tag_storage.push(Vec::new());
        } else {
            source_tag_storage.push(
                s.page_own_tags
                    .iter()
                    .map(|(nm, sl, tid_opt)| CardTagVal {
                        name: nm,
                        slug: sl,
                        url: match tid_opt {
                            Some(tid) => &tag_entries[*tid].url,
                            None => b"",
                        },
                    })
                    .collect(),
            );
        }
    }
    let mut source_tag_refs: Vec<Vec<&dyn ValueSource>> = Vec::with_capacity(sources.len());
    for (si, s) in sources.iter().enumerate() {
        if s.is_post {
            source_tag_refs.push(
                s.tag_indices
                    .iter()
                    .map(|&tid| &card_tag_vals[tid] as &dyn ValueSource)
                    .collect(),
            );
        } else {
            source_tag_refs.push(source_tag_storage[si].iter().map(|c| c as &dyn ValueSource).collect());
        }
    }

    let cards: Vec<CardVal<'_>> = sources
        .iter()
        .enumerate()
        .map(|(si, s)| CardVal {
            id: s.post_rank,
            title: &s.title,
            url: &s.url,
            date: s.date.unwrap_or(b""),
            date_long: &s.date_long,
            summary: &s.summary,
            excerpt: &s.excerpt,
            reading_minutes: ((s.body_words + 199) / 200).max(1),
            tag_refs: &source_tag_refs[si],
        })
        .collect();

    let recent_count = case.site.recent.min(posts.len());
    let recent_refs: Vec<&dyn ValueSource> = posts[..recent_count]
        .iter()
        .map(|&si| &cards[si] as &dyn ValueSource)
        .collect();

    let globals = GlobalsScope {
        site: &site_val,
        tags: &all_tag_refs,
        recent: &recent_refs,
    };

    // Compile, include-inline, and site-constant-fold each template role!
    let mut compiled_page_tpls: Vec<Vec<Op>> = Vec::with_capacity(parsed_tpls.len());
    for (name, nodes) in &parsed_tpls {
        // Check whether this template is ever referenced as a page/post layout (might still be `index`/`tag`/`tags` too!)
        let is_used_layout = sources.iter().any(|s| parsed_tpls[s.layout_idx].0 == *name);
        if is_used_layout {
            let mut inlined = Vec::new();
            inline_template(nodes, &get_tpl, 0, &mut inlined);
            let folded = fold_site_ops(&inlined, &globals, true, false, false, &[]);
            compiled_page_tpls.push(folded);
        } else {
            compiled_page_tpls.push(Vec::new());
        }
    }

    let compiled_index: Option<Vec<Op>> = get_tpl(b"index").map(|nodes| {
        let mut inlined = Vec::new();
        inline_template(nodes, &get_tpl, 0, &mut inlined);
        fold_site_ops(&inlined, &globals, false, false, true, &[])
    });
    let compiled_tag: Option<Vec<Op>> = get_tpl(b"tag").map(|nodes| {
        let mut inlined = Vec::new();
        inline_template(nodes, &get_tpl, 0, &mut inlined);
        fold_site_ops(&inlined, &globals, false, true, false, &[])
    });
    let compiled_tags: Option<Vec<Op>> = get_tpl(b"tags").map(|nodes| {
        let mut inlined = Vec::new();
        inline_template(nodes, &get_tpl, 0, &mut inlined);
        fold_site_ops(&inlined, &globals, false, false, false, &[])
    });

    // Discover pure card loop body pattern for CardCache!
    fn find_first_pure_card_body<'a>(ops: &'a [Op], path_root: &[u8]) -> Option<&'a [Op]> {
        for op in ops {
            match op {
                Op::For { path, body, pure_item_loop, .. } if *pure_item_loop && path[0].as_slice() == path_root => {
                    return Some(body.as_slice());
                }
                Op::If { then_ops, else_ops, .. } => {
                    if let Some(b) = find_first_pure_card_body(then_ops, path_root) {
                        return Some(b);
                    }
                    if let Some(eo) = else_ops {
                        if let Some(b) = find_first_pure_card_body(eo, path_root) {
                            return Some(b);
                        }
                    }
                }
                Op::For { body, .. } => {
                    if let Some(b) = find_first_pure_card_body(body, path_root) {
                        return Some(b);
                    }
                }
                _ => {}
            }
        }
        None
    }
    let card_pattern = compiled_index
        .as_ref()
        .and_then(|ops| find_first_pure_card_body(ops, b"posts"))
        .or_else(|| compiled_tag.as_ref().and_then(|ops| find_first_pure_card_body(ops, b"tag")));
    let mut card_cache = card_pattern.map(|pat| CardCache {
        body_pattern: pat,
        cached: vec![None; posts.len()],
    });

    // Precompute TOC & Link objects and emit JSON!
    out_json.push(b'{');
    let mut first_kv = true;
    let mut built_paths = OwnedStrSet::with_capacity(sources.len() + tag_entries.len() + 64);
    let mut key_buf: Vec<u8> = Vec::with_capacity(128);
    let mut page_html_buf: Vec<u8> = Vec::with_capacity(16 * 1024);

    let links: Vec<LinkVal<'_>> = sources
        .iter()
        .map(|s| LinkVal {
            title: &s.title,
            url: &s.url,
            date: s.date.unwrap_or(b""),
            date_long: &s.date_long,
        })
        .collect();

    let toc_storage: Vec<Vec<TocVal<'_>>> = sources
        .iter()
        .map(|s| {
            s.toc
                .iter()
                .map(|h| TocVal {
                    level: h.level as usize,
                    id: &h.id,
                    html: &h.html,
                })
                .collect()
        })
        .collect();
    let toc_refs: Vec<Vec<&dyn ValueSource>> = toc_storage
        .iter()
        .map(|tv| tv.iter().map(|h| h as &dyn ValueSource).collect())
        .collect();

    // Render all pages & posts
    let mut no_cache: Option<CardCache<'_>> = None;
    for (si, s) in sources.iter().enumerate() {
        let pval = PageVal {
            card: &cards[si],
            slug: &s.slug,
            content: &s.html,
            toc: &toc_refs[si],
            newer: s.newer.map(|ni| &links[ni]),
            older: s.older.map(|oi| &links[oi]),
            is_post: s.is_post,
        };
        let pscope = PageScope { page: &pval };
        let gscope = Scope { vars: &globals, parent: None };
        let scope = Scope { vars: &pscope, parent: Some(&gscope) };
        page_html_buf.clear();
        render_ops(&compiled_page_tpls[s.layout_idx], &scope, &mut no_cache, &mut page_html_buf);

        key_buf.clear();
        key_buf.extend_from_slice(&s.out_dir);
        key_buf.extend_from_slice(b"/index.html");
        built_paths.insert(key_buf.clone());
        emit_kv_pre_json(out_json, &mut first_kv, &key_buf, &page_html_buf);
    }

    // Render paginated index pages
    if let Some(ref idx_ops) = compiled_index {
        let per = case.site.per_page;
        let pages = ((posts.len() + per - 1) / per).max(1);
        let make_index_url = |k: usize, dst: &mut Vec<u8>| {
            dst.clear();
            dst.extend_from_slice(base);
            if k == 1 {
                dst.push(b'/');
            } else {
                dst.extend_from_slice(b"/page/");
                push_usize(k, dst);
                dst.push(b'/');
            }
        };
        let mut u_cur = Vec::with_capacity(64);
        let mut u_newer = Vec::with_capacity(64);
        let mut u_older = Vec::with_capacity(64);
        let mut post_card_refs: Vec<&dyn ValueSource> = Vec::with_capacity(per);
        for k in 1..=pages {
            let start = (k - 1) * per;
            let end = (k * per).min(posts.len());
            post_card_refs.clear();
            for &si in &posts[start..end] {
                post_card_refs.push(&cards[si]);
            }
            make_index_url(k, &mut u_cur);
            if k == 1 {
                u_newer.clear();
            } else {
                make_index_url(k - 1, &mut u_newer);
            }
            if k == pages {
                u_older.clear();
            } else {
                make_index_url(k + 1, &mut u_older);
            }
            let pager = PagerVal {
                number: k,
                total: pages,
                url: &u_cur,
                newer_url: &u_newer,
                older_url: &u_older,
            };
            let iscope = IndexScope {
                posts: &post_card_refs,
                pager: &pager,
            };
            let gscope = Scope { vars: &globals, parent: None };
            let scope = Scope { vars: &iscope, parent: Some(&gscope) };
            page_html_buf.clear();
            render_ops(idx_ops, &scope, &mut card_cache, &mut page_html_buf);

            key_buf.clear();
            if k == 1 {
                key_buf.extend_from_slice(b"index.html");
            } else {
                key_buf.extend_from_slice(b"page/");
                push_usize(k, &mut key_buf);
                key_buf.extend_from_slice(b"/index.html");
            }
            built_paths.insert(key_buf.clone());
            emit_kv_pre_json(out_json, &mut first_kv, &key_buf, &page_html_buf);
        }
    }

    // Render tag pages
    if let Some(ref tag_ops) = compiled_tag {
        let mut tpost_refs: Vec<&dyn ValueSource> = Vec::with_capacity(64);
        for &tid in &tag_order {
            let te = &tag_entries[tid];
            tpost_refs.clear();
            for &pr in &te.posts {
                tpost_refs.push(&cards[posts[pr]]);
            }
            let tval = TagFullVal {
                name: te.name,
                slug: &te.slug,
                url: &te.url,
                count: te.posts.len(),
                posts: &tpost_refs,
            };
            let tscope = TagScope { tag: &tval };
            let gscope = Scope { vars: &globals, parent: None };
            let scope = Scope { vars: &tscope, parent: Some(&gscope) };
            page_html_buf.clear();
            render_ops(tag_ops, &scope, &mut card_cache, &mut page_html_buf);

            key_buf.clear();
            key_buf.extend_from_slice(b"tags/");
            key_buf.extend_from_slice(&te.slug);
            key_buf.extend_from_slice(b"/index.html");
            built_paths.insert(key_buf.clone());
            emit_kv_pre_json(out_json, &mut first_kv, &key_buf, &page_html_buf);
        }
    }

    // Render tags/index.html
    if let Some(ref tags_ops) = compiled_tags {
        let gscope = Scope { vars: &globals, parent: None };
        page_html_buf.clear();
        render_ops(tags_ops, &gscope, &mut no_cache, &mut page_html_buf);
        built_paths.insert(b"tags/index.html".to_vec());
        emit_kv_pre_json(out_json, &mut first_kv, b"tags/index.html", &page_html_buf);
    }

    // Static files (`shadowed` if a built page already has its path).
    for st in &statics {
        if built_paths.contains(st.path) {
            let pstr = core::str::from_utf8(st.path).unwrap_or("");
            report_lines.push(format!("shadowed {}: a built page has its path", pstr));
        } else if st.path != b"build-report.txt" {
            emit_kv_raw(out_json, &mut first_kv, st.path, st.content);
        }
    }

    // 9. Report (`build-report.txt`)
    let mut all_lines: Vec<Vec<u8>> = Vec::with_capacity(report_lines.len() + broken_links.items.len());
    for l in report_lines {
        all_lines.push(l.into_bytes());
    }
    for b in broken_links.items {
        all_lines.push(b);
    }
    all_lines.sort_by(|a, b| cmp_utf16(a, b));

    let num_pages = sources.len() - posts.len();
    let summary = format!(
        "built {} pages, {} posts, {} tags and {} static files",
        num_pages,
        posts.len(),
        tag_entries.len(),
        statics.len()
    );
    all_lines.push(summary.into_bytes());

    let mut report_text = Vec::with_capacity(all_lines.len() * 48);
    for l in &all_lines {
        report_text.extend_from_slice(l);
        report_text.push(b'\n');
    }
    emit_kv_raw(out_json, &mut first_kv, b"build-report.txt", &report_text);
    out_json.push(b'}');
}
