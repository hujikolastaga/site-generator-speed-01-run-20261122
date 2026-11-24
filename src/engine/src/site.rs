use alloc::borrow::Cow;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use crate::frontmatter::{normalize_newlines, parse_front_matter, FmResult};
use crate::json::Case;
use crate::markdown::{render_markdown_into, MdScratch, TocItem};
use crate::out::ByteSink;
use crate::simd::find_byte;
use crate::template::{
    compile_ops, fold_site_ops, inline_template, ops_use_escaped_md_html, parse_template,
    render_compiled_ops, CompiledOp, LoopFrame, Node, Op, RenderCtx, RuntimeCard, RuntimeCardTag,
    RuntimePage, RuntimePager, RuntimeTagPage, RuntimeTagSummary, Val, ValueSource,
};
use crate::text::{
    cmp_utf16, escape_html_then_json_into, escape_json_into, fast_hash, long_date_into,
    needs_html_or_json_escape, needs_json_escape, push_usize, slugify_into, words_of,
};

const RESERVED: [&[u8]; 3] = [b"posts", b"tags", b"page"];

// ---------------------------------------------------------------- Hash Maps / Sets (64-bit fast_hash)

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
        let h = fast_hash(key);
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
        let h = fast_hash(key);
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
                let h = fast_hash(item);
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

    fn insert_slice(&mut self, val: &[u8]) -> bool {
        self.maybe_grow();
        let h = fast_hash(val);
        let mut idx = (h as usize) & self.mask;
        loop {
            match self.slots[idx] {
                None => {
                    let id = self.items.len();
                    self.items.push(val.to_vec());
                    self.slots[idx] = Some((h, id));
                    return true;
                }
                Some((eh, id)) if eh == h && self.items[id] == val => return false,
                _ => idx = (idx + 1) & self.mask,
            }
        }
    }

    #[inline]
    fn contains(&self, val: &[u8]) -> bool {
        let h = fast_hash(val);
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

struct TagSlugIndex {
    slugs: Vec<Vec<u8>>,
    slots: Vec<Option<(u64, usize)>>,
    mask: usize,
}

impl TagSlugIndex {
    fn new(n: usize) -> Self {
        let cap = (n.max(16) * 2).next_power_of_two();
        Self {
            slugs: Vec::with_capacity(n),
            slots: vec![None; cap],
            mask: cap - 1,
        }
    }

    #[inline]
    fn get(&self, slug: &[u8]) -> Option<usize> {
        let h = fast_hash(slug);
        let mut idx = (h as usize) & self.mask;
        loop {
            match self.slots[idx] {
                None => return None,
                Some((eh, id)) if eh == h && self.slugs[id] == slug => return Some(id),
                _ => idx = (idx + 1) & self.mask,
            }
        }
    }

    #[inline]
    fn get_or_insert_slice(&mut self, slug: &[u8]) -> (usize, bool) {
        if (self.slugs.len() + 1) * 2 > self.slots.len() {
            let nc = self.slots.len() * 2;
            let nm = nc - 1;
            let mut ns = vec![None; nc];
            for (id, s) in self.slugs.iter().enumerate() {
                let h = fast_hash(s);
                let mut i = (h as usize) & nm;
                while ns[i].is_some() {
                    i = (i + 1) & nm;
                }
                ns[i] = Some((h, id));
            }
            self.slots = ns;
            self.mask = nm;
        }
        let h = fast_hash(slug);
        let mut idx = (h as usize) & self.mask;
        loop {
            match self.slots[idx] {
                None => {
                    let id = self.slugs.len();
                    self.slugs.push(slug.to_vec());
                    self.slots[idx] = Some((h, id));
                    return (id, true);
                }
                Some((eh, id)) if eh == h && self.slugs[id] == slug => return (id, false),
                _ => idx = (idx + 1) & self.mask,
            }
        }
    }
}

// ---------------------------------------------------------------- Path / URI Helpers

#[inline]
fn dir_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&c| c == b'/') {
        Some(pos) => &path[..pos],
        None => b"",
    }
}

#[inline]
fn resolve_rel_path(dir: &[u8], rel: &[u8], out: &mut Vec<u8>, seg_ends: &mut Vec<usize>) -> bool {
    out.clear();
    seg_ends.clear();
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

// ---------------------------------------------------------------- Fold Value Adapters (Site Globals Only)

struct FoldSite<'a> {
    title: &'a [u8],
    base_url: &'a [u8],
}
impl<'a> ValueSource for FoldSite<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"title" => Some(Val::Str(self.title)),
            b"base_url" => Some(Val::Str(self.base_url)),
            _ => None,
        }
    }
}

