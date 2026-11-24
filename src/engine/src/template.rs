use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use crate::simd::find_byte;
use crate::text::{escape_html_into, escape_html_then_json_into, escape_json_into, has_js_ws, is_blank_ht_sp, push_usize, trim_js};

pub const INCLUDE_DEPTH: usize = 8;

#[derive(Clone, Debug)]
pub enum Node {
    Text(Vec<u8>),
    Var(Vec<Vec<u8>>),
    Raw(Vec<Vec<u8>>),
    If {
        not: bool,
        path: Vec<Vec<u8>>,
        then_branch: Vec<Node>,
        else_branch: Option<Vec<Node>>,
    },
    For {
        var_name: Vec<u8>,
        path: Vec<Vec<u8>>,
        body: Vec<Node>,
    },
    Include(Vec<u8>),
}

enum Tok<'a> {
    Text(Vec<u8>),
    Var(&'a [u8]),
    Raw(&'a [u8]),
    Tag(&'a [u8]),
}

fn json_quote(b: &[u8]) -> String {
    let mut v = Vec::with_capacity(b.len() + 4);
    v.push(b'"');
    escape_json_into(b, &mut v);
    v.push(b'"');
    unsafe { String::from_utf8_unchecked(v) }
}

fn is_valid_ident(b: &[u8]) -> bool {
    if b.is_empty() || !(b[0].is_ascii_alphabetic() || b[0] == b'_') {
        return false;
    }
    b[1..].iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_')
}

fn parse_path(tpl_name: &str, text: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    if text.is_empty() {
        return Err(format!("template \"{}\": {} is not a path", tpl_name, json_quote(text)));
    }
    let mut parts = Vec::with_capacity(3);
    for seg in text.split(|&c| c == b'.') {
        if !is_valid_ident(seg) {
            return Err(format!("template \"{}\": {} is not a path", tpl_name, json_quote(text)));
        }
        parts.push(seg.to_vec());
    }
    Ok(parts)
}

fn find_bytes(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from + needle.len() > hay.len() {
        return None;
    }
    let first = needle[0];
    let limit = hay.len() - needle.len();
    let mut i = from;
    while i <= limit {
        let j = find_byte(&hay[..limit + 1], i, first);
        if j > limit {
            return None;
        }
        if &hay[j..j + needle.len()] == needle {
            return Some(j);
        }
        i = j + 1;
    }
    None
}

fn tokenize<'a>(tpl_name: &str, src: &'a [u8]) -> Result<Vec<Tok<'a>>, String> {
    let mut tokens = Vec::with_capacity(32);
    let mut text = Vec::new();
    let mut i = 0usize;
    let len = src.len();
    while i < len {
        let open = find_byte(src, i, b'{');
        if open >= len || open + 1 >= len {
            text.extend_from_slice(&src[i..]);
            break;
        }
        let next = src[open + 1];
        if next != b'{' && next != b'%' {
            text.extend_from_slice(&src[i..open + 1]);
            i = open + 1;
            continue;
        }
        text.extend_from_slice(&src[i..open]);
        if next == b'{' {
            let raw = open + 2 < len && src[open + 2] == b'{';
            let pat = if raw { &b"}}}"[..] } else { &b"}}"[..] };
            let pat_len = pat.len();
            let close = find_bytes(src, pat, open + pat_len)
                .ok_or_else(|| format!("template \"{}\": a {} is not closed", tpl_name, if raw { "{{{" } else { "{{" }))?;
            if !text.is_empty() {
                tokens.push(Tok::Text(core::mem::take(&mut text)));
            }
            let inner = trim_js(&src[open + pat_len..close]);
            tokens.push(if raw { Tok::Raw(inner) } else { Tok::Var(inner) });
            i = close + pat_len;
            continue;
        }
        let close = find_bytes(src, b"%}", open + 2)
            .ok_or_else(|| format!("template \"{}\": a {{% is not closed", tpl_name))?;
        let end = close + 2;
        let line_start = match src[..open].iter().rposition(|&c| c == b'\n') {
            Some(nl_pos) => nl_pos + 1,
            None => 0,
        };
        let nl = find_byte(src, end, b'\n');
        let standalone = is_blank_ht_sp(&src[line_start..open]) && is_blank_ht_sp(&src[end..nl]);
        if standalone {
            let cut = open - line_start;
            let new_len = text.len().saturating_sub(cut);
            text.truncate(new_len);
        }
        if !text.is_empty() {
            tokens.push(Tok::Text(core::mem::take(&mut text)));
        }
        let tag = trim_js(&src[open + 2..close]);
        tokens.push(Tok::Tag(tag));
        i = if standalone {
            if nl < len { nl + 1 } else { len }
        } else {
            end
        };
    }
    if !text.is_empty() {
        tokens.push(Tok::Text(text));
    }
    Ok(tokens)
}

