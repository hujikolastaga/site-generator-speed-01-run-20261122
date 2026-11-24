use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use crate::markdown::TocItem;
use crate::out::ByteSink;
use crate::simd::find_byte;
use crate::text::{
    escape_html_into, escape_html_then_json_into, escape_json_into, has_js_ws, is_blank_ht_sp,
    push_usize, trim_js,
};

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

// ---------------------------------------------------------------- Folding Phase (Site Constants)

pub trait ValueSource {
    fn prop(&self, key: &[u8]) -> Option<Val<'_>>;
    fn truthy(&self) -> bool {
        true
    }
    fn write_text(&self, _json_mode: bool, _out: &mut Vec<u8>) {}
    fn write_html_text(&self, _json_mode: bool, _out: &mut Vec<u8>) {}
}

#[derive(Copy, Clone)]
pub enum Val<'a> {
    Str(&'a [u8]),
    PreJsonStr(&'a [u8]),
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
            Val::Str(s) | Val::PreJsonStr(s) => !s.is_empty(),
            Val::Obj(o) => o.truthy(),
            Val::Arr(a) => !a.is_empty(),
        }
    }

    pub fn write_raw(&self, json_mode: bool, out: &mut Vec<u8>) {
        match *self {
            Val::PreJsonStr(s) => out.extend_from_slice(s),
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
            Val::PreJsonStr(s) => out.extend_from_slice(s),
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
            Op::For { var_name, path, body } => {
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
                out.push(Op::For {
                    var_name: var_name.clone(),
                    path: path.clone(),
                    body: body_ops,
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

/// Check if an inlined `Op` sequence ever uses HTML-escaped `{{ *.content }}`, `{{ *.excerpt }}`, or `{{ *.html }}`.
pub fn ops_use_escaped_md_html(ops: &[Op]) -> bool {
    for op in ops {
        match op {
            Op::Var(p) => {
                if let Some(last) = p.last() {
                    if matches!(last.as_slice(), b"content" | b"excerpt" | b"html") {
                        return true;
                    }
                }
            }
            Op::If { then_ops, else_ops, .. } => {
                if ops_use_escaped_md_html(then_ops) {
                    return true;
                }
                if let Some(eo) = else_ops {
                    if ops_use_escaped_md_html(eo) {
                        return true;
                    }
                }
            }
            Op::For { body, .. } => {
                if ops_use_escaped_md_html(body) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn render_fold_ops(ops: &[Op], scope: &Scope<'_>, out: &mut Vec<u8>) {
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
                    render_fold_ops(then_ops, scope, out);
                } else if let Some(eo) = else_ops {
                    render_fold_ops(eo, scope, out);
                }
            }
            Op::For { var_name, path, body } => {
                if let Some(Val::Arr(xs)) = scope.lookup(path) {
                    let len = xs.len();
                    for (k, &elem) in xs.iter().enumerate() {
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
                        render_fold_ops(body, &child, out);
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
            Op::For { var_name, path, body } => {
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
                    }];
                    render_fold_ops(&single_op, &scope, &mut rendered);
                    push_json_text(&mut out, rendered);
                } else {
                    let f_body = fold_site_ops(body, site_globals, has_page, has_tag, has_posts_pager, &bound);
                    out.push(Op::For {
                        var_name: var_name.clone(),
                        path: path.clone(),
                        body: f_body,
                    });
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- Compiled Typed IR

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum CardField {
    Title,
    Url,
    Date,
    DateLong,
    Summary,
    Excerpt,
    ReadingMinutes,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum TagField {
    Name,
    Slug,
    Url,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum LinkField {
    Title,
    Url,
    Date,
    DateLong,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum LoopField {
    Index,
    First,
    Last,
    Length,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum FastExpr {
    PageTitle,
    PageUrl,
    PageSlug,
    PageDate,
    PageDateLong,
    PageSummary,
    PageExcerpt,
    PageReadingMinutes,
    PageContent,
    PageIsPost,
    PageTags,
    PageToc,
    PageNewerExists,
    PageNewerField(LinkField),
    PageOlderExists,
    PageOlderField(LinkField),

    PagerExists,
    PagerNumber,
    PagerTotal,
    PagerUrl,
    PagerNewerUrl,
    PagerOlderUrl,
    Posts,

    TagExists,
    TagName,
    TagSlug,
    TagUrl,
    TagCount,
    TagPosts,

    SiteTitle,
    SiteBaseUrl,
    GlobalTags,
    GlobalRecent,

    FrameCard(u8, CardField),
    FrameCardTags(u8),
    FrameTag(u8, TagField),
    FrameTagSummaryField(u8, TagField),
    FrameTagSummaryCount(u8),
    FrameTocLevel(u8),
    FrameTocId(u8),
    FrameTocHtml(u8),
    FrameLoop(u8, LoopField),
    OpaqueObj,
    AlwaysNull,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum ArrExpr {
    Posts,
    TagPosts,
    PageTags,
    PageToc,
    FrameCardTags(u8),
    GlobalTags,
    GlobalRecent,
    Empty,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum VarKind {
    Site,
    Page,
    Pager,
    TagFull,
    Link(bool), // true = newer, false = older
    Card,
    CardTag,
    TagSummary,
    Toc,
    ArrPosts,
    ArrTagPosts,
    ArrPageTags,
    ArrPageToc,
    ArrFrameCardTags(u8),
    ArrGlobalTags,
    ArrGlobalRecent,
}

#[derive(Clone, PartialEq, Eq)]
pub enum CompiledOp {
    JsonText(Box<[u8]>),
    Var(FastExpr),
    Raw(FastExpr),
    If {
        not: bool,
        expr: FastExpr,
        then_ops: Vec<CompiledOp>,
        else_ops: Option<Vec<CompiledOp>>,
    },
    For {
        arr: ArrExpr,
        body: Vec<CompiledOp>,
        cache_slot: Option<usize>,
    },
}

#[derive(Copy, Clone)]
enum CompKind {
    Kind(VarKind, Option<u8>),
    Expr(FastExpr),
}

fn resolve_expr(
    path: &[Vec<u8>],
    has_page: bool,
    has_tag: bool,
    has_posts_pager: bool,
    loop_env: &[(&[u8], VarKind)],
) -> (FastExpr, ArrExpr) {
    let seg0 = path[0].as_slice();
    let mut cur: Option<CompKind> = None;

    for idx in (0..loop_env.len()).rev() {
        if seg0 == b"loop" {
            let slot = loop_env.len() - 1;
            if path.len() == 1 {
                return (FastExpr::OpaqueObj, ArrExpr::Empty);
            }
            if path.len() == 2 {
                let expr = match path[1].as_slice() {
                    b"index" => FastExpr::FrameLoop(slot as u8, LoopField::Index),
                    b"first" => FastExpr::FrameLoop(slot as u8, LoopField::First),
                    b"last" => FastExpr::FrameLoop(slot as u8, LoopField::Last),
                    b"length" => FastExpr::FrameLoop(slot as u8, LoopField::Length),
                    _ => FastExpr::AlwaysNull,
                };
                return (expr, ArrExpr::Empty);
            }
            return (FastExpr::AlwaysNull, ArrExpr::Empty);
        }
        if loop_env[idx].0 == seg0 {
            cur = Some(CompKind::Kind(loop_env[idx].1, Some(idx as u8)));
            break;
        }
    }

    if cur.is_none() {
        cur = match seg0 {
            b"page" if has_page => Some(CompKind::Kind(VarKind::Page, None)),
            b"posts" if has_posts_pager => Some(CompKind::Kind(VarKind::ArrPosts, None)),
            b"pager" if has_posts_pager => Some(CompKind::Kind(VarKind::Pager, None)),
            b"tag" if has_tag => Some(CompKind::Kind(VarKind::TagFull, None)),
            b"site" => Some(CompKind::Kind(VarKind::Site, None)),
            b"tags" => Some(CompKind::Kind(VarKind::ArrGlobalTags, None)),
            b"recent" => Some(CompKind::Kind(VarKind::ArrGlobalRecent, None)),
            _ => None,
        };
    }

    let Some(mut state) = cur else {
        return (FastExpr::AlwaysNull, ArrExpr::Empty);
    };

    for seg in &path[1..] {
        let s = seg.as_slice();
        state = match state {
            CompKind::Kind(VarKind::Site, _) => match s {
                b"title" => CompKind::Expr(FastExpr::SiteTitle),
                b"base_url" => CompKind::Expr(FastExpr::SiteBaseUrl),
                _ => return (FastExpr::AlwaysNull, ArrExpr::Empty),
            },
            CompKind::Kind(VarKind::Page, _) => match s {
                b"title" => CompKind::Expr(FastExpr::PageTitle),
                b"url" => CompKind::Expr(FastExpr::PageUrl),
                b"slug" => CompKind::Expr(FastExpr::PageSlug),
                b"date" => CompKind::Expr(FastExpr::PageDate),
                b"date_long" => CompKind::Expr(FastExpr::PageDateLong),
                b"summary" => CompKind::Expr(FastExpr::PageSummary),
                b"excerpt" => CompKind::Expr(FastExpr::PageExcerpt),
                b"reading_minutes" => CompKind::Expr(FastExpr::PageReadingMinutes),
                b"content" => CompKind::Expr(FastExpr::PageContent),
                b"is_post" => CompKind::Expr(FastExpr::PageIsPost),
                b"tags" => CompKind::Kind(VarKind::ArrPageTags, None),
                b"toc" => CompKind::Kind(VarKind::ArrPageToc, None),
                b"newer" => CompKind::Kind(VarKind::Link(true), None),
                b"older" => CompKind::Kind(VarKind::Link(false), None),
                _ => return (FastExpr::AlwaysNull, ArrExpr::Empty),
            },
            CompKind::Kind(VarKind::Link(is_newer), _) => {
                let lf = match s {
                    b"title" => LinkField::Title,
                    b"url" => LinkField::Url,
                    b"date" => LinkField::Date,
                    b"date_long" => LinkField::DateLong,
                    _ => return (FastExpr::AlwaysNull, ArrExpr::Empty),
                };
                CompKind::Expr(if is_newer {
                    FastExpr::PageNewerField(lf)
                } else {
                    FastExpr::PageOlderField(lf)
                })
            }
            CompKind::Kind(VarKind::Pager, _) => match s {
                b"number" => CompKind::Expr(FastExpr::PagerNumber),
                b"total" => CompKind::Expr(FastExpr::PagerTotal),
                b"url" => CompKind::Expr(FastExpr::PagerUrl),
                b"newer_url" => CompKind::Expr(FastExpr::PagerNewerUrl),
                b"older_url" => CompKind::Expr(FastExpr::PagerOlderUrl),
                _ => return (FastExpr::AlwaysNull, ArrExpr::Empty),
            },
            CompKind::Kind(VarKind::TagFull, _) => match s {
                b"name" => CompKind::Expr(FastExpr::TagName),
                b"slug" => CompKind::Expr(FastExpr::TagSlug),
                b"url" => CompKind::Expr(FastExpr::TagUrl),
                b"count" => CompKind::Expr(FastExpr::TagCount),
                b"posts" => CompKind::Kind(VarKind::ArrTagPosts, None),
                _ => return (FastExpr::AlwaysNull, ArrExpr::Empty),
            },
            CompKind::Kind(VarKind::Card, Some(slot)) => match s {
                b"title" => CompKind::Expr(FastExpr::FrameCard(slot, CardField::Title)),
                b"url" => CompKind::Expr(FastExpr::FrameCard(slot, CardField::Url)),
                b"date" => CompKind::Expr(FastExpr::FrameCard(slot, CardField::Date)),
                b"date_long" => CompKind::Expr(FastExpr::FrameCard(slot, CardField::DateLong)),
                b"summary" => CompKind::Expr(FastExpr::FrameCard(slot, CardField::Summary)),
                b"excerpt" => CompKind::Expr(FastExpr::FrameCard(slot, CardField::Excerpt)),
                b"reading_minutes" => CompKind::Expr(FastExpr::FrameCard(slot, CardField::ReadingMinutes)),
                b"tags" => CompKind::Kind(VarKind::ArrFrameCardTags(slot), Some(slot)),
                _ => return (FastExpr::AlwaysNull, ArrExpr::Empty),
            },
            CompKind::Kind(VarKind::CardTag, Some(slot)) => match s {
                b"name" => CompKind::Expr(FastExpr::FrameTag(slot, TagField::Name)),
                b"slug" => CompKind::Expr(FastExpr::FrameTag(slot, TagField::Slug)),
                b"url" => CompKind::Expr(FastExpr::FrameTag(slot, TagField::Url)),
                _ => return (FastExpr::AlwaysNull, ArrExpr::Empty),
            },
            CompKind::Kind(VarKind::TagSummary, Some(slot)) => match s {
                b"name" => CompKind::Expr(FastExpr::FrameTagSummaryField(slot, TagField::Name)),
                b"slug" => CompKind::Expr(FastExpr::FrameTagSummaryField(slot, TagField::Slug)),
                b"url" => CompKind::Expr(FastExpr::FrameTagSummaryField(slot, TagField::Url)),
                b"count" => CompKind::Expr(FastExpr::FrameTagSummaryCount(slot)),
                _ => return (FastExpr::AlwaysNull, ArrExpr::Empty),
            },
            CompKind::Kind(VarKind::Toc, Some(slot)) => match s {
                b"level" => CompKind::Expr(FastExpr::FrameTocLevel(slot)),
                b"id" => CompKind::Expr(FastExpr::FrameTocId(slot)),
                b"html" => CompKind::Expr(FastExpr::FrameTocHtml(slot)),
                _ => return (FastExpr::AlwaysNull, ArrExpr::Empty),
            },
            _ => return (FastExpr::AlwaysNull, ArrExpr::Empty),
        };
    }

    match state {
        CompKind::Expr(e) => (e, ArrExpr::Empty),
        CompKind::Kind(VarKind::Page, _) => (FastExpr::OpaqueObj, ArrExpr::Empty),
        CompKind::Kind(VarKind::Pager, _) => (FastExpr::PagerExists, ArrExpr::Empty),
        CompKind::Kind(VarKind::TagFull, _) => (FastExpr::TagExists, ArrExpr::Empty),
        CompKind::Kind(VarKind::Link(true), _) => (FastExpr::PageNewerExists, ArrExpr::Empty),
        CompKind::Kind(VarKind::Link(false), _) => (FastExpr::PageOlderExists, ArrExpr::Empty),
        CompKind::Kind(VarKind::ArrPosts, _) => (FastExpr::Posts, ArrExpr::Posts),
        CompKind::Kind(VarKind::ArrTagPosts, _) => (FastExpr::TagPosts, ArrExpr::TagPosts),
        CompKind::Kind(VarKind::ArrPageTags, _) => (FastExpr::PageTags, ArrExpr::PageTags),
        CompKind::Kind(VarKind::ArrPageToc, _) => (FastExpr::PageToc, ArrExpr::PageToc),
        CompKind::Kind(VarKind::ArrFrameCardTags(slot), _) => {
            (FastExpr::FrameCardTags(slot), ArrExpr::FrameCardTags(slot))
        }
        CompKind::Kind(VarKind::ArrGlobalTags, _) => (FastExpr::GlobalTags, ArrExpr::GlobalTags),
        CompKind::Kind(VarKind::ArrGlobalRecent, _) => (FastExpr::GlobalRecent, ArrExpr::GlobalRecent),
        CompKind::Kind(VarKind::Card | VarKind::CardTag | VarKind::TagSummary | VarKind::Toc | VarKind::Site, _) => {
            (FastExpr::OpaqueObj, ArrExpr::Empty)
        }
    }
}

fn body_is_pure_card(ops: &[CompiledOp], item_slot: u8) -> bool {
    for op in ops {
        match op {
            CompiledOp::JsonText(_) => {}
            CompiledOp::Var(e) | CompiledOp::Raw(e) => {
                if !expr_pure_for_slot(*e, item_slot) {
                    return false;
                }
            }
            CompiledOp::If { expr, then_ops, else_ops, .. } => {
                if !expr_pure_for_slot(*expr, item_slot) || !body_is_pure_card(then_ops, item_slot) {
                    return false;
                }
                if let Some(eo) = else_ops {
                    if !body_is_pure_card(eo, item_slot) {
                        return false;
                    }
                }
            }
            CompiledOp::For { arr, body, .. } => {
                let arr_ok = match *arr {
                    ArrExpr::FrameCardTags(s) => s == item_slot,
                    ArrExpr::GlobalTags | ArrExpr::GlobalRecent | ArrExpr::Empty => true,
                    _ => false,
                };
                if !arr_ok || !body_is_pure_card(body, item_slot) {
                    return false;
                }
            }
        }
    }
    true
}

#[inline]
fn expr_pure_for_slot(e: FastExpr, item_slot: u8) -> bool {
    match e {
        FastExpr::SiteTitle
        | FastExpr::SiteBaseUrl
        | FastExpr::GlobalTags
        | FastExpr::GlobalRecent
        | FastExpr::OpaqueObj
        | FastExpr::AlwaysNull => true,
        FastExpr::FrameCard(s, _) | FastExpr::FrameCardTags(s) => s == item_slot,
        FastExpr::FrameTag(s, _)
        | FastExpr::FrameTagSummaryField(s, _)
        | FastExpr::FrameTagSummaryCount(s)
        | FastExpr::FrameTocLevel(s)
        | FastExpr::FrameTocId(s)
        | FastExpr::FrameTocHtml(s)
        | FastExpr::FrameLoop(s, _) => s > item_slot,
        _ => false,
    }
}

fn compile_inner<'a>(
    ops: &'a [Op],
    has_page: bool,
    has_tag: bool,
    has_posts_pager: bool,
    loop_env: &mut Vec<(&'a [u8], VarKind)>,
    card_patterns: &mut Vec<Vec<CompiledOp>>,
) -> Vec<CompiledOp> {
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            Op::JsonText(jt) => {
                if let Some(CompiledOp::JsonText(prev)) = out.last_mut() {
                    let mut joined = Vec::with_capacity(prev.len() + jt.len());
                    joined.extend_from_slice(prev);
                    joined.extend_from_slice(jt);
                    *prev = joined.into_boxed_slice();
                } else {
                    out.push(CompiledOp::JsonText(jt.clone().into_boxed_slice()));
                }
            }
            Op::Var(p) => {
                let (expr, _) = resolve_expr(p, has_page, has_tag, has_posts_pager, loop_env);
                out.push(CompiledOp::Var(expr));
            }
            Op::Raw(p) => {
                let (expr, _) = resolve_expr(p, has_page, has_tag, has_posts_pager, loop_env);
                out.push(CompiledOp::Raw(expr));
            }
            Op::If { not, path, then_ops, else_ops } => {
                let (expr, _) = resolve_expr(path, has_page, has_tag, has_posts_pager, loop_env);
                let ct = compile_inner(then_ops, has_page, has_tag, has_posts_pager, loop_env, card_patterns);
                let ce = else_ops
                    .as_ref()
                    .map(|eo| compile_inner(eo, has_page, has_tag, has_posts_pager, loop_env, card_patterns));
                out.push(CompiledOp::If {
                    not: *not,
                    expr,
                    then_ops: ct,
                    else_ops: ce,
                });
            }
            Op::For { var_name, path, body } => {
                let (_, arr) = resolve_expr(path, has_page, has_tag, has_posts_pager, loop_env);
                let elem_kind = match arr {
                    ArrExpr::Posts | ArrExpr::TagPosts | ArrExpr::GlobalRecent => VarKind::Card,
                    ArrExpr::PageTags | ArrExpr::FrameCardTags(_) => VarKind::CardTag,
                    ArrExpr::GlobalTags => VarKind::TagSummary,
                    ArrExpr::PageToc => VarKind::Toc,
                    ArrExpr::Empty => {
                        continue;
                    }
                };
                let slot = loop_env.len();
                loop_env.push((var_name.as_slice(), elem_kind));
                let cbody = compile_inner(body, has_page, has_tag, has_posts_pager, loop_env, card_patterns);
                loop_env.pop();

                let cache_slot = if matches!(arr, ArrExpr::Posts | ArrExpr::TagPosts | ArrExpr::GlobalRecent)
                    && slot == 0
                    && body_is_pure_card(&cbody, slot as u8)
                {
                    let existing = card_patterns.iter().position(|p| p == &cbody);
                    let cid = match existing {
                        Some(id) => id,
                        None => {
                            let id = card_patterns.len();
                            card_patterns.push(cbody.clone());
                            id
                        }
                    };
                    Some(cid)
                } else {
                    None
                };

                out.push(CompiledOp::For {
                    arr,
                    body: cbody,
                    cache_slot,
                });
            }
        }
    }
    out
}

pub fn compile_ops(
    ops: &[Op],
    has_page: bool,
    has_tag: bool,
    has_posts_pager: bool,
    card_patterns: &mut Vec<Vec<CompiledOp>>,
) -> Vec<CompiledOp> {
    let mut env = Vec::with_capacity(6);
    compile_inner(ops, has_page, has_tag, has_posts_pager, &mut env, card_patterns)
}

// ---------------------------------------------------------------- Runtime Context & Execution

pub struct SafeStr<'a> {
    pub raw: &'a [u8],
    pub safe_hj: bool,
    pub safe_j: bool,
}

impl<'a> SafeStr<'a> {
    #[inline(always)]
    pub fn emit_var<S: ByteSink>(&self, out: &mut S) {
        if self.safe_hj {
            out.write_slice(self.raw);
        } else {
            escape_html_then_json_into(self.raw, out);
        }
    }

    #[inline(always)]
    pub fn emit_raw<S: ByteSink>(&self, out: &mut S) {
        if self.safe_j {
            out.write_slice(self.raw);
        } else {
            escape_json_into(self.raw, out);
        }
    }
}

pub struct RuntimeCard<'a> {
    pub post_rank: usize,
    pub title: &'a [u8],
    pub title_safe_hj: bool,
    pub title_safe_j: bool,
    pub url: &'a [u8],
    pub url_safe_hj: bool,
    pub url_safe_j: bool,
    pub date: &'a [u8],
    pub date_long: &'a [u8],
    pub summary: &'a [u8],
    pub summary_safe_hj: bool,
    pub summary_safe_j: bool,
    pub excerpt: &'a [u8],
    pub reading_minutes: usize,
    pub tag_ids: &'a [usize], // post -> indices in `tag_entries`
    pub own_tags: &'a [RuntimeCardTag<'a>],
    pub is_post: bool,
}

pub struct RuntimeCardTag<'a> {
    pub name: &'a [u8],
    pub name_hj: &'a [u8],
    pub name_j: &'a [u8],
    pub slug: &'a [u8],
    pub url: &'a [u8],
    pub url_hj: &'a [u8],
    pub url_j: &'a [u8],
}

pub struct RuntimeTagSummary<'a> {
    pub name: &'a [u8],
    pub name_hj: &'a [u8],
    pub name_j: &'a [u8],
    pub slug: &'a [u8],
    pub url: &'a [u8],
    pub url_hj: &'a [u8],
    pub url_j: &'a [u8],
    pub count: usize,
}

pub struct RuntimePage<'a> {
    pub source_idx: usize,
    pub slug: &'a [u8],
    pub content: &'a [u8],
    pub toc: &'a [TocItem],
    pub newer: Option<usize>,
    pub older: Option<usize>,
}

pub struct RuntimePager<'a> {
    pub number: usize,
    pub total: usize,
    pub url: &'a [u8],
    pub newer_url: &'a [u8],
    pub older_url: &'a [u8],
    pub url_safe_hj: bool,
    pub url_safe_j: bool,
}

pub struct RuntimeTagPage<'a> {
    pub name: &'a [u8],
    pub name_hj: &'a [u8],
    pub name_j: &'a [u8],
    pub slug: &'a [u8],
    pub url: &'a [u8],
    pub url_hj: &'a [u8],
    pub url_j: &'a [u8],
    pub count: usize,
    pub post_sis: &'a [usize],
}

pub struct RenderCtx<'a> {
    pub md_json_mode: bool,
    pub site_title: &'a [u8],
    pub site_base_url: &'a [u8],
    pub cards: &'a [RuntimeCard<'a>],
    pub global_tags: &'a [RuntimeTagSummary<'a>],
    pub post_tag_cards: &'a [RuntimeCardTag<'a>],
    pub recent_sis: &'a [usize],
    pub posts_slice: &'a [usize],
    pub page: Option<&'a RuntimePage<'a>>,
    pub page_excerpt_override: Option<&'a [u8]>,
    pub pager: Option<&'a RuntimePager<'a>>,
    pub tag: Option<&'a RuntimeTagPage<'a>>,
}

#[derive(Copy, Clone)]
pub enum FrameRef {
    Card(usize),              // index into ctx.cards
    PostTag { si: usize, idx: usize }, // tag idx for source `si` (card or page)
    TagSummary(usize),        // index into ctx.global_tags
    Toc(usize),               // index into ctx.page.unwrap().toc
}

#[derive(Copy, Clone)]
pub struct LoopFrame {
    pub item: FrameRef,
    pub index: usize,
    pub length: usize,
    pub first: bool,
    pub last: bool,
}

#[inline(always)]
fn get_frame_tag<'a>(ctx: &'a RenderCtx<'a>, si: usize, idx: usize) -> &'a RuntimeCardTag<'a> {
    let c = &ctx.cards[si];
    if c.is_post {
        &ctx.post_tag_cards[c.tag_ids[idx]]
    } else {
        &c.own_tags[idx]
    }
}

#[inline(always)]
fn write_str_mode<S: ByteSink>(s: &[u8], safe_hj: bool, safe_j: bool, html_escape: bool, out: &mut S) {
    if html_escape {
        if safe_hj {
            out.write_slice(s);
        } else {
            escape_html_then_json_into(s, out);
        }
    } else if safe_j {
        out.write_slice(s);
    } else {
        escape_json_into(s, out);
    }
}

#[inline(always)]
fn write_md_html_mode<S: ByteSink>(s: &[u8], md_json_mode: bool, html_escape: bool, out: &mut S) {
    if md_json_mode {
        // Already pre-JSON formatted; html_escape is guaranteed false whenever md_json_mode is true.
        out.write_slice(s);
    } else if html_escape {
        escape_html_then_json_into(s, out);
    } else {
        escape_json_into(s, out);
    }
}

#[inline(always)]
fn write_comma_sep<S: ByteSink>(len: usize, out: &mut S) {
    for k in 1..len {
        let _ = k;
        out.write_slice(b", ");
    }
}

#[inline]
fn link_field_write<S: ByteSink>(
    card: &RuntimeCard<'_>,
    f: LinkField,
    html_escape: bool,
    out: &mut S,
) {
    match f {
        LinkField::Title => write_str_mode(card.title, card.title_safe_hj, card.title_safe_j, html_escape, out),
        LinkField::Url => write_str_mode(card.url, card.url_safe_hj, card.url_safe_j, html_escape, out),
        LinkField::Date => out.write_slice(card.date),
        LinkField::DateLong => out.write_slice(card.date_long),
    }
}

#[inline]
fn card_field_write<S: ByteSink>(
    card: &RuntimeCard<'_>,
    f: CardField,
    md_json_mode: bool,
    html_escape: bool,
    out: &mut S,
) {
    match f {
        CardField::Title => write_str_mode(card.title, card.title_safe_hj, card.title_safe_j, html_escape, out),
        CardField::Url => write_str_mode(card.url, card.url_safe_hj, card.url_safe_j, html_escape, out),
        CardField::Date => out.write_slice(card.date),
        CardField::DateLong => out.write_slice(card.date_long),
        CardField::Summary => write_str_mode(card.summary, card.summary_safe_hj, card.summary_safe_j, html_escape, out),
        CardField::Excerpt => write_md_html_mode(card.excerpt, md_json_mode, html_escape, out),
        CardField::ReadingMinutes => push_usize(card.reading_minutes, out),
    }
}

#[inline]
fn eval_truthy(expr: FastExpr, ctx: &RenderCtx<'_>, frames: &[LoopFrame]) -> bool {
    match expr {
        FastExpr::PageTitle => ctx.page.map(|p| !ctx.cards[p.source_idx].title.is_empty()).unwrap_or(false),
        FastExpr::PageUrl => ctx.page.map(|p| !ctx.cards[p.source_idx].url.is_empty()).unwrap_or(false),
        FastExpr::PageSlug => ctx.page.map(|p| !p.slug.is_empty()).unwrap_or(false),
        FastExpr::PageDate => ctx.page.map(|p| !ctx.cards[p.source_idx].date.is_empty()).unwrap_or(false),
        FastExpr::PageDateLong => ctx.page.map(|p| !ctx.cards[p.source_idx].date_long.is_empty()).unwrap_or(false),
        FastExpr::PageSummary => ctx.page.map(|p| !ctx.cards[p.source_idx].summary.is_empty()).unwrap_or(false),
        FastExpr::PageExcerpt => ctx.page.map(|p| !ctx.page_excerpt_override.unwrap_or(ctx.cards[p.source_idx].excerpt).is_empty()).unwrap_or(false),
        FastExpr::PageReadingMinutes => ctx.page.map(|p| ctx.cards[p.source_idx].reading_minutes != 0).unwrap_or(false),
        FastExpr::PageContent => ctx.page.map(|p| !p.content.is_empty()).unwrap_or(false),
        FastExpr::PageIsPost => ctx.page.map(|p| ctx.cards[p.source_idx].is_post).unwrap_or(false),
        FastExpr::PageTags => ctx.page.map(|p| {
            let c = &ctx.cards[p.source_idx];
            if c.is_post { !c.tag_ids.is_empty() } else { !c.own_tags.is_empty() }
        }).unwrap_or(false),
        FastExpr::PageToc => ctx.page.map(|p| !p.toc.is_empty()).unwrap_or(false),
        FastExpr::PageNewerExists => ctx.page.and_then(|p| p.newer).is_some(),
        FastExpr::PageNewerField(f) => ctx.page.and_then(|p| p.newer).map(|ni| !card_field_empty(&ctx.cards[ni], f)).unwrap_or(false),
        FastExpr::PageOlderExists => ctx.page.and_then(|p| p.older).is_some(),
        FastExpr::PageOlderField(f) => ctx.page.and_then(|p| p.older).map(|oi| !card_field_empty(&ctx.cards[oi], f)).unwrap_or(false),

        FastExpr::PagerExists => ctx.pager.is_some(),
        FastExpr::PagerNumber => ctx.pager.map(|pg| pg.number != 0).unwrap_or(false),
        FastExpr::PagerTotal => ctx.pager.map(|pg| pg.total != 0).unwrap_or(false),
        FastExpr::PagerUrl => ctx.pager.map(|pg| !pg.url.is_empty()).unwrap_or(false),
        FastExpr::PagerNewerUrl => ctx.pager.map(|pg| !pg.newer_url.is_empty()).unwrap_or(false),
        FastExpr::PagerOlderUrl => ctx.pager.map(|pg| !pg.older_url.is_empty()).unwrap_or(false),
        FastExpr::Posts => !ctx.posts_slice.is_empty(),

        FastExpr::TagExists => ctx.tag.is_some(),
        FastExpr::TagName => ctx.tag.map(|t| !t.name.is_empty()).unwrap_or(false),
        FastExpr::TagSlug => ctx.tag.map(|t| !t.slug.is_empty()).unwrap_or(false),
        FastExpr::TagUrl => ctx.tag.map(|t| !t.url.is_empty()).unwrap_or(false),
        FastExpr::TagCount => ctx.tag.map(|t| t.count != 0).unwrap_or(false),
        FastExpr::TagPosts => ctx.tag.map(|t| !t.post_sis.is_empty()).unwrap_or(false),

        FastExpr::SiteTitle => !ctx.site_title.is_empty(),
        FastExpr::SiteBaseUrl => !ctx.site_base_url.is_empty(),
        FastExpr::GlobalTags => !ctx.global_tags.is_empty(),
        FastExpr::GlobalRecent => !ctx.recent_sis.is_empty(),

        FastExpr::FrameCard(slot, f) => {
            if let FrameRef::Card(si) = frames[slot as usize].item {
                let c = &ctx.cards[si];
                match f {
                    CardField::Title => !c.title.is_empty(),
                    CardField::Url => !c.url.is_empty(),
                    CardField::Date => !c.date.is_empty(),
                    CardField::DateLong => !c.date_long.is_empty(),
                    CardField::Summary => !c.summary.is_empty(),
                    CardField::Excerpt => !c.excerpt.is_empty(),
                    CardField::ReadingMinutes => c.reading_minutes != 0,
                }
            } else {
                false
            }
        }
        FastExpr::FrameCardTags(slot) => {
            if let FrameRef::Card(si) = frames[slot as usize].item {
                let c = &ctx.cards[si];
                if c.is_post { !c.tag_ids.is_empty() } else { !c.own_tags.is_empty() }
            } else {
                false
            }
        }
        FastExpr::FrameTag(slot, f) => {
            if let FrameRef::PostTag { si, idx } = frames[slot as usize].item {
                let t = get_frame_tag(ctx, si, idx);
                match f {
                    TagField::Name => !t.name.is_empty(),
                    TagField::Slug => !t.slug.is_empty(),
                    TagField::Url => !t.url.is_empty(),
                }
            } else {
                false
            }
        }
        FastExpr::FrameTagSummaryField(slot, f) => {
            if let FrameRef::TagSummary(tid) = frames[slot as usize].item {
                let t = &ctx.global_tags[tid];
                match f {
                    TagField::Name => !t.name.is_empty(),
                    TagField::Slug => !t.slug.is_empty(),
                    TagField::Url => !t.url.is_empty(),
                }
            } else {
                false
            }
        }
        FastExpr::FrameTagSummaryCount(slot) => {
            if let FrameRef::TagSummary(tid) = frames[slot as usize].item {
                ctx.global_tags[tid].count != 0
            } else {
                false
            }
        }
        FastExpr::FrameTocLevel(slot) => {
            if let FrameRef::Toc(k) = frames[slot as usize].item {
                ctx.page.map(|p| p.toc[k].level != 0).unwrap_or(false)
            } else {
                false
            }
        }
        FastExpr::FrameTocId(slot) => {
            if let FrameRef::Toc(k) = frames[slot as usize].item {
                ctx.page.map(|p| !p.toc[k].id.is_empty()).unwrap_or(false)
            } else {
                false
            }
        }
        FastExpr::FrameTocHtml(slot) => {
            if let FrameRef::Toc(k) = frames[slot as usize].item {
                ctx.page.map(|p| !p.toc[k].html.is_empty()).unwrap_or(false)
            } else {
                false
            }
        }
        FastExpr::FrameLoop(slot, f) => {
            let lp = &frames[slot as usize];
            match f {
                LoopField::Index => lp.index != 0,
                LoopField::First => lp.first,
                LoopField::Last => lp.last,
                LoopField::Length => lp.length != 0,
            }
        }
        FastExpr::OpaqueObj => true,
        FastExpr::AlwaysNull => false,
    }
}

#[inline]
fn card_field_empty(card: &RuntimeCard<'_>, f: LinkField) -> bool {
    match f {
        LinkField::Title => card.title.is_empty(),
        LinkField::Url => card.url.is_empty(),
        LinkField::Date => card.date.is_empty(),
        LinkField::DateLong => card.date_long.is_empty(),
    }
}

#[inline]
fn eval_write<S: ByteSink>(
    expr: FastExpr,
    html_escape: bool,
    ctx: &RenderCtx<'_>,
    frames: &[LoopFrame],
    out: &mut S,
) {
    match expr {
        FastExpr::PageTitle => {
            if let Some(p) = ctx.page {
                card_field_write(&ctx.cards[p.source_idx], CardField::Title, ctx.md_json_mode, html_escape, out);
            }
        }
        FastExpr::PageUrl => {
            if let Some(p) = ctx.page {
                card_field_write(&ctx.cards[p.source_idx], CardField::Url, ctx.md_json_mode, html_escape, out);
            }
        }
        FastExpr::PageSlug => {
            if let Some(p) = ctx.page {
                out.write_slice(p.slug);
            }
        }
        FastExpr::PageDate => {
            if let Some(p) = ctx.page {
                out.write_slice(ctx.cards[p.source_idx].date);
            }
        }
        FastExpr::PageDateLong => {
            if let Some(p) = ctx.page {
                out.write_slice(ctx.cards[p.source_idx].date_long);
            }
        }
        FastExpr::PageSummary => {
            if let Some(p) = ctx.page {
                card_field_write(&ctx.cards[p.source_idx], CardField::Summary, ctx.md_json_mode, html_escape, out);
            }
        }
        FastExpr::PageExcerpt => {
            if let Some(p) = ctx.page {
                let s = ctx.page_excerpt_override.unwrap_or(ctx.cards[p.source_idx].excerpt);
                write_md_html_mode(s, ctx.md_json_mode, html_escape, out);
            }
        }
        FastExpr::PageReadingMinutes => {
            if let Some(p) = ctx.page {
                push_usize(ctx.cards[p.source_idx].reading_minutes, out);
            }
        }
        FastExpr::PageContent => {
            if let Some(p) = ctx.page {
                write_md_html_mode(p.content, ctx.md_json_mode, html_escape, out);
            }
        }
        FastExpr::PageIsPost => {
            if let Some(p) = ctx.page {
                out.write_slice(if ctx.cards[p.source_idx].is_post { b"true" } else { b"false" });
            }
        }
        FastExpr::PageTags => {
            if let Some(p) = ctx.page {
                let c = &ctx.cards[p.source_idx];
                write_comma_sep(if c.is_post { c.tag_ids.len() } else { c.own_tags.len() }, out);
            }
        }
        FastExpr::PageToc => {
            if let Some(p) = ctx.page {
                write_comma_sep(p.toc.len(), out);
            }
        }
        FastExpr::PageNewerField(f) => {
            if let Some(ni) = ctx.page.and_then(|p| p.newer) {
                link_field_write(&ctx.cards[ni], f, html_escape, out);
            }
        }
        FastExpr::PageOlderField(f) => {
            if let Some(oi) = ctx.page.and_then(|p| p.older) {
                link_field_write(&ctx.cards[oi], f, html_escape, out);
            }
        }

        FastExpr::PagerNumber => {
            if let Some(pg) = ctx.pager {
                push_usize(pg.number, out);
            }
        }
        FastExpr::PagerTotal => {
            if let Some(pg) = ctx.pager {
                push_usize(pg.total, out);
            }
        }
        FastExpr::PagerUrl => {
            if let Some(pg) = ctx.pager {
                write_str_mode(pg.url, pg.url_safe_hj, pg.url_safe_j, html_escape, out);
            }
        }
        FastExpr::PagerNewerUrl => {
            if let Some(pg) = ctx.pager {
                write_str_mode(pg.newer_url, pg.url_safe_hj, pg.url_safe_j, html_escape, out);
            }
        }
        FastExpr::PagerOlderUrl => {
            if let Some(pg) = ctx.pager {
                write_str_mode(pg.older_url, pg.url_safe_hj, pg.url_safe_j, html_escape, out);
            }
        }
        FastExpr::Posts => write_comma_sep(ctx.posts_slice.len(), out),

        FastExpr::TagName => {
            if let Some(t) = ctx.tag {
                out.write_slice(if html_escape { t.name_hj } else { t.name_j });
            }
        }
        FastExpr::TagSlug => {
            if let Some(t) = ctx.tag {
                out.write_slice(t.slug);
            }
        }
        FastExpr::TagUrl => {
            if let Some(t) = ctx.tag {
                out.write_slice(if html_escape { t.url_hj } else { t.url_j });
            }
        }
        FastExpr::TagCount => {
            if let Some(t) = ctx.tag {
                push_usize(t.count, out);
            }
        }
        FastExpr::TagPosts => {
            if let Some(t) = ctx.tag {
                write_comma_sep(t.post_sis.len(), out);
            }
        }

        FastExpr::SiteTitle => write_str_mode(ctx.site_title, false, false, html_escape, out),
        FastExpr::SiteBaseUrl => write_str_mode(ctx.site_base_url, false, false, html_escape, out),
        FastExpr::GlobalTags => write_comma_sep(ctx.global_tags.len(), out),
        FastExpr::GlobalRecent => write_comma_sep(ctx.recent_sis.len(), out),

        FastExpr::FrameCard(slot, f) => {
            if let FrameRef::Card(si) = frames[slot as usize].item {
                card_field_write(&ctx.cards[si], f, ctx.md_json_mode, html_escape, out);
            }
        }
        FastExpr::FrameCardTags(slot) => {
            if let FrameRef::Card(si) = frames[slot as usize].item {
                let c = &ctx.cards[si];
                write_comma_sep(if c.is_post { c.tag_ids.len() } else { c.own_tags.len() }, out);
            }
        }
        FastExpr::FrameTag(slot, f) => {
            if let FrameRef::PostTag { si, idx } = frames[slot as usize].item {
                let t = get_frame_tag(ctx, si, idx);
                match f {
                    TagField::Name => out.write_slice(if html_escape { t.name_hj } else { t.name_j }),
                    TagField::Slug => out.write_slice(t.slug),
                    TagField::Url => out.write_slice(if html_escape { t.url_hj } else { t.url_j }),
                }
            }
        }
        FastExpr::FrameTagSummaryField(slot, f) => {
            if let FrameRef::TagSummary(tid) = frames[slot as usize].item {
                let t = &ctx.global_tags[tid];
                match f {
                    TagField::Name => out.write_slice(if html_escape { t.name_hj } else { t.name_j }),
                    TagField::Slug => out.write_slice(t.slug),
                    TagField::Url => out.write_slice(if html_escape { t.url_hj } else { t.url_j }),
                }
            }
        }
        FastExpr::FrameTagSummaryCount(slot) => {
            if let FrameRef::TagSummary(tid) = frames[slot as usize].item {
                push_usize(ctx.global_tags[tid].count, out);
            }
        }
        FastExpr::FrameTocLevel(slot) => {
            if let FrameRef::Toc(k) = frames[slot as usize].item {
                if let Some(p) = ctx.page {
                    push_usize(p.toc[k].level as usize, out);
                }
            }
        }
        FastExpr::FrameTocId(slot) => {
            if let FrameRef::Toc(k) = frames[slot as usize].item {
                if let Some(p) = ctx.page {
                    out.write_slice(&p.toc[k].id);
                }
            }
        }
        FastExpr::FrameTocHtml(slot) => {
            if let FrameRef::Toc(k) = frames[slot as usize].item {
                if let Some(p) = ctx.page {
                    write_md_html_mode(&p.toc[k].html, ctx.md_json_mode, html_escape, out);
                }
            }
        }
        FastExpr::FrameLoop(slot, f) => {
            let lp = &frames[slot as usize];
            match f {
                LoopField::Index => push_usize(lp.index, out),
                LoopField::Length => push_usize(lp.length, out),
                LoopField::First => out.write_slice(if lp.first { b"true" } else { b"false" }),
                LoopField::Last => out.write_slice(if lp.last { b"true" } else { b"false" }),
            }
        }
        FastExpr::PageNewerExists
        | FastExpr::PageOlderExists
        | FastExpr::PagerExists
        | FastExpr::TagExists
        | FastExpr::OpaqueObj
        | FastExpr::AlwaysNull => {}
    }
}

pub fn render_compiled_ops<S: ByteSink>(
    ops: &[CompiledOp],
    ctx: &RenderCtx<'_>,
    frames: &mut Vec<LoopFrame>,
    caches: &mut [Vec<Option<Vec<u8>>>],
    out: &mut S,
) {
    for op in ops {
        match op {
            CompiledOp::JsonText(jt) => out.write_slice(jt),
            CompiledOp::Var(e) => eval_write(*e, true, ctx, frames, out),
            CompiledOp::Raw(e) => eval_write(*e, false, ctx, frames, out),
            CompiledOp::If { not, expr, then_ops, else_ops } => {
                let tv = eval_truthy(*expr, ctx, frames);
                if tv != *not {
                    render_compiled_ops(then_ops, ctx, frames, caches, out);
                } else if let Some(eo) = else_ops {
                    render_compiled_ops(eo, ctx, frames, caches, out);
                }
            }
            CompiledOp::For { arr, body, cache_slot } => match *arr {
                ArrExpr::Posts => {
                    run_card_loop(ctx.posts_slice, body, *cache_slot, ctx, frames, caches, out);
                }
                ArrExpr::TagPosts => {
                    if let Some(t) = ctx.tag {
                        run_card_loop(t.post_sis, body, *cache_slot, ctx, frames, caches, out);
                    }
                }
                ArrExpr::GlobalRecent => {
                    run_card_loop(ctx.recent_sis, body, *cache_slot, ctx, frames, caches, out);
                }
                ArrExpr::PageTags => {
                    if let Some(p) = ctx.page {
                        let si = p.source_idx;
                        let c = &ctx.cards[si];
                        let len = if c.is_post { c.tag_ids.len() } else { c.own_tags.len() };
                        for k in 0..len {
                            frames.push(LoopFrame {
                                item: FrameRef::PostTag { si, idx: k },
                                index: k + 1,
                                length: len,
                                first: k == 0,
                                last: k + 1 == len,
                            });
                            render_compiled_ops(body, ctx, frames, caches, out);
                            frames.pop();
                        }
                    }
                }
                ArrExpr::FrameCardTags(slot) => {
                    if let FrameRef::Card(si) = frames[slot as usize].item {
                        let c = &ctx.cards[si];
                        let len = if c.is_post { c.tag_ids.len() } else { c.own_tags.len() };
                        for k in 0..len {
                            frames.push(LoopFrame {
                                item: FrameRef::PostTag { si, idx: k },
                                index: k + 1,
                                length: len,
                                first: k == 0,
                                last: k + 1 == len,
                            });
                            render_compiled_ops(body, ctx, frames, caches, out);
                            frames.pop();
                        }
                    }
                }
                ArrExpr::PageToc => {
                    if let Some(p) = ctx.page {
                        let len = p.toc.len();
                        for k in 0..len {
                            frames.push(LoopFrame {
                                item: FrameRef::Toc(k),
                                index: k + 1,
                                length: len,
                                first: k == 0,
                                last: k + 1 == len,
                            });
                            render_compiled_ops(body, ctx, frames, caches, out);
                            frames.pop();
                        }
                    }
                }
                ArrExpr::GlobalTags => {
                    let len = ctx.global_tags.len();
                    for k in 0..len {
                        frames.push(LoopFrame {
                            item: FrameRef::TagSummary(k),
                            index: k + 1,
                            length: len,
                            first: k == 0,
                            last: k + 1 == len,
                        });
                        render_compiled_ops(body, ctx, frames, caches, out);
                        frames.pop();
                    }
                }
                ArrExpr::Empty => {}
            },
        }
    }
}

#[inline]
fn run_card_loop<S: ByteSink>(
    sis: &[usize],
    body: &[CompiledOp],
    cache_slot: Option<usize>,
    ctx: &RenderCtx<'_>,
    frames: &mut Vec<LoopFrame>,
    caches: &mut [Vec<Option<Vec<u8>>>],
    out: &mut S,
) {
    let len = sis.len();
    for (k, &si) in sis.iter().enumerate() {
        if let Some(cs) = cache_slot {
            let pr = ctx.cards[si].post_rank;
            if let Some(hit) = caches[cs][pr].as_ref() {
                out.write_slice(hit);
                continue;
            }
            let mut buf = Vec::with_capacity(512);
            frames.push(LoopFrame {
                item: FrameRef::Card(si),
                index: k + 1,
                length: len,
                first: k == 0,
                last: k + 1 == len,
            });
            render_compiled_ops(body, ctx, frames, caches, &mut buf);
            frames.pop();
            out.write_slice(&buf);
            caches[cs][pr] = Some(buf);
            continue;
        }
        frames.push(LoopFrame {
            item: FrameRef::Card(si),
            index: k + 1,
            length: len,
            first: k == 0,
            last: k + 1 == len,
        });
        render_compiled_ops(body, ctx, frames, caches, out);
        frames.pop();
    }
}