struct FoldTagSummary<'a> {
    name: &'a [u8],
    slug: &'a [u8],
    url: &'a [u8],
    count: usize,
}
impl<'a> ValueSource for FoldTagSummary<'a> {
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

struct FoldCardTag<'a> {
    name: &'a [u8],
    slug: &'a [u8],
    url: &'a [u8],
}
impl<'a> ValueSource for FoldCardTag<'a> {
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

struct FoldRecentCard<'a> {
    md_json_mode: bool,
    title: &'a [u8],
    url: &'a [u8],
    date: &'a [u8],
    date_long: &'a [u8],
    summary: &'a [u8],
    excerpt: &'a [u8],
    reading_minutes: usize,
    tag_refs: &'a [&'a dyn ValueSource],
}
impl<'a> ValueSource for FoldRecentCard<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"title" => Some(Val::Str(self.title)),
            b"url" => Some(Val::Str(self.url)),
            b"date" => Some(Val::Str(self.date)),
            b"date_long" => Some(Val::Str(self.date_long)),
            b"summary" => Some(Val::Str(self.summary)),
            b"excerpt" => Some(if self.md_json_mode {
                Val::PreJsonStr(self.excerpt)
            } else {
                Val::Str(self.excerpt)
            }),
            b"reading_minutes" => Some(Val::Num(self.reading_minutes)),
            b"tags" => Some(Val::Arr(self.tag_refs)),
            _ => None,
        }
    }
}

struct FoldGlobals<'a> {
    site: &'a FoldSite<'a>,
    tags: &'a [&'a dyn ValueSource],
    recent: &'a [&'a dyn ValueSource],
}
impl<'a> ValueSource for FoldGlobals<'a> {
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

// ---------------------------------------------------------------- JSON Output Emitter

#[inline(always)]
fn escape_pre_vec(raw: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut hj = Vec::with_capacity(raw.len() + 4);
    escape_html_then_json_into(raw, &mut hj);
    let mut j = Vec::with_capacity(raw.len() + 4);
    escape_json_into(raw, &mut j);
    (hj, j)
}

#[inline(always)]
fn emit_kv_raw<S: ByteSink>(out: &mut S, first: &mut bool, key: &[u8], raw_val: &[u8]) {
    if !*first {
        out.push(b',');
    }
    *first = false;
    out.push(b'"');
    escape_json_into(key, out);
    out.write_slice(b"\":\"");
    escape_json_into(raw_val, out);
    out.push(b'"');
}

// ---------------------------------------------------------------- Site Pipeline

pub fn build_site_into_json<'a, S: ByteSink>(case: &'a Case<'a>, out_json: &mut S) {
    let pool = &case.str_pool;

    // 1. Parse templates sorted by name (UTF-16 code units).
    let mut tpl_order: Vec<usize> = (0..case.templates.len()).collect();
    tpl_order.sort_by(|&a, &b| {
        cmp_utf16(
            case.templates[a].name.as_bytes(pool),
            case.templates[b].name.as_bytes(pool),
        )
    });