pub fn parse_template(name_bytes: &[u8], src: &[u8]) -> Result<Vec<Node>, String> {
    let name = core::str::from_utf8(name_bytes).unwrap_or("");
    let tokens = tokenize(name, src)?;
    enum Frame {
        Root(Vec<Node>),
        For { var_name: Vec<u8>, path: Vec<Vec<u8>>, body: Vec<Node> },
        If { not: bool, path: Vec<Vec<u8>>, then_b: Vec<Node>, else_b: Option<Vec<Node>> },
    }
    impl Frame {
        fn cur_body(&mut self) -> &mut Vec<Node> {
            match self {
                Frame::Root(b) => b,
                Frame::For { body, .. } => body,
                Frame::If { then_b, else_b, .. } => else_b.as_mut().unwrap_or(then_b),
            }
        }
    }
    let mut stack = Vec::with_capacity(8);
    stack.push(Frame::Root(Vec::with_capacity(16)));

    for tok in tokens {
        match tok {
            Tok::Text(t) => stack.last_mut().unwrap().cur_body().push(Node::Text(t)),
            Tok::Var(p) => {
                let path = parse_path(name, p)?;
                stack.last_mut().unwrap().cur_body().push(Node::Var(path));
            }
            Tok::Raw(p) => {
                let path = parse_path(name, p)?;
                stack.last_mut().unwrap().cur_body().push(Node::Raw(path));
            }
            Tok::Tag(tag) => {
                if let Some(rest) = tag.strip_prefix(b"for ") {
                    if let Some(in_pos) = find_bytes(rest, b" in ", 0) {
                        let var_name = &rest[..in_pos];
                        let path_slice = &rest[in_pos + 4..];
                        if is_valid_ident(var_name) && !path_slice.is_empty() && !has_js_ws(path_slice) {
                            let path = parse_path(name, path_slice)?;
                            stack.push(Frame::For {
                                var_name: var_name.to_vec(),
                                path,
                                body: Vec::new(),
                            });
                            continue;
                        }
                    }
                }
                if let Some(rest) = tag.strip_prefix(b"if ") {
                    let (not, path_slice) = if let Some(after_not) = rest.strip_prefix(b"not ") {
                        (true, after_not)
                    } else {
                        (false, rest)
                    };
                    if !path_slice.is_empty() && !has_js_ws(path_slice) {
                        let path = parse_path(name, path_slice)?;
                        stack.push(Frame::If {
                            not,
                            path,
                            then_b: Vec::new(),
                            else_b: None,
                        });
                        continue;
                    }
                }
                if tag == b"else" {
                    match stack.last_mut().unwrap() {
                        Frame::If { else_b, .. } if else_b.is_none() => {
                            *else_b = Some(Vec::new());
                            continue;
                        }
                        _ => return Err(format!("template \"{}\": an else outside an if", name)),
                    }
                }
                if tag == b"endfor" {
                    if matches!(stack.last().unwrap(), Frame::For { .. }) {
                        if let Some(Frame::For { var_name, path, body }) = stack.pop() {
                            stack.last_mut().unwrap().cur_body().push(Node::For { var_name, path, body });
                            continue;
                        }
                    }
                    return Err(format!("template \"{}\": an endfor with no open for", name));
                }
                if tag == b"endif" {
                    if matches!(stack.last().unwrap(), Frame::If { .. }) {
                        if let Some(Frame::If { not, path, then_b, else_b }) = stack.pop() {
                            stack.last_mut().unwrap().cur_body().push(Node::If {
                                not,
                                path,
                                then_branch: then_b,
                                else_branch: else_b,
                            });
                            continue;
                        }
                    }
                    return Err(format!("template \"{}\": an endif with no open if", name));
                }
                if let Some(rest) = tag.strip_prefix(b"include \"") {
                    if let Some(tpl) = rest.strip_suffix(b"\"") {
                        if !tpl.is_empty() && tpl.iter().all(|&c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'-')) {
                            stack.last_mut().unwrap().cur_body().push(Node::Include(tpl.to_vec()));
                            continue;
                        }
                    }
                }
                let tstr = core::str::from_utf8(tag).unwrap_or("");
                return Err(format!("template \"{}\": {{% {} %}} is not a tag", name, tstr));
            }
        }
    }

    if stack.len() > 1 {
        let kind = match stack.last().unwrap() {
            Frame::For { .. } => "for",
            Frame::If { .. } => "if",
            Frame::Root(_) => "",
        };
        return Err(format!("template \"{}\": a {} is not closed", name, kind));
    }

    match stack.pop().unwrap() {
        Frame::Root(b) => Ok(b),
        _ => unreachable!(),
    }
}

pub trait ValueSource {
    fn prop(&self, key: &[u8]) -> Option<Val<'_>>;
    fn truthy(&self) -> bool {
        true
    }
    fn write_text(&self, _json_mode: bool, _out: &mut Vec<u8>) {}
    fn write_html_text(&self, _json_mode: bool, _out: &mut Vec<u8>) {}
    fn post_id(&self) -> Option<usize> {
        None
    }
}

#[derive(Copy, Clone)]
pub enum Val<'a> {
    Str(&'a [u8]),
    Num(usize),
    Bool(bool),
    Null,
    Obj(&'a dyn ValueSource),
    Arr(&'a [&'a dyn ValueSource]),
}

impl<'a> Val<'a> {
    pub fn truthy(&self) -> bool {
        match *self {
            Val::Null => false,
            Val::Bool(b) => b,
            Val::Num(n) => n != 0,
            Val::Str(s) => !s.is_empty(),
            Val::Obj(o) => o.truthy(),
            Val::Arr(a) => !a.is_empty(),
        }
    }

    pub fn write_raw(&self, json_mode: bool, out: &mut Vec<u8>) {
        match *self {
            Val::Str(s) => {
                if json_mode {
                    escape_json_into(s, out);
                } else {
                    out.extend_from_slice(s);
                }
            }
            Val::Num(n) => push_usize(n, out),
            Val::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
            Val::Arr(items) => {
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.extend_from_slice(b", ");
                    }
                    item.write_text(json_mode, out);
                }
            }
            Val::Null | Val::Obj(_) => {}
        }
    }

    pub fn write_escaped(&self, json_mode: bool, out: &mut Vec<u8>) {
        match *self {
            Val::Str(s) => {
                if json_mode {
                    escape_html_then_json_into(s, out);
                } else {
                    escape_html_into(s, out);
                }
            }
            Val::Num(n) => push_usize(n, out),
            Val::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
            Val::Arr(items) => {
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.extend_from_slice(b", ");
                    }
                    item.write_html_text(json_mode, out);
                }
            }
            Val::Null | Val::Obj(_) => {}
        }
    }

    pub fn prop(&self, key: &[u8]) -> Option<Val<'a>> {
        match *self {
            Val::Obj(o) => o.prop(key),
            _ => None,
        }
    }
}

pub struct Scope<'a> {
    pub vars: &'a dyn ValueSource,
    pub parent: Option<&'a Scope<'a>>,
}

impl<'a> Scope<'a> {
    #[inline]
    pub fn lookup(&self, path: &[Vec<u8>]) -> Option<Val<'a>> {
        let first = path[0].as_slice();
        let mut s = Some(self);
        while let Some(cur) = s {
            if let Some(mut v) = cur.vars.prop(first) {
                for seg in &path[1..] {
                    v = v.prop(seg.as_slice())?;
                }
                return Some(v);
            }
            s = cur.parent;
        }
        None
    }
}

pub struct LoopVar {
    pub index: usize,
    pub first: bool,
    pub last: bool,
    pub length: usize,
}