    let mut parsed_tpls: Vec<(&[u8], Vec<Node>)> = Vec::with_capacity(tpl_order.len());
    let mut report_lines: Vec<String> = Vec::new();
    for idx in tpl_order {
        let t = &case.templates[idx];
        let name = t.name.as_bytes(pool);
        let source = t.source.as_bytes(pool);
        match parse_template(name, source) {
            Ok(nodes) => parsed_tpls.push((name, nodes)),
            Err(msg) => report_lines.push(msg),
        }
    }
    let get_tpl = |name: &[u8]| -> Option<&[Node]> {
        parsed_tpls.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_slice())
    };

    let site_title = case.site.title.as_bytes(pool);
    let mut base: &[u8] = case.site.base_url.as_bytes(pool);
    while base.ends_with(b"/") {
        base = &base[..base.len() - 1];
    }
    let base_safe_hj = !needs_html_or_json_escape(base);
    let base_safe_j = base_safe_hj || !needs_json_escape(base);

    // 2. Partition & sort input files by path (UTF-16 code units).
    let mut file_order: Vec<usize> = (0..case.files.len()).collect();
    file_order.sort_by(|&a, &b| {
        cmp_utf16(
            case.files[a].path.as_bytes(pool),
            case.files[b].path.as_bytes(pool),
        )
    });

    struct StaticItem<'a> {
        path: &'a [u8],
        content: &'a [u8],
    }

    let mut statics: Vec<StaticItem<'a>> = Vec::with_capacity(16);
    let mut norm_texts: Vec<Cow<'a, [u8]>> = Vec::with_capacity(file_order.len());
    let mut md_raw_indices: Vec<usize> = Vec::with_capacity(file_order.len());

    for idx in file_order {
        let f = &case.files[idx];
        let p = f.path.as_bytes(pool);
        let c = f.content.as_bytes(pool);
        if p.ends_with(b".md") {
            if !f.has_cr {
                norm_texts.push(Cow::Borrowed(c));
            } else {
                norm_texts.push(normalize_newlines(c));
            }
            md_raw_indices.push(idx);
        } else {
            statics.push(StaticItem { path: p, content: c });
        }
    }

    // 3. Parse front matter.
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
    let mut used_layout_flags = vec![false; parsed_tpls.len()];

    for (k, &orig_idx) in md_raw_indices.iter().enumerate() {
        let path = case.files[orig_idx].path.as_bytes(pool);
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
                report_lines.push(format!(
                    "refused {}: layout \"{}\" is no template that parses",
                    path_str, lstr
                ));
                continue;
            }
        };
        if fm.draft {
            report_lines.push(format!("draft {}", path_str));
            continue;
        }
        used_layout_flags[layout_idx] = true;
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

    // Determine `md_json_mode` early by inlining all used templates!
    let mut inlined_pages: Vec<Option<Vec<Op>>> = vec![None; parsed_tpls.len()];
    let mut uses_escaped_md = false;
    for (li, (_, nodes)) in parsed_tpls.iter().enumerate() {
        if used_layout_flags[li] {
            let mut ops = Vec::with_capacity(32);
            inline_template(nodes, &get_tpl, 0, &mut ops);
            if ops_use_escaped_md_html(&ops) {
                uses_escaped_md = true;
            }
            inlined_pages[li] = Some(ops);
        }
    }
    let inlined_index: Option<Vec<Op>> = get_tpl(b"index").map(|nodes| {
        let mut ops = Vec::with_capacity(32);
        inline_template(nodes, &get_tpl, 0, &mut ops);
        if ops_use_escaped_md_html(&ops) {
            uses_escaped_md = true;
        }
        ops
    });
    let inlined_tag: Option<Vec<Op>> = get_tpl(b"tag").map(|nodes| {
        let mut ops = Vec::with_capacity(32);
        inline_template(nodes, &get_tpl, 0, &mut ops);
        if ops_use_escaped_md_html(&ops) {
            uses_escaped_md = true;
        }
        ops
    });
    let inlined_tags: Option<Vec<Op>> = get_tpl(b"tags").map(|nodes| {
        let mut ops = Vec::with_capacity(32);
        inline_template(nodes, &get_tpl, 0, &mut ops);
        if ops_use_escaped_md_html(&ops) {
            uses_escaped_md = true;
        }
        ops
    });
    let md_json_mode = !uses_escaped_md;

    // 4. Permalinks (per-directory unique slugs).
    struct SourceItem<'a, 'b> {
        title: Cow<'b, [u8]>,
        title_safe_hj: bool,
        title_safe_j: bool,
        date: Option<&'b [u8]>,
        date_long: Vec<u8>,
        raw_tags: Vec<&'b [u8]>,
        layout_idx: usize,
        slug: Vec<u8>,
        out_dir: Vec<u8>,
        url: Vec<u8>,
        url_safe_hj: bool,
        url_safe_j: bool,
        summary: Cow<'b, [u8]>,
        summary_safe_hj: bool,
        summary_safe_j: bool,
        reading_minutes: usize,
        is_post: bool,
        post_rank: usize,
        newer: Option<usize>,
        older: Option<usize>,
        tag_indices: Vec<usize>,
        own_tags_meta: Vec<(Vec<u8>, Vec<u8>, Option<usize>)>,
        recent_slot: Option<usize>,
        body: &'b [u8],
        src_path: &'a [u8],
    }

    let mut dir_sets: Vec<(&[u8], OwnedStrSet)> = Vec::with_capacity(12);
    let mut root_set = OwnedStrSet::with_capacity(64);
    for r in RESERVED {
        root_set.insert_slice(r);
    }
    dir_sets.push((b"", root_set));

    let mut by_source = StrMap::with_capacity(fm_kept.len());
    let mut sources: Vec<SourceItem<'a, '_>> = Vec::with_capacity(fm_kept.len());
    let mut slug_buf: Vec<u8> = Vec::with_capacity(72);

    for (si, f) in fm_kept.into_iter().enumerate() {
        let dir = if f.is_post { &b"posts"[..] } else { dir_of(f.src_path) };
        let set_pos = match dir_sets.iter().position(|(d, _)| *d == dir) {
            Some(p) => p,
            None => {
                dir_sets.push((dir, OwnedStrSet::with_capacity(32)));
                dir_sets.len() - 1
            }
        };
        slug_buf.clear();
        match f.slug_override {
            Some(so) => slug_buf.extend_from_slice(so),
            None => slugify_into(&f.title, &mut slug_buf),
        }
        let base_len = slug_buf.len();
        let taken = &mut dir_sets[set_pos].1;
        let mut n = 2usize;
        while taken.contains(&slug_buf) {
            slug_buf.truncate(base_len);
            slug_buf.push(b'-');
            push_usize(n, &mut slug_buf);
            n += 1;
        }
        taken.insert_slice(&slug_buf);
        let slug = slug_buf.clone();

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

        let url_safe_hj = base_safe_hj && !needs_html_or_json_escape(&out_dir);
        let url_safe_j = url_safe_hj || (base_safe_j && !needs_json_escape(&out_dir));

        let mut date_long = Vec::new();
        if let Some(d) = f.date {
            date_long.reserve(20);
            long_date_into(d, &mut date_long);
        }

        let title_safe_hj = !needs_html_or_json_escape(&f.title);
        let title_safe_j = title_safe_hj || !needs_json_escape(&f.title);
        let summary = f.summary.unwrap_or(Cow::Borrowed(&b""[..]));
        let summary_safe_hj = summary.is_empty() || !needs_html_or_json_escape(&summary);
        let summary_safe_j = summary_safe_hj || !needs_json_escape(&summary);

        let body_words = words_of(f.body);
        let reading_minutes = ((body_words + 199) / 200).max(1);

        by_source.insert(f.src_path, si);
        sources.push(SourceItem {
            title: f.title,
            title_safe_hj,
            title_safe_j,
            date: f.date,
            date_long,
            raw_tags: f.raw_tags,
            layout_idx: f.layout_idx,
            slug,
            out_dir,
            url,
            url_safe_hj,
            url_safe_j,
            summary,
            summary_safe_hj,
            summary_safe_j,
            reading_minutes,
            is_post: f.is_post,
            post_rank: 0,
            newer: None,
            older: None,
            tag_indices: Vec::new(),
            own_tags_meta: Vec::new(),
            recent_slot: None,
            body: f.body,
            src_path: f.src_path,
        });
    }

    // 5. Link resolver shared structures.
    let mut static_map = StrMap::with_capacity(statics.len());
    for (idx, st) in statics.iter().enumerate() {
        static_map.insert(st.path, idx);
    }
    let has_index_static = statics.iter().any(|st| st.path.ends_with(b"index.html"));
    let mut static_shadowed = vec![false; statics.len()];

    let mut broken_links = OwnedStrSet::with_capacity(64);
    let mut rel_buf = Vec::with_capacity(128);
    let mut seg_ends: Vec<usize> = Vec::with_capacity(12);

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
    let recent_count = case.site.recent.min(posts.len());
    for (ri, &si) in posts[..recent_count].iter().enumerate() {
        sources[si].recent_slot = Some(ri);
    }

    // 7. Tags: grouped by slug in posts' collection order, sorted by count desc & slug asc.
    struct TagEntry<'b> {
        name: &'b [u8],
        name_hj: Vec<u8>,
        name_j: Vec<u8>,
        slug: Vec<u8>,
        url: Vec<u8>,
        url_hj: Vec<u8>,
        url_j: Vec<u8>,
        post_sis: Vec<usize>,
    }
    let mut tag_entries: Vec<TagEntry<'_>> = Vec::with_capacity(64);
    let mut ts_idx = TagSlugIndex::new(64);

    for &si in &posts {
        let mut seen_this_post: Vec<usize> = Vec::with_capacity(8);
        let raw_list = core::mem::take(&mut sources[si].raw_tags);
        for name in raw_list {
            slug_buf.clear();
            slugify_into(name, &mut slug_buf);
            let (tid, inserted) = ts_idx.get_or_insert_slice(&slug_buf);
            if inserted {
                let s_ref = &ts_idx.slugs[tid];
                let mut turl = Vec::with_capacity(base.len() + 7 + s_ref.len());
                turl.extend_from_slice(base);
                turl.extend_from_slice(b"/tags/");
                turl.extend_from_slice(s_ref);
                turl.push(b'/');
                let (name_hj, name_j) = escape_pre_vec(name);
                let (url_hj, url_j) = escape_pre_vec(&turl);
                tag_entries.push(TagEntry {
                    name,
                    name_hj,
                    name_j,
                    slug: s_ref.clone(),
                    url: turl,
                    url_hj,
                    url_j,
                    post_sis: Vec::with_capacity(16),
                });
            }
            if !seen_this_post.contains(&tid) {
                seen_this_post.push(tid);
                tag_entries[tid].post_sis.push(si);
            }
        }
        sources[si].tag_indices = seen_this_post;
    }

    for si in 0..sources.len() {
        if sources[si].is_post {
            continue;
        }
        let mut seen_slugs: Vec<Vec<u8>> = Vec::with_capacity(6);
        let mut own = Vec::with_capacity(6);
        let raw_list = core::mem::take(&mut sources[si].raw_tags);
        for name in raw_list {
            slug_buf.clear();
            slugify_into(name, &mut slug_buf);
            if seen_slugs.iter().any(|x| x.as_slice() == slug_buf.as_slice()) {
                continue;
            }
            seen_slugs.push(slug_buf.clone());
            if let Some(tid) = ts_idx.get(&slug_buf) {
                own.push((
                    tag_entries[tid].name.to_vec(),
                    tag_entries[tid].slug.clone(),
                    Some(tid),
                ));
            } else {
                own.push((name.to_vec(), slug_buf.clone(), None));
            }
        }
        sources[si].own_tags_meta = own;
    }

    let mut tag_order: Vec<usize> = (0..tag_entries.len()).collect();
    tag_order.sort_by(|&a, &b| {
        tag_entries[b]
            .post_sis
            .len()
            .cmp(&tag_entries[a].post_sis.len())
            .then_with(|| cmp_utf16(&tag_entries[a].slug, &tag_entries[b].slug))
    });

    // 8. Pre-render ONLY the `recent_count` posts before site constant folding!
    struct RecentRendered {
        html: Vec<u8>,
        toc: Vec<TocItem>,
        excerpt: Vec<u8>,
    }
    let urls: Vec<&[u8]> = sources.iter().map(|s| s.url.as_slice()).collect();
    let mut md_scratch = MdScratch::new();
    let mut recent_pre: Vec<RecentRendered> = Vec::with_capacity(recent_count);

    for &si in &posts[..recent_count] {
        let src_path = sources[si].src_path;
        let dir = dir_of(src_path);
        let body = sources[si].body;
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
            let ok = !rel.is_empty() && resolve_rel_path(dir, rel, &mut rel_buf, &mut seg_ends);
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
            let mut bmsg = Vec::with_capacity(16 + src_path.len() + target.len());
            bmsg.extend_from_slice(b"broken-link ");
            bmsg.extend_from_slice(src_path);
            bmsg.extend_from_slice(b" -> ");
            bmsg.extend_from_slice(target);
            broken_links.insert_slice(&bmsg);
            out.extend_from_slice(target);
        };
        if md_json_mode {
            render_markdown_into::<true, _>(body, &mut link_fn, &mut md_scratch);
        } else {
            render_markdown_into::<false, _>(body, &mut link_fn, &mut md_scratch);
        }
        let ex = md_scratch.excerpt_slice().to_vec();
        recent_pre.push(RecentRendered {
            html: core::mem::take(&mut md_scratch.html),
            toc: core::mem::take(&mut md_scratch.toc),
            excerpt: ex,
        });
    }

    // Build FoldGlobals and fold + compile templates!
    let fold_site = FoldSite {
        title: site_title,
        base_url: base,
    };
    let fold_tag_cards: Vec<FoldCardTag<'_>> = tag_entries
        .iter()
        .map(|te| FoldCardTag {
            name: te.name,
            slug: &te.slug,
            url: &te.url,
        })
        .collect();
    let fold_all_tags: Vec<FoldTagSummary<'_>> = tag_order
        .iter()
        .map(|&tid| {
            let te = &tag_entries[tid];
            FoldTagSummary {
                name: te.name,
                slug: &te.slug,
                url: &te.url,
                count: te.post_sis.len(),
            }
        })
        .collect();
    let fold_all_tag_refs: Vec<&dyn ValueSource> =
        fold_all_tags.iter().map(|t| t as &dyn ValueSource).collect();

    let mut fold_recent_tag_refs: Vec<Vec<&dyn ValueSource>> = Vec::with_capacity(recent_count);
    for &si in &posts[..recent_count] {
        fold_recent_tag_refs.push(
            sources[si]
                .tag_indices
                .iter()
                .map(|&tid| &fold_tag_cards[tid] as &dyn ValueSource)
                .collect(),
        );
    }
    let fold_recent_cards: Vec<FoldRecentCard<'_>> = posts[..recent_count]
        .iter()
        .enumerate()
        .map(|(ri, &si)| {
            let s = &sources[si];
            FoldRecentCard {
                md_json_mode,
                title: &s.title,
                url: &s.url,
                date: s.date.unwrap_or(b""),
                date_long: &s.date_long,
                summary: &s.summary,
                excerpt: &recent_pre[ri].excerpt,
                reading_minutes: s.reading_minutes,
                tag_refs: &fold_recent_tag_refs[ri],
            }
        })
        .collect();
    let fold_recent_refs: Vec<&dyn ValueSource> = fold_recent_cards
        .iter()
        .map(|c| c as &dyn ValueSource)
        .collect();
    let fold_globals = FoldGlobals {
        site: &fold_site,
        tags: &fold_all_tag_refs,
        recent: &fold_recent_refs,
    };

    let mut card_patterns: Vec<Vec<CompiledOp>> = Vec::new();
    let compiled_pages: Vec<Option<Vec<CompiledOp>>> = inlined_pages
        .into_iter()
        .map(|opt| {
            opt.map(|ops| {
                let folded = fold_site_ops(&ops, &fold_globals, true, false, false, &[]);
                compile_ops(&folded, true, false, false, &mut card_patterns)
            })
        })
        .collect();
    let compiled_index: Option<Vec<CompiledOp>> = inlined_index.map(|ops| {
        let folded = fold_site_ops(&ops, &fold_globals, false, false, true, &[]);
        compile_ops(&folded, false, false, true, &mut card_patterns)
    });
    let compiled_tag: Option<Vec<CompiledOp>> = inlined_tag.map(|ops| {
        let folded = fold_site_ops(&ops, &fold_globals, false, true, false, &[]);
        compile_ops(&folded, false, true, false, &mut card_patterns)
    });
    let compiled_tags: Option<Vec<CompiledOp>> = inlined_tags.map(|ops| {
        let folded = fold_site_ops(&ops, &fold_globals, false, false, false, &[]);
        compile_ops(&folded, false, false, false, &mut card_patterns)
    });

    // Build non-post own tag runtime structs + pre-escaped strings.
    struct OwnTagPre {
        name: Vec<u8>,
        name_hj: Vec<u8>,
        name_j: Vec<u8>,
        slug: Vec<u8>,
    }
    let mut own_tag_pres: Vec<Vec<OwnTagPre>> = Vec::with_capacity(sources.len());
    for s in &sources {
        if s.is_post || s.own_tags_meta.is_empty() {
            own_tag_pres.push(Vec::new());
        } else {
            own_tag_pres.push(
                s.own_tags_meta
                    .iter()
                    .filter(|(_, _, tid_opt)| tid_opt.is_none())
                    .map(|(nm, sl, _)| {
                        let (name_hj, name_j) = escape_pre_vec(nm);
                        OwnTagPre {
                            name: nm.clone(),
                            name_hj,
                            name_j,
                            slug: sl.clone(),
                        }
                    })
                    .collect(),
            );
        }
    }

    let post_tag_cards: Vec<RuntimeCardTag<'_>> = tag_entries
        .iter()
        .map(|te| RuntimeCardTag {
            name: te.name,
            name_hj: &te.name_hj,
            name_j: &te.name_j,
            slug: &te.slug,
            url: &te.url,
            url_hj: &te.url_hj,
            url_j: &te.url_j,
        })
        .collect();

    let mut source_own_tag_cards: Vec<Vec<RuntimeCardTag<'_>>> = Vec::with_capacity(sources.len());
    for (si, s) in sources.iter().enumerate() {
        if s.is_post || s.own_tags_meta.is_empty() {
            source_own_tag_cards.push(Vec::new());
        } else {
            let mut unresolved_idx = 0usize;
            let mut v = Vec::with_capacity(s.own_tags_meta.len());
            for &(_, _, tid_opt) in &s.own_tags_meta {
                match tid_opt {
                    Some(tid) => {
                        let te = &tag_entries[tid];
                        v.push(RuntimeCardTag {
                            name: te.name,
                            name_hj: &te.name_hj,
                            name_j: &te.name_j,
                            slug: &te.slug,
                            url: &te.url,
                            url_hj: &te.url_hj,
                            url_j: &te.url_j,
                        });
                    }
                    None => {
                        let pre = &own_tag_pres[si][unresolved_idx];
                        unresolved_idx += 1;
                        v.push(RuntimeCardTag {
                            name: &pre.name,
                            name_hj: &pre.name_hj,
                            name_j: &pre.name_j,
                            slug: &pre.slug,
                            url: b"",
                            url_hj: b"",
                            url_j: b"",
                        });
                    }
                }
            }
            source_own_tag_cards.push(v);
        }
    }

    let global_tags: Vec<RuntimeTagSummary<'_>> = tag_order
        .iter()
        .map(|&tid| {
            let te = &tag_entries[tid];
            RuntimeTagSummary {
                name: te.name,
                name_hj: &te.name_hj,
                name_j: &te.name_j,
                slug: &te.slug,
                url: &te.url,
                url_hj: &te.url_hj,
                url_j: &te.url_j,
                count: te.post_sis.len(),
            }
        })
        .collect();

    let mut cards: Vec<RuntimeCard<'_>> = sources
        .iter()
        .enumerate()
        .map(|(si, s)| RuntimeCard {
            post_rank: s.post_rank,
            title: &s.title,
            title_safe_hj: s.title_safe_hj,
            title_safe_j: s.title_safe_j,
            url: &s.url,
            url_safe_hj: s.url_safe_hj,
            url_safe_j: s.url_safe_j,
            date: s.date.unwrap_or(b""),
            date_long: &s.date_long,
            summary: &s.summary,
            summary_safe_hj: s.summary_safe_hj,
            summary_safe_j: s.summary_safe_j,
            excerpt: match s.recent_slot {
                Some(ri) => &recent_pre[ri].excerpt,
                None => b"",
            },
            reading_minutes: s.reading_minutes,
            tag_ids: &s.tag_indices,
            own_tags: &source_own_tag_cards[si],
            is_post: s.is_post,
        })
        .collect();

    let recent_sis = &posts[..recent_count];
    let mut frames: Vec<LoopFrame> = Vec::with_capacity(8);
    let mut caches: Vec<Vec<Option<Vec<u8>>>> = card_patterns
        .iter()
        .map(|_| vec![None; posts.len()])
        .collect();

    let mut key_buf: Vec<u8> = Vec::with_capacity(128);
    out_json.push(b'{');
    let mut first_kv = true;

    // 9. Stream-render every page/post directly into `out_json`!
    let mut post_excerpts: Vec<Option<Vec<u8>>> = vec![None; posts.len()];
    for (si, s) in sources.iter().enumerate() {
        let layout_ops = compiled_pages[s.layout_idx].as_ref().unwrap();
        let (content_slice, toc_slice, excerpt_slice): (&[u8], &[TocItem], &[u8]) =
            if let Some(ri) = s.recent_slot {
                (
                    &recent_pre[ri].html,
                    &recent_pre[ri].toc,
                    &recent_pre[ri].excerpt,
                )
            } else {
                let src_path = s.src_path;
                let dir = dir_of(src_path);
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
                    let ok = !rel.is_empty()
                        && resolve_rel_path(dir, rel, &mut rel_buf, &mut seg_ends);
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
                    let mut bmsg = Vec::with_capacity(16 + src_path.len() + target.len());
                    bmsg.extend_from_slice(b"broken-link ");
                    bmsg.extend_from_slice(src_path);
                    bmsg.extend_from_slice(b" -> ");
                    bmsg.extend_from_slice(target);
                    broken_links.insert_slice(&bmsg);
                    out.extend_from_slice(target);
                };
                if md_json_mode {
                    render_markdown_into::<true, _>(s.body, &mut link_fn, &mut md_scratch);
                } else {
                    render_markdown_into::<false, _>(s.body, &mut link_fn, &mut md_scratch);
                }
                if s.is_post {
                    post_excerpts[s.post_rank] = Some(md_scratch.excerpt_slice().to_vec());
                }
                (&md_scratch.html, &md_scratch.toc, md_scratch.excerpt_slice())
            };

        let rpage = RuntimePage {
            source_idx: si,
            slug: &s.slug,
            content: content_slice,
            toc: toc_slice,
            newer: s.newer,
            older: s.older,
        };
        let rctx = RenderCtx {
            md_json_mode,
            site_title,
            site_base_url: base,
            cards: &cards,
            global_tags: &global_tags,
            post_tag_cards: &post_tag_cards,
            recent_sis,
            posts_slice: &[],
            page: Some(&rpage),
            page_excerpt_override: Some(excerpt_slice),
            pager: None,
            tag: None,
        };

        key_buf.clear();
        key_buf.extend_from_slice(&s.out_dir);
        key_buf.extend_from_slice(b"/index.html");
        if has_index_static {
            if let Some(st_idx) = static_map.get(&key_buf) {
                static_shadowed[st_idx] = true;
            }
        }
        if !first_kv {
            out_json.push(b',');
        }
        first_kv = false;
        out_json.push(b'"');
        escape_json_into(&key_buf, out_json);
        out_json.write_slice(b"\":\"");
        render_compiled_ops(layout_ops, &rctx, &mut frames, &mut caches, out_json);
        out_json.push(b'"');
    }

    // Attach remaining post excerpts for `index` and `tag` cards.
    for (pr, &si) in posts.iter().enumerate() {
        if let Some(ref ex) = post_excerpts[pr] {
            cards[si].excerpt = ex.as_slice();
        }
    }

    // 10. Render paginated index pages directly into `out_json`.
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

        for k in 1..=pages {
            let start = (k - 1) * per;
            let end = (k * per).min(posts.len());
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
            let rpager = RuntimePager {
                number: k,
                total: pages,
                url: &u_cur,
                newer_url: &u_newer,
                older_url: &u_older,
                url_safe_hj: base_safe_hj,
                url_safe_j: base_safe_j,
            };
            let rctx = RenderCtx {
                md_json_mode,
                site_title,
                site_base_url: base,
                cards: &cards,
                global_tags: &global_tags,
                post_tag_cards: &post_tag_cards,
                recent_sis,
                posts_slice: &posts[start..end],
                page: None,
                page_excerpt_override: None,
                pager: Some(&rpager),
                tag: None,
            };

            key_buf.clear();
            if k == 1 {
                key_buf.extend_from_slice(b"index.html");
            } else {
                key_buf.extend_from_slice(b"page/");
                push_usize(k, &mut key_buf);
                key_buf.extend_from_slice(b"/index.html");
            }
            if has_index_static {
                if let Some(st_idx) = static_map.get(&key_buf) {
                    static_shadowed[st_idx] = true;
                }
            }
            if !first_kv {
                out_json.push(b',');
            }
            first_kv = false;
            out_json.push(b'"');
            out_json.write_slice(&key_buf);
            out_json.write_slice(b"\":\"");
            render_compiled_ops(idx_ops, &rctx, &mut frames, &mut caches, out_json);
            out_json.push(b'"');
        }
    }

    // 11. Render tag pages directly into `out_json`.
    if let Some(ref tag_ops) = compiled_tag {
        for &tid in &tag_order {
            let te = &tag_entries[tid];
            let rtag = RuntimeTagPage {
                name: te.name,
                name_hj: &te.name_hj,
                name_j: &te.name_j,
                slug: &te.slug,
                url: &te.url,
                url_hj: &te.url_hj,
                url_j: &te.url_j,
                count: te.post_sis.len(),
                post_sis: &te.post_sis,
            };
            let rctx = RenderCtx {
                md_json_mode,
                site_title,
                site_base_url: base,
                cards: &cards,
                global_tags: &global_tags,
                post_tag_cards: &post_tag_cards,
                recent_sis,
                posts_slice: &[],
                page: None,
                page_excerpt_override: None,
                pager: None,
                tag: Some(&rtag),
            };

            key_buf.clear();
            key_buf.extend_from_slice(b"tags/");
            key_buf.extend_from_slice(&te.slug);
            key_buf.extend_from_slice(b"/index.html");
            if has_index_static {
                if let Some(st_idx) = static_map.get(&key_buf) {
                    static_shadowed[st_idx] = true;
                }
            }
            if !first_kv {
                out_json.push(b',');
            }
            first_kv = false;
            out_json.push(b'"');
            out_json.write_slice(&key_buf);
            out_json.write_slice(b"\":\"");
            render_compiled_ops(tag_ops, &rctx, &mut frames, &mut caches, out_json);
            out_json.push(b'"');
        }
    }

    // 12. Render `tags/index.html` directly into `out_json`.
    if let Some(ref tags_ops) = compiled_tags {
        let rctx = RenderCtx {
            md_json_mode,
            site_title,
            site_base_url: base,
            cards: &cards,
            global_tags: &global_tags,
            post_tag_cards: &post_tag_cards,
            recent_sis,
            posts_slice: &[],
            page: None,
            page_excerpt_override: None,
            pager: None,
            tag: None,
        };
        if has_index_static {
            if let Some(st_idx) = static_map.get(b"tags/index.html") {
                static_shadowed[st_idx] = true;
            }
        }
        if !first_kv {
            out_json.push(b',');
        }
        first_kv = false;
        out_json.write_slice(b"\"tags/index.html\":\"");
        render_compiled_ops(tags_ops, &rctx, &mut frames, &mut caches, out_json);
        out_json.push(b'"');
    }

    // 13. Static files.
    for (idx, st) in statics.iter().enumerate() {
        if static_shadowed[idx] {
            let pstr = core::str::from_utf8(st.path).unwrap_or("");
            report_lines.push(format!("shadowed {}: a built page has its path", pstr));
        } else if st.path != b"build-report.txt" {
            emit_kv_raw(out_json, &mut first_kv, st.path, st.content);
        }
    }

    // 14. Report (`build-report.txt`).
    let mut all_lines: Vec<Vec<u8>> =
        Vec::with_capacity(report_lines.len() + broken_links.items.len());
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