impl ValueSource for LoopVar {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        match key {
            b"index" => Some(Val::Num(self.index)),
            b"first" => Some(Val::Bool(self.first)),
            b"last" => Some(Val::Bool(self.last)),
            b"length" => Some(Val::Num(self.length)),
            _ => None,
        }
    }
}

pub struct LoopScope<'a> {
    pub var_name: &'a [u8],
    pub elem: Val<'a>,
    pub lp: LoopVar,
}

impl<'a> ValueSource for LoopScope<'a> {
    #[inline]
    fn prop(&self, key: &[u8]) -> Option<Val<'_>> {
        if key == b"loop" {
            return Some(Val::Obj(&self.lp));
        }
        if key == self.var_name {
            return Some(self.elem);
        }
        None
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum Op {
    JsonText(Vec<u8>),
    Var(Vec<Vec<u8>>),
    Raw(Vec<Vec<u8>>),
    If {
        not: bool,
        path: Vec<Vec<u8>>,
        then_ops: Vec<Op>,
        else_ops: Option<Vec<Op>>,
    },
    For {
        var_name: Vec<u8>,
        path: Vec<Vec<u8>>,
        body: Vec<Op>,
        pure_item_loop: bool,
    },
}

fn push_json_text(out: &mut Vec<Op>, jt: Vec<u8>) {
    if jt.is_empty() {
        return;
    }
    if let Some(Op::JsonText(prev)) = out.last_mut() {
        prev.extend_from_slice(&jt);
    } else {
        out.push(Op::JsonText(jt));
    }
}

#[inline]
fn merge_op(out: &mut Vec<Op>, op: Op) {
    match op {
        Op::JsonText(jt) => push_json_text(out, jt),
        other => out.push(other),
    }
}

fn collect_free_vars(ops: &[Op], bound: &[&[u8]], out_free: &mut Vec<Vec<u8>>) {
    for op in ops {
        match op {
            Op::JsonText(_) => {}
            Op::Var(p) | Op::Raw(p) => {
                let root = p[0].as_slice();
                if !bound.iter().any(|&b| b == root) && !out_free.iter().any(|f| f.as_slice() == root) {
                    out_free.push(root.to_vec());
                }
            }
            Op::If { path, then_ops, else_ops, .. } => {
                let root = path[0].as_slice();
                if !bound.iter().any(|&b| b == root) && !out_free.iter().any(|f| f.as_slice() == root) {
                    out_free.push(root.to_vec());
                }
                collect_free_vars(then_ops, bound, out_free);
                if let Some(eo) = else_ops {
                    collect_free_vars(eo, bound, out_free);
                }
            }
            Op::For { var_name, path, body, .. } => {
                let root = path[0].as_slice();
                if !bound.iter().any(|&b| b == root) && !out_free.iter().any(|f| f.as_slice() == root) {
                    out_free.push(root.to_vec());
                }
                let mut inner = bound.to_vec();
                inner.push(var_name.as_slice());
                inner.push(b"loop");
                collect_free_vars(body, &inner, out_free);
            }
        }
    }
}

fn is_pure_item_loop(body: &[Op], var_name: &[u8]) -> bool {
    let mut free = Vec::new();
    let bound = [var_name];
    collect_free_vars(body, &bound, &mut free);
    free.iter().all(|v| matches!(v.as_slice(), b"site" | b"tags" | b"recent"))
}

pub fn inline_template<'a, F>(nodes: &[Node], get_tpl: &F, depth: usize, out: &mut Vec<Op>)
where
    F: Fn(&[u8]) -> Option<&'a [Node]>,
{
    for n in nodes {
        match n {
            Node::Text(t) => {
                if !t.is_empty() {
                    let mut jt = Vec::with_capacity(t.len());
                    escape_json_into(t, &mut jt);
                    push_json_text(out, jt);
                }
            }
            Node::Var(p) => out.push(Op::Var(p.clone())),
            Node::Raw(p) => out.push(Op::Raw(p.clone())),
            Node::If { not, path, then_branch, else_branch } => {
                let mut then_ops = Vec::new();
                inline_template(then_branch, get_tpl, depth, &mut then_ops);
                let else_ops = else_branch.as_ref().map(|eb| {
                    let mut eo = Vec::new();
                    inline_template(eb, get_tpl, depth, &mut eo);
                    eo
                });
                out.push(Op::If {
                    not: *not,
                    path: path.clone(),
                    then_ops,
                    else_ops,
                });
            }
            Node::For { var_name, path, body } => {
                let mut body_ops = Vec::new();
                inline_template(body, get_tpl, depth, &mut body_ops);
                let pure_item_loop = is_pure_item_loop(&body_ops, var_name);
                out.push(Op::For {
                    var_name: var_name.clone(),
                    path: path.clone(),
                    body: body_ops,
                    pure_item_loop,
                });
            }
            Node::Include(tpl_name) => {
                if depth < INCLUDE_DEPTH {
                    if let Some(sub) = get_tpl(tpl_name) {
                        inline_template(sub, get_tpl, depth + 1, out);
                    }
                }
            }
        }
    }
}

pub fn fold_site_ops(
    ops: &[Op],
    site_globals: &dyn ValueSource,
    has_page: bool,
    has_tag: bool,
    has_posts_pager: bool,
    shadowed: &[&[u8]],
) -> Vec<Op> {
    let mut out = Vec::with_capacity(ops.len());
    let is_const_var = |root: &[u8]| -> bool {
        if shadowed.iter().any(|&s| s == root) {
            return false;
        }
        match root {
            b"site" | b"tags" | b"recent" => true,
            b"page" => !has_page,
            b"tag" => !has_tag,
            b"posts" | b"pager" => !has_posts_pager,
            _ => true,
        }
    };

    for op in ops {
        match op {
            Op::JsonText(t) => push_json_text(&mut out, t.clone()),
            Op::Var(p) => {
                if is_const_var(&p[0]) {
                    let mut rendered = Vec::new();
                    let scope = Scope { vars: site_globals, parent: None };
                    if let Some(v) = scope.lookup(p) {
                        v.write_escaped(true, &mut rendered);
                    }
                    push_json_text(&mut out, rendered);
                } else {
                    out.push(op.clone());
                }
            }
            Op::Raw(p) => {
                if is_const_var(&p[0]) {
                    let mut rendered = Vec::new();
                    let scope = Scope { vars: site_globals, parent: None };
                    if let Some(v) = scope.lookup(p) {
                        v.write_raw(true, &mut rendered);
                    }
                    push_json_text(&mut out, rendered);
                } else {
                    out.push(op.clone());
                }
            }
            Op::If { not, path, then_ops, else_ops } => {
                if is_const_var(&path[0]) {
                    let scope = Scope { vars: site_globals, parent: None };
                    let tv = scope.lookup(path).map(|v| v.truthy()).unwrap_or(false);
                    if tv != *not {
                        for fop in fold_site_ops(then_ops, site_globals, has_page, has_tag, has_posts_pager, shadowed) {
                            merge_op(&mut out, fop);
                        }
                    } else if let Some(eo) = else_ops {
                        for fop in fold_site_ops(eo, site_globals, has_page, has_tag, has_posts_pager, shadowed) {
                            merge_op(&mut out, fop);
                        }
                    }
                    continue;
                }
                if path.len() == 1
                    && !shadowed.iter().any(|&s| s == path[0].as_slice())
                    && ((path[0].as_slice() == b"page" && has_page)
                        || (path[0].as_slice() == b"tag" && has_tag)
                        || (path[0].as_slice() == b"pager" && has_posts_pager))
                {
                    if !*not {
                        for fop in fold_site_ops(then_ops, site_globals, has_page, has_tag, has_posts_pager, shadowed) {
                            merge_op(&mut out, fop);
                        }
                    } else if let Some(eo) = else_ops {
                        for fop in fold_site_ops(eo, site_globals, has_page, has_tag, has_posts_pager, shadowed) {
                            merge_op(&mut out, fop);
                        }
                    }
                    continue;
                }
                let f_then = fold_site_ops(then_ops, site_globals, has_page, has_tag, has_posts_pager, shadowed);
                let f_else = else_ops.as_ref().map(|eo| fold_site_ops(eo, site_globals, has_page, has_tag, has_posts_pager, shadowed));
                out.push(Op::If {
                    not: *not,
                    path: path.clone(),
                    then_ops: f_then,
                    else_ops: f_else,
                });
            }
            Op::For { var_name, path, body, .. } => {
                let mut bound = shadowed.to_vec();
                bound.push(var_name.as_slice());
                bound.push(b"loop");
                let mut free = Vec::new();
                collect_free_vars(body, &bound, &mut free);
                if is_const_var(&path[0]) && free.iter().all(|r| is_const_var(r)) {
                    let mut rendered = Vec::with_capacity(512);
                    let scope = Scope { vars: site_globals, parent: None };
                    let single_op = [Op::For {
                        var_name: var_name.clone(),
                        path: path.clone(),
                        body: body.clone(),
                        pure_item_loop: false,
                    }];
                    render_ops(&single_op, &scope, &mut None, &mut rendered);
                    push_json_text(&mut out, rendered);
                } else {
                    let f_body = fold_site_ops(body, site_globals, has_page, has_tag, has_posts_pager, &bound);
                    let pure = shadowed.is_empty() && is_pure_item_loop(&f_body, var_name);
                    out.push(Op::For {
                        var_name: var_name.clone(),
                        path: path.clone(),
                        body: f_body,
                        pure_item_loop: pure,
                    });
                }
            }
        }
    }
    out
}

pub struct CardCache<'a> {
    pub body_pattern: &'a [Op],
    pub cached: Vec<Option<Vec<u8>>>,
}

pub fn render_ops(
    ops: &[Op],
    scope: &Scope<'_>,
    card_cache: &mut Option<CardCache<'_>>,
    out: &mut Vec<u8>,
) {
    for op in ops {
        match op {
            Op::JsonText(jt) => out.extend_from_slice(jt),
            Op::Var(p) => {
                if let Some(v) = scope.lookup(p) {
                    v.write_escaped(true, out);
                }
            }
            Op::Raw(p) => {
                if let Some(v) = scope.lookup(p) {
                    v.write_raw(true, out);
                }
            }
            Op::If { not, path, then_ops, else_ops } => {
                let tv = scope.lookup(path).map(|v| v.truthy()).unwrap_or(false);
                if tv != *not {
                    render_ops(then_ops, scope, card_cache, out);
                } else if let Some(eo) = else_ops {
                    render_ops(eo, scope, card_cache, out);
                }
            }
            Op::For { var_name, path, body, pure_item_loop } => {
                if let Some(Val::Arr(xs)) = scope.lookup(path) {
                    let len = xs.len();
                    let can_cache = *pure_item_loop
                        && card_cache
                            .as_ref()
                            .map(|cc| cc.body_pattern == body.as_slice())
                            .unwrap_or(false);
                    for (k, &elem) in xs.iter().enumerate() {
                        if can_cache {
                            if let Some(pid) = elem.post_id() {
                                if let Some(cc) = card_cache.as_mut() {
                                    if let Some(hit) = cc.cached[pid].as_ref() {
                                        out.extend_from_slice(hit);
                                        continue;
                                    }
                                    let ls = LoopScope {
                                        var_name,
                                        elem: Val::Obj(elem),
                                        lp: LoopVar {
                                            index: k + 1,
                                            first: k == 0,
                                            last: k + 1 == len,
                                            length: len,
                                        },
                                    };
                                    let child = Scope { vars: &ls, parent: Some(scope) };
                                    let mut buf = Vec::with_capacity(512);
                                    let mut none_cc = None;
                                    render_ops(body, &child, &mut none_cc, &mut buf);
                                    out.extend_from_slice(&buf);
                                    cc.cached[pid] = Some(buf);
                                    continue;
                                }
                            }
                        }
                        let ls = LoopScope {
                            var_name,
                            elem: Val::Obj(elem),
                            lp: LoopVar {
                                index: k + 1,
                                first: k == 0,
                                last: k + 1 == len,
                                length: len,
                            },
                        };
                        let child = Scope { vars: &ls, parent: Some(scope) };
                        render_ops(body, &child, card_cache, out);
                    }
                }
            }
        }
    }
}
