//! The interpreter that runs code while compiling: `comptime val`, the
//! conditions of `comptime if`, the lists of `comptime for`, attribute
//! values, `comptime fn`s and every ordinary function they call.
//!
//! It runs the checked code (the typed tree) directly. Values are shared the
//! way they are in compiled code (a list or struct is one object, wherever
//! it's held), and every operation gives exactly what the compiled program
//! would: the same string functions, the same overflow and bounds panics.
//! A panic becomes a compile error. What a program can only do when it runs
//! (files, the network, the clock, printing, tasks) is refused.
//!
//! In generic code checked as written, values that depend on the type
//! parameters aren't known yet: evaluation stops with `Ctl::Unknown`, and
//! the code depending on it is worked out in each copy instead.

use super::*;
use std::cell::RefCell;
use std::rc::Rc;

/// Evaluation steps one `comptime` evaluation may take.
const MAX_STEPS: u64 = 20_000_000;
/// Nested calls.
const MAX_DEPTH: usize = 500;

/// A value while compiling.
#[derive(Clone, Debug)]
pub(super) enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Rc<str>),
    Bytes(Rc<[u8]>),
    Unit,
    Null,
    List(Rc<RefCell<Vec<Value>>>),
    Map(Rc<RefCell<MapVal>>),
    /// A struct, tuple, closure's captured values or interface box.
    Struct(Rc<StructVal>),
    /// An enum value: its variant and fields.
    Enum(StructId, usize, Rc<Vec<Value>>),
    /// A function value: its code, and the closure (captured values) if any.
    Closure(FuncId, Option<Rc<StructVal>>),
    /// A field of a struct (from `fields(T)`): the struct and the index.
    Field(StructId, usize),
    /// A parameter of a function (from `params(H)`).
    Param(FuncId, usize),
    /// What a `comptime val` holds when working it out failed (reported):
    /// using it stops quietly instead of reporting more errors.
    Failed,
}

#[derive(Debug)]
pub(super) struct StructVal {
    pub sid: StructId,
    pub fields: RefCell<Vec<Value>>,
    /// For an attribute's value: where it's written (for `compile_error(..., at = a)`).
    pub origin: Option<Span>,
}

/// A map, in insertion order.
#[derive(Debug, Default)]
pub(super) struct MapVal {
    pub entries: Vec<(Value, Value)>,
    index: HashMap<Key, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Int(i64),
    Float(u64),
    Bool(bool),
    Str(Rc<[u8]>),
    Variant(StructId, usize),
}

impl MapVal {
    fn get(&self, k: &Key) -> Option<&Value> {
        self.index.get(k).map(|&i| &self.entries[i].1)
    }

    fn set(&mut self, k: Key, key: Value, v: Value) {
        match self.index.get(&k) {
            Some(&i) => self.entries[i].1 = v,
            None => {
                self.index.insert(k, self.entries.len());
                self.entries.push((key, v));
            }
        }
    }

    fn remove(&mut self, k: &Key) -> Option<Value> {
        let i = self.index.remove(k)?;
        let (_, v) = self.entries.remove(i);
        for slot in self.index.values_mut() {
            if *slot > i {
                *slot -= 1;
            }
        }
        Some(v)
    }
}

/// Why evaluation stopped before giving a value.
#[derive(Debug)]
pub(super) enum Ctl {
    Break,
    Continue,
    Return(Value),
    /// Something isn't known yet (generic code, or a run-time value): where.
    Unknown(Span),
    /// A panic, or code that can't run while compiling: the error.
    Fail(Box<Diagnostic>),
    /// `compile_error(message)` ran: the message, `at`, and where it's written.
    CompileError { message: String, at: Option<Span>, raised: Span },
    /// It failed, and that was reported already.
    Quiet,
}

type R<T> = Result<T, Ctl>;

/// The local variables of one function call.
struct Frame {
    locals: Vec<Option<Value>>,
}

impl Frame {
    fn set(&mut self, id: LocalId, v: Value) {
        if id >= self.locals.len() {
            self.locals.resize(id + 1, None);
        }
        self.locals[id] = Some(v);
    }
}

pub(super) fn str_val(s: &str) -> Value {
    Value::Str(Rc::from(s))
}

pub(super) fn list_val(items: Vec<Value>) -> Value {
    Value::List(Rc::new(RefCell::new(items)))
}

/// The bytes of a `String` or `Bytes` value.
fn bytes(v: &Value) -> &[u8] {
    match v {
        Value::Str(s) => s.as_bytes(),
        Value::Bytes(b) => b,
        _ => &[],
    }
}

/// A `String` (if `like` is one) or `Bytes` made of `b`.
fn like(like: &Value, b: &[u8]) -> Value {
    match like {
        Value::Bytes(_) => Value::Bytes(Rc::from(b)),
        _ => Value::Str(Rc::from(String::from_utf8_lossy(b).as_ref())),
    }
}

fn find_from(h: &[u8], n: &[u8], from: usize) -> Option<usize> {
    if n.is_empty() {
        return Some(from);
    }
    if n.len() > h.len() {
        return None;
    }
    (from..=h.len() - n.len()).find(|&i| &h[i..i + n.len()] == n)
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0c | 0x0b)
}

/// Text trimmed of spaces and tabs, as the runtime's number parsing does.
fn trim_number(s: &[u8]) -> &str {
    let mut a = 0;
    let mut b = s.len();
    while a < b && (s[a] == b' ' || s[a] == b'\t') {
        a += 1;
    }
    while b > a && (s[b - 1] == b' ' || s[b - 1] == b'\t') {
        b -= 1;
    }
    std::str::from_utf8(&s[a..b]).unwrap_or("")
}

fn parse_int(s: &[u8]) -> Option<i64> {
    let t = trim_number(s);
    if t.is_empty() {
        return None;
    }
    t.parse::<i64>().ok()
}

fn parse_float(s: &[u8]) -> Option<f64> {
    let t = trim_number(s);
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok()
}

fn parse_bool(s: &[u8]) -> Option<bool> {
    let t = trim_number(s).to_ascii_lowercase();
    match t.as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// A float as Tarn prints it: the shortest text that reads back the same,
/// like Python's `repr` (`0.1`, `3.0`, `1e+16`, `1.5e-07`).
pub(super) fn format_float(d: f64) -> String {
    if d.is_nan() {
        return "nan".into();
    }
    if d.is_infinite() {
        return if d < 0.0 { "-inf".into() } else { "inf".into() };
    }
    if d == 0.0 {
        return if d.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // Rust's `{:e}` is the shortest round-tripping form: "1.5e-7", "-3e0".
    let sci = format!("{d:e}");
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let nd = digits.len() as i32;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if !(-4..16).contains(&exp) {
        out.push_str(&digits[..1]);
        if nd > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push_str(&format!("e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs()));
        return out;
    }
    if exp < 0 {
        out.push_str("0.");
        for _ in 0..(-exp - 1) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        for i in 0..=exp {
            out.push(digits.as_bytes().get(i as usize).map(|&c| c as char).unwrap_or('0'));
        }
        out.push('.');
        if nd > exp + 1 {
            out.push_str(&digits[(exp + 1) as usize..]);
        } else {
            out.push('0');
        }
    }
    out
}

/// A string as written in Tarn code (quotes and escapes).
fn repr(s: &[u8]) -> Vec<u8> {
    let mut out = vec![b'"'];
    for &c in s {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0 => out.extend_from_slice(b"\\0"),
            c => out.push(c),
        }
    }
    out.push(b'"');
    out
}

/// The length in bytes of the UTF-8 character starting at `i`.
fn char_len_at(s: &[u8], i: usize) -> usize {
    let c = s[i];
    let n = if c < 0x80 {
        1
    } else if c >> 5 == 0x6 {
        2
    } else if c >> 4 == 0xE {
        3
    } else if c >> 3 == 0x1E {
        4
    } else {
        1
    };
    n.min(s.len() - i)
}

fn utf8_count(s: &[u8]) -> i64 {
    s.iter().filter(|&&c| c & 0xC0 != 0x80).count() as i64
}

fn is_char_boundary(s: &[u8], i: i64) -> bool {
    i == 0 || i == s.len() as i64 || (i > 0 && (i as usize) < s.len() && s[i as usize] & 0xC0 != 0x80)
}

pub(super) struct Interp<'c> {
    pub c: &'c mut Checker,
    steps: u64,
    depth: usize,
    /// The functions being run, innermost last (for "can't run" messages).
    stack: Vec<FuncId>,
}

impl<'c> Interp<'c> {
    pub fn new(c: &'c mut Checker) -> Self {
        Interp { c, steps: 0, depth: 0, stack: Vec::new() }
    }

    fn fail<T>(&self, msg: impl Into<String>, span: Span) -> R<T> {
        let mut d = Diagnostic::error(msg, span);
        if let Some(&f) = self.stack.last() {
            d = d.with_note(format!("while running `{}` (compile-time code)", self.c.funcs[f].name));
        }
        Err(Ctl::Fail(Box::new(d)))
    }

    fn panic<T>(&self, msg: impl Into<String>, span: Span) -> R<T> {
        let msg = msg.into();
        match self.fail(format!("panic while compiling: {msg}"), span) {
            Err(Ctl::Fail(d)) => Err(Ctl::Fail(Box::new(d.with_primary("panics here")))),
            other => other,
        }
    }

    fn cannot<T>(&self, what: &str, span: Span) -> R<T> {
        let d = Diagnostic::error(format!("{what} can't run while compiling"), span)
            .with_note("code that runs while compiling can compute with values, but not touch files, the network, the clock or the screen");
        let d = match self.stack.last() {
            Some(&f) => d.with_note(format!("`{}` uses it", self.c.funcs[f].name)),
            None => d,
        };
        Err(Ctl::Fail(Box::new(d)))
    }

    fn tick(&mut self, span: Span) -> R<()> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(Ctl::Fail(Box::new(
                Diagnostic::error("compile-time code runs too long", span)
                    .with_note(format!("it was stopped after {MAX_STEPS} steps; is there an endless loop?")),
            )));
        }
        Ok(())
    }

    // ---------- entry points ----------

    /// Evaluate `e`, an expression of the function being checked, whose
    /// locals have the values in `known` (`None`: not known while compiling).
    pub fn eval_here(&mut self, e: &TExpr, known: Vec<Option<Value>>) -> R<Value> {
        let mut f = Frame { locals: known };
        self.expr(e, &mut f)
    }

    /// Call function `fid` with `args`.
    pub fn call(&mut self, fid: FuncId, args: Vec<Value>, span: Span) -> R<Value> {
        if let Some((_, targs)) = &self.c.funcs[fid].instance
            && targs.iter().any(|t| t.has_param(&self.c.struct_has_param))
        {
            return Err(Ctl::Unknown(span));
        }
        if !self.c.funcs[fid].type_params.is_empty() {
            return Err(Ctl::Unknown(span));
        }
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(Ctl::Fail(Box::new(
                Diagnostic::error("compile-time code calls itself too deeply", span).with_note(format!("more than {MAX_DEPTH} calls deep: is there an endless recursion?")),
            )));
        }
        let body = match self.c.body_for(fid, span) {
            Ok(b) => b,
            Err(e) => {
                self.depth -= 1;
                return Err(e);
            }
        };
        let mut f = Frame { locals: vec![None; body.locals.len()] };
        for (p, v) in body.params.iter().zip(args) {
            f.set(*p, v);
        }
        self.stack.push(fid);
        let r = self.stmts(&body.body, &mut f);
        self.stack.pop();
        self.depth -= 1;
        match r {
            Ok(()) => Ok(Value::Unit),
            Err(Ctl::Return(v)) => Ok(v),
            Err(Ctl::Break | Ctl::Continue) => Ok(Value::Unit),
            Err(e) => Err(e),
        }
    }

    /// Call a function value.
    fn call_value(&mut self, f: Value, args: Vec<Value>, span: Span) -> R<Value> {
        match f {
            Value::Closure(func, env) => {
                let me = match env {
                    Some(e) => Value::Struct(e),
                    None => Value::Unit,
                };
                let mut all = vec![me];
                all.extend(args);
                self.call(func, all, span)
            }
            Value::Null => self.panic("called a null function", span),
            _ => self.fail("internal error: calling something that isn't a function while compiling", span),
        }
    }

    // ---------- statements ----------

    fn stmts(&mut self, stmts: &[TStmt], f: &mut Frame) -> R<()> {
        for s in stmts {
            self.stmt(s, f)?;
        }
        Ok(())
    }

    fn stmt(&mut self, s: &TStmt, f: &mut Frame) -> R<()> {
        match s {
            TStmt::Let(id, e) => {
                let v = self.expr(e, f)?;
                f.set(*id, v);
            }
            TStmt::Assign { place, op, value, span } => self.assign(place, *op, value, *span, f)?,
            TStmt::Expr(e) => {
                self.expr(e, f)?;
            }
            TStmt::If { branches, else_block } => {
                for (c, b) in branches {
                    if self.truth(c, f)? {
                        return self.stmts(b, f);
                    }
                }
                if let Some(b) = else_block {
                    self.stmts(b, f)?;
                }
            }
            TStmt::While { cond, body } => {
                while self.truth(cond, f)? {
                    self.tick(cond.span)?;
                    match self.stmts(body, f) {
                        Ok(()) | Err(Ctl::Continue) => {}
                        Err(Ctl::Break) => break,
                        Err(e) => return Err(e),
                    }
                }
            }
            TStmt::ForRange { var, start, end, inclusive, body } => {
                let a = self.int(start, f)?;
                let b = self.int(end, f)?;
                let mut i = a;
                while if *inclusive { i <= b } else { i < b } {
                    self.tick(start.span)?;
                    f.set(*var, Value::Int(i));
                    match self.stmts(body, f) {
                        Ok(()) | Err(Ctl::Continue) => {}
                        Err(Ctl::Break) => break,
                        Err(e) => return Err(e),
                    }
                    if i == i64::MAX {
                        break;
                    }
                    i += 1;
                }
            }
            TStmt::ForList { var, index, list, body } => {
                let l = self.expr(list, f)?;
                let mut k = 0usize;
                loop {
                    // (The length is read each time, as compiled code does.)
                    let item = match &l {
                        Value::List(items) => match items.borrow().get(k) {
                            Some(v) => v.clone(),
                            None => break,
                        },
                        // `for c in s`: each character.
                        Value::Str(s) => {
                            let b = s.as_bytes();
                            if k >= b.len() {
                                break;
                            }
                            let n = char_len_at(b, k);
                            let ch = like(&l, &b[k..k + n]);
                            k += n - 1;
                            ch
                        }
                        _ => return self.fail("internal error: `for` over a value that isn't a list", list.span),
                    };
                    self.tick(list.span)?;
                    f.set(*var, item);
                    if let Some(ix) = index {
                        f.set(*ix, Value::Int(k as i64));
                    }
                    match self.stmts(body, f) {
                        Ok(()) | Err(Ctl::Continue) => {}
                        Err(Ctl::Break) => break,
                        Err(e) => return Err(e),
                    }
                    k += 1;
                }
            }
            TStmt::ForMap { key, value, map, body, span } => {
                let m = self.expr(map, f)?;
                let Value::Map(m) = m else { return self.fail("internal error: `for` over a value that isn't a map", map.span) };
                let len = m.borrow().entries.len();
                let mut i = 0;
                while i < len {
                    if m.borrow().entries.len() != len {
                        return self.panic("map changed size during iteration", *span);
                    }
                    self.tick(*span)?;
                    let (k, v) = m.borrow().entries[i].clone();
                    f.set(*key, k);
                    if let Some(vid) = value {
                        f.set(*vid, v);
                    }
                    match self.stmts(body, f) {
                        Ok(()) | Err(Ctl::Continue) => {}
                        Err(Ctl::Break) => break,
                        Err(e) => return Err(e),
                    }
                    i += 1;
                }
                if m.borrow().entries.len() != len {
                    return self.panic("map changed size during iteration", *span);
                }
            }
            TStmt::Match { scrutinee, arms } => {
                let v = self.expr(scrutinee, f)?;
                let arm = self.pick_arm(&v, arms, scrutinee.span, f)?;
                self.stmts(&arms[arm].body, f)?;
            }
            TStmt::Break => return Err(Ctl::Break),
            TStmt::Continue => return Err(Ctl::Continue),
            TStmt::Return(None) => return Err(Ctl::Return(Value::Unit)),
            TStmt::Return(Some(e)) => {
                let v = self.expr(e, f)?;
                return Err(Ctl::Return(v));
            }
            TStmt::Block(b) => self.stmts(b, f)?,
            TStmt::Unrolled(iters) => {
                for b in iters {
                    match self.stmts(b, f) {
                        Ok(()) | Err(Ctl::Continue) => {}
                        Err(Ctl::Break) => break,
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        Ok(())
    }

    fn assign(&mut self, place: &Place, op: Option<CompoundOp>, value: &TExpr, span: Span, f: &mut Frame) -> R<()> {
        match place {
            Place::Local(id) => {
                let v = self.expr(value, f)?;
                let v = match op {
                    None => v,
                    Some(op) => {
                        let old = f.locals.get(*id).cloned().flatten().ok_or(Ctl::Unknown(span))?;
                        self.compound(op, old, v, &value.ty, span)?
                    }
                };
                f.set(*id, v);
            }
            Place::Field { obj, idx, .. } => {
                let o = self.expr(obj, f)?;
                let Value::Struct(sv) = o else { return self.fail("internal error: a field of a value that isn't a struct", obj.span) };
                let v = self.expr(value, f)?;
                let v = match op {
                    None => v,
                    Some(op) => {
                        let old = sv.fields.borrow()[*idx].clone();
                        self.compound(op, old, v, &value.ty, span)?
                    }
                };
                sv.fields.borrow_mut()[*idx] = v;
            }
            Place::Index { list, index, span: at } => {
                let l = self.expr(list, f)?;
                let i = self.int(index, f)?;
                let v = self.expr(value, f)?;
                let Value::List(items) = l else { return self.fail("internal error: indexing a value that isn't a list", list.span) };
                let len = items.borrow().len() as i64;
                if i < 0 || i >= len {
                    return self.panic(format!("index {i} out of bounds for list of length {len}"), *at);
                }
                let v = match op {
                    None => v,
                    Some(op) => {
                        let old = items.borrow()[i as usize].clone();
                        self.compound(op, old, v, &value.ty, span)?
                    }
                };
                items.borrow_mut()[i as usize] = v;
            }
            Place::Custom { obj, index, get, set, .. } => {
                let o = self.expr(obj, f)?;
                let i = self.expr(index, f)?;
                let v = self.expr(value, f)?;
                let v = match (op, get) {
                    (None, _) => v,
                    (Some(op), Some(g)) => {
                        let old = self.call(*g, vec![o.clone(), i.clone()], span)?;
                        self.compound(op, old, v, &value.ty, span)?
                    }
                    (Some(_), None) => return self.fail("internal error: `op=` without `fn []`", span),
                };
                self.call(*set, vec![o, i, v], span)?;
            }
            Place::MapEntry { map, key, span: at } => {
                let m = self.expr(map, f)?;
                let k = self.expr(key, f)?;
                let v = self.expr(value, f)?;
                let Value::Map(m) = m else { return self.fail("internal error: not a map", map.span) };
                let hk = self.key(&k, key.span)?;
                let v = match op {
                    None => v,
                    Some(op) => {
                        let old = m.borrow().get(&hk).cloned();
                        let Some(old) = old else { return self.panic(self.missing_key(&k), *at) };
                        self.compound(op, old, v, &value.ty, span)?
                    }
                };
                m.borrow_mut().set(hk, k, v);
            }
        }
        Ok(())
    }

    fn compound(&mut self, op: CompoundOp, old: Value, v: Value, ty: &Type, span: Span) -> R<Value> {
        match op {
            CompoundOp::Builtin(b) => self.binary(b, ty, old, v, span),
            CompoundOp::Call(fid) => self.call(fid, vec![old, v], span),
        }
    }

    fn missing_key(&self, k: &Value) -> String {
        match k {
            Value::Str(s) => format!("key \"{s}\" not found in map"),
            Value::Int(n) => format!("key {n} not found in map"),
            Value::Bool(b) => format!("key {b} not found in map"),
            _ => "key not found in map".into(),
        }
    }

    fn key(&self, k: &Value, span: Span) -> R<Key> {
        Ok(match k {
            Value::Int(n) => Key::Int(*n),
            Value::Float(x) => Key::Float(x.to_bits()),
            Value::Bool(b) => Key::Bool(*b),
            Value::Str(s) => Key::Str(Rc::from(s.as_bytes())),
            Value::Enum(sid, k, fields) if fields.is_empty() => Key::Variant(*sid, *k),
            _ => return self.cannot("a map with keys of your own types", span),
        })
    }

    fn pick_arm(&mut self, v: &Value, arms: &[TArm], span: Span, f: &mut Frame) -> R<usize> {
        for (i, a) in arms.iter().enumerate() {
            if self.matches(&a.pat, v, f)? {
                match &a.guard {
                    Some(g) if !self.truth(g, f)? => continue,
                    _ => return Ok(i),
                }
            }
        }
        self.fail("internal error: no case of a `match` fits while compiling", span)
    }

    fn matches(&mut self, p: &TPat, v: &Value, f: &mut Frame) -> R<bool> {
        Ok(match &p.kind {
            TPatKind::Wild => true,
            TPatKind::Bind(id, inner) => {
                if self.matches(inner, v, f)? {
                    f.set(*id, v.clone());
                    true
                } else {
                    false
                }
            }
            TPatKind::Variant(_, k, subs) => match v {
                Value::Enum(_, vk, fields) if vk == k => {
                    let fields = fields.clone();
                    for (s, x) in subs.iter().zip(fields.iter()) {
                        if !self.matches(s, x, f)? {
                            return Ok(false);
                        }
                    }
                    true
                }
                _ => false,
            },
            TPatKind::Struct(_, subs) => match v {
                Value::Struct(sv) => {
                    let fields = sv.fields.borrow().clone();
                    for (s, x) in subs.iter().zip(fields.iter()) {
                        if !self.matches(s, x, f)? {
                            return Ok(false);
                        }
                    }
                    true
                }
                _ => false,
            },
            TPatKind::Int(n) => matches!(v, Value::Int(m) if m == n),
            TPatKind::Str(s) => matches!(v, Value::Str(t) if **t == **s),
            TPatKind::Bool(b) => matches!(v, Value::Bool(c) if c == b),
            TPatKind::Null => matches!(v, Value::Null),
            TPatKind::Some(inner) => !matches!(v, Value::Null) && self.matches(inner, v, f)?,
            TPatKind::Is(ty, inner) => match self.unbox_as(v, ty) {
                Some(x) => self.matches(inner, &x, f)?,
                None => false,
            },
            TPatKind::Or(alts) => {
                for a in alts {
                    if self.matches(a, v, f)? {
                        return Ok(true);
                    }
                }
                false
            }
        })
    }

    /// The value inside interface value `v`, if it's a `ty` (looking through boxes in boxes).
    fn unbox_as(&self, v: &Value, ty: &Type) -> Option<Value> {
        let Value::Struct(sv) = v else { return None };
        let inner_ty = self.c.structs[sv.sid].fields.first()?.ty.clone();
        let inner = sv.fields.borrow().first()?.clone();
        if inner_ty == *ty {
            return Some(inner);
        }
        if matches!(inner_ty, Type::Iface(_)) { self.unbox_as(&inner, ty) } else { None }
    }

    // ---------- expressions ----------

    fn truth(&mut self, e: &TExpr, f: &mut Frame) -> R<bool> {
        match self.expr(e, f)? {
            Value::Bool(b) => Ok(b),
            _ => self.fail("internal error: a condition that isn't a `Bool` while compiling", e.span),
        }
    }

    fn int(&mut self, e: &TExpr, f: &mut Frame) -> R<i64> {
        match self.expr(e, f)? {
            Value::Int(n) => Ok(n),
            _ => self.fail("internal error: expected an `Int` while compiling", e.span),
        }
    }

    fn exprs(&mut self, es: &[TExpr], f: &mut Frame) -> R<Vec<Value>> {
        es.iter().map(|e| self.expr(e, f)).collect()
    }

    fn expr(&mut self, e: &TExpr, f: &mut Frame) -> R<Value> {
        self.tick(e.span)?;
        let span = e.span;
        Ok(match &e.kind {
            TExprKind::Int(n) => Value::Int(*n),
            TExprKind::Float(x) => Value::Float(*x),
            TExprKind::Bool(b) => Value::Bool(*b),
            TExprKind::Str(s) => str_val(s),
            TExprKind::Null => Value::Null,
            TExprKind::Local(id) => match f.locals.get(*id).cloned().flatten() {
                Some(Value::Failed) => return Err(Ctl::Quiet),
                Some(v) => v,
                None => return Err(Ctl::Unknown(span)),
            },
            TExprKind::Call(fid, args) => {
                let a = self.exprs(args, f)?;
                self.call(*fid, a, span)?
            }
            TExprKind::Builtin(b, args) => {
                let a = self.exprs(args, f)?;
                self.builtin(*b, a, &e.ty, span)?
            }
            TExprKind::Binary(op, l, r) => {
                let a = self.expr(l, f)?;
                let b = self.expr(r, f)?;
                self.binary(*op, &l.ty, a, b, span)?
            }
            TExprKind::Neg(x) => match self.expr(x, f)? {
                Value::Int(n) => match n.checked_neg() {
                    Some(m) => Value::Int(m),
                    None => return self.panic("integer overflow", span),
                },
                Value::Float(v) => Value::Float(-v),
                _ => return self.fail("internal error: negating a non-number", span),
            },
            TExprKind::Not(x) => Value::Bool(!self.truth(x, f)?),
            TExprKind::And(a, b) => Value::Bool(self.truth(a, f)? && self.truth(b, f)?),
            TExprKind::Or(a, b) => Value::Bool(self.truth(a, f)? || self.truth(b, f)?),
            TExprKind::Field(x, _, idx) => {
                let o = self.expr(x, f)?;
                self.field(&o, *idx, span)?
            }
            TExprKind::Index(l, i) => {
                let lv = self.expr(l, f)?;
                let i = self.int(i, f)?;
                match &lv {
                    Value::List(items) => {
                        let items = items.borrow();
                        if i < 0 || i as usize >= items.len() {
                            return self.panic(format!("index {i} out of bounds for list of length {}", items.len()), span);
                        }
                        items[i as usize].clone()
                    }
                    Value::Bytes(b) => {
                        if i < 0 || i as usize >= b.len() {
                            return self.panic(format!("index {i} out of bounds for bytes of length {}", b.len()), span);
                        }
                        Value::Int(b[i as usize] as i64)
                    }
                    _ => return self.fail("internal error: indexing while compiling", span),
                }
            }
            TExprKind::StructNew(sid, args) => {
                let a = self.exprs(args, f)?;
                Value::Struct(Rc::new(StructVal { sid: *sid, fields: RefCell::new(a), origin: None }))
            }
            TExprKind::EnumNew(sid, k, args) => {
                let a = self.exprs(args, f)?;
                Value::Enum(*sid, *k, Rc::new(a))
            }
            TExprKind::ListLit(items) => list_val(self.exprs(items, f)?),
            TExprKind::Let(id, v, body) => {
                let v = self.expr(v, f)?;
                f.set(*id, v);
                self.expr(body, f)?
            }
            TExprKind::MapLit(entries) => {
                let mut m = MapVal::default();
                for (k, v) in entries {
                    let kv = self.expr(k, f)?;
                    let vv = self.expr(v, f)?;
                    let hk = self.key(&kv, k.span)?;
                    m.set(hk, kv, vv);
                }
                Value::Map(Rc::new(RefCell::new(m)))
            }
            TExprKind::ListFind { list, value, elem, needle, test, index } => {
                let l = self.expr(list, f)?;
                let n = self.expr(value, f)?;
                f.set(*needle, n);
                let items = match &l {
                    Value::List(items) => items.borrow().clone(),
                    _ => return self.fail("internal error: searching a value that isn't a list", span),
                };
                let mut found = None;
                for (i, x) in items.into_iter().enumerate() {
                    f.set(*elem, x);
                    if self.truth(test, f)? {
                        found = Some(i);
                        break;
                    }
                }
                match (found, index) {
                    (Some(i), true) => Value::Int(i as i64),
                    (None, true) => Value::Null,
                    (found, false) => Value::Bool(found.is_some()),
                }
            }
            TExprKind::ListFormat { list, elem, each } => {
                let l = self.expr(list, f)?;
                let items = match &l {
                    Value::List(items) => items.borrow().clone(),
                    _ => return self.fail("internal error: formatting a value that isn't a list", span),
                };
                let mut out = String::from("[");
                for (i, x) in items.into_iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    f.set(*elem, x);
                    let s = self.expr(each, f)?;
                    out.push_str(&String::from_utf8_lossy(bytes(&s)));
                }
                out.push(']');
                str_val(&out)
            }
            TExprKind::MapFormat { map, key, value, key_str, value_str } => {
                let m = self.expr(map, f)?;
                let Value::Map(m) = m else { return self.fail("internal error: formatting a value that isn't a map", span) };
                let entries = m.borrow().entries.clone();
                let mut out = String::from("{");
                for (i, (k, v)) in entries.into_iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    f.set(*key, k);
                    f.set(*value, v);
                    let ks = self.expr(key_str, f)?;
                    out.push_str(&String::from_utf8_lossy(bytes(&ks)));
                    out.push_str(": ");
                    let vs = self.expr(value_str, f)?;
                    out.push_str(&String::from_utf8_lossy(bytes(&vs)));
                }
                out.push('}');
                str_val(&out)
            }
            TExprKind::FString(parts) => {
                let mut out = Vec::new();
                for p in parts {
                    let v = self.expr(p, f)?;
                    out.extend_from_slice(bytes(&v));
                }
                str_val(&String::from_utf8_lossy(&out))
            }
            TExprKind::IsNull(x) => Value::Bool(matches!(self.expr(x, f)?, Value::Null)),
            TExprKind::Wrap(x) | TExprKind::Unwrap(x) => self.expr(x, f)?,
            TExprKind::NotNull(x) => match self.expr(x, f)? {
                Value::Null => return self.panic("`!!` on a null value", span),
                v => v,
            },
            TExprKind::Try { value, binder, on_err } => match self.expr(value, f)? {
                Value::Enum(_, 0, fields) => fields.first().cloned().unwrap_or(Value::Unit),
                Value::Enum(_, _, fields) => {
                    f.set(*binder, fields.first().cloned().unwrap_or(Value::Unit));
                    self.expr(on_err, f)?
                }
                _ => return self.fail("internal error: `?` on a value that isn't a `Result`", span),
            },
            TExprKind::SafeCall { recv, binder, body } => match self.expr(recv, f)? {
                Value::Null => Value::Null,
                v => {
                    f.set(*binder, v);
                    self.expr(body, f)?
                }
            },
            TExprKind::Elvis(a, b) => match self.expr(a, f)? {
                Value::Null => self.expr(b, f)?,
                v => v,
            },
            TExprKind::Jump(s) => {
                self.stmt(s, f)?;
                Value::Unit
            }
            TExprKind::Closure { func, sid, captures } => {
                let caps = self.exprs(captures, f)?;
                let env = sid.map(|s| Rc::new(StructVal { sid: s, fields: RefCell::new(caps), origin: None }));
                Value::Closure(*func, env)
            }
            TExprKind::CallValue(callee, args) => {
                let fv = self.expr(callee, f)?;
                let a = self.exprs(args, f)?;
                self.call_value(fv, a, span)?
            }
            TExprKind::Boxed(sid, x) => {
                let v = self.expr(x, f)?;
                Value::Struct(Rc::new(StructVal { sid: *sid, fields: RefCell::new(vec![v]), origin: None }))
            }
            TExprKind::CallMethod(recv, slot, args) => {
                let r = self.expr(recv, f)?;
                let a = self.exprs(args, f)?;
                let Value::Struct(sv) = &r else { return self.fail("internal error: a method call on something that isn't an interface value", span) };
                let Some(&m) = self.c.structs[sv.sid].vtable.get(*slot) else {
                    return self.fail("internal error: a method missing from an interface's table while compiling", span);
                };
                let mut all = vec![r.clone()];
                all.extend(a);
                self.call(m, all, span)?
            }
            TExprKind::Block(stmts, v) => {
                self.stmts(stmts, f)?;
                self.expr(v, f)?
            }
            TExprKind::Match(x, arms) => {
                let v = self.expr(x, f)?;
                let k = self.pick_arm(&v, arms, x.span, f)?;
                self.stmts(&arms[k].body, f)?;
                match &arms[k].value {
                    Some(val) => self.expr(val, f)?,
                    None => Value::Unit,
                }
            }
            TExprKind::Sync(..) => return self.cannot("tasks, channels and mutexes", span),
            TExprKind::Meta(op, args) => {
                let a = self.exprs(args, f)?;
                self.meta(op, a, span)?
            }
        })
    }

    /// Field `idx` of `o` (a struct, or a `Field`/`Parameter` while compiling).
    fn field(&mut self, o: &Value, idx: usize, span: Span) -> R<Value> {
        match o {
            Value::Struct(sv) => Ok(sv.fields.borrow()[idx].clone()),
            Value::Field(sid, i) => Ok(self.c.field_info(*sid, *i)[idx].clone()),
            Value::Param(fid, i) => Ok(self.c.param_info(*fid, *i)[idx].clone()),
            Value::Null => self.fail("internal error: a field of `null` while compiling", span),
            _ => self.fail("internal error: a field of a value that isn't a struct while compiling", span),
        }
    }

    fn meta(&mut self, op: &MetaOp, args: Vec<Value>, span: Span) -> R<Value> {
        Ok(match op {
            MetaOp::Fields(t) => match t {
                Type::Struct(sid) if !t.has_param(&self.c.struct_has_param) => list_val((0..self.c.structs[*sid].fields.len()).map(|i| Value::Field(*sid, i)).collect()),
                _ => return Err(Ctl::Unknown(span)),
            },
            MetaOp::Params(fid) => list_val((0..self.c.funcs[*fid].params.len()).map(|i| Value::Param(*fid, i)).collect()),
            MetaOp::AttrOf(None, _) => return Err(Ctl::Unknown(span)),
            MetaOp::AttrOf(Some(target), attr) => self.c.attr_value(*target, *attr, span)?,
            MetaOp::DescAttr(attr) | MetaOp::DescHas(attr) => {
                let target = match args.first() {
                    Some(Value::Field(sid, i)) => AttrTarget::Field(self.c.struct_origin[*sid], *i),
                    Some(Value::Param(fid, i)) => AttrTarget::Param(self.c.fn_decl_of(*fid), *i),
                    _ => return self.fail("internal error: `attribute` on something that isn't a field or parameter", span),
                };
                let v = self.c.attr_value(target, *attr, span)?;
                match op {
                    MetaOp::DescHas(_) => Value::Bool(!matches!(v, Value::Null)),
                    _ => v,
                }
            }
            MetaOp::CompileError => {
                let message = String::from_utf8_lossy(args.first().map(bytes).unwrap_or(&[])).into_owned();
                let at = args.get(1).and_then(|a| self.c.origin_of(a));
                return Err(Ctl::CompileError { message, at, raised: span });
            }
        })
    }

    pub(super) fn binary(&mut self, op: BinOp, ty: &Type, a: Value, b: Value, span: Span) -> R<Value> {
        use std::cmp::Ordering;
        let cmp = |o: Option<Ordering>| -> Value {
            match op {
                BinOp::Lt => Value::Bool(o == Some(Ordering::Less)),
                BinOp::Le => Value::Bool(matches!(o, Some(Ordering::Less | Ordering::Equal))),
                BinOp::Gt => Value::Bool(o == Some(Ordering::Greater)),
                BinOp::Ge => Value::Bool(matches!(o, Some(Ordering::Greater | Ordering::Equal))),
                _ => Value::Int(match o {
                    Some(Ordering::Less) => -1,
                    Some(Ordering::Greater) => 1,
                    _ => 0,
                }),
            }
        };
        if matches!(op, BinOp::Eq | BinOp::Ne) {
            let eq = match (&a, &b) {
                (Value::Null, Value::Null) => true,
                (Value::Null, _) | (_, Value::Null) => false,
                (Value::Int(x), Value::Int(y)) => x == y,
                (Value::Float(x), Value::Float(y)) => x == y,
                (Value::Bool(x), Value::Bool(y)) => x == y,
                (Value::Str(_) | Value::Bytes(_), Value::Str(_) | Value::Bytes(_)) => bytes(&a) == bytes(&b),
                (Value::Enum(s1, k1, _), Value::Enum(s2, k2, _)) => s1 == s2 && k1 == k2,
                (Value::Unit, Value::Unit) => true,
                _ => return self.fail(format!("internal error: `{}` on these values while compiling", op.symbol()), span),
            };
            return Ok(Value::Bool(eq == (op == BinOp::Eq)));
        }
        Ok(match (a, b) {
            (Value::Int(x), Value::Int(y)) => match op {
                BinOp::Add => Value::Int(x.checked_add(y).ok_or(()).or_else(|_| self.panic("integer overflow", span))?),
                BinOp::Sub => Value::Int(x.checked_sub(y).ok_or(()).or_else(|_| self.panic("integer overflow", span))?),
                BinOp::Mul => Value::Int(x.checked_mul(y).ok_or(()).or_else(|_| self.panic("integer overflow", span))?),
                BinOp::Div | BinOp::Mod => {
                    if y == 0 {
                        return self.panic("division by zero", span);
                    }
                    let r = if op == BinOp::Div { x.checked_div(y) } else { x.checked_rem(y) };
                    Value::Int(r.ok_or(()).or_else(|_| self.panic("integer overflow", span))?)
                }
                _ => cmp(Some(x.cmp(&y))),
            },
            (Value::Float(x), Value::Float(y)) => match op {
                BinOp::Add => Value::Float(x + y),
                BinOp::Sub => Value::Float(x - y),
                BinOp::Mul => Value::Float(x * y),
                BinOp::Div => Value::Float(x / y),
                BinOp::Mod => Value::Float(x % y),
                _ => cmp(x.partial_cmp(&y)),
            },
            (a @ (Value::Str(_) | Value::Bytes(_)), b) => match op {
                BinOp::Add => {
                    let mut v = bytes(&a).to_vec();
                    v.extend_from_slice(bytes(&b));
                    like(&a, &v)
                }
                _ => cmp(Some(bytes(&a).cmp(bytes(&b)))),
            },
            _ => {
                let tn = self.c.tn(ty);
                return self.fail(format!("internal error: `{}` on `{tn}` values while compiling", op.symbol()), span);
            }
        })
    }

    pub(super) fn to_str(&self, v: &Value) -> String {
        match v {
            Value::Int(n) => n.to_string(),
            Value::Float(x) => format_float(*x),
            Value::Bool(b) => b.to_string(),
            Value::Str(s) => s.to_string(),
            Value::Enum(sid, k, _) => self.c.structs[*sid].variants().get(*k).map(|v| v.name.clone()).unwrap_or_default(),
            Value::Null => "null".into(),
            _ => String::new(),
        }
    }

    fn list(&self, v: &Value, span: Span) -> R<Rc<RefCell<Vec<Value>>>> {
        match v {
            Value::List(l) => Ok(l.clone()),
            _ => self.fail("internal error: expected a list while compiling", span),
        }
    }

    fn map(&self, v: &Value, span: Span) -> R<Rc<RefCell<MapVal>>> {
        match v {
            Value::Map(m) => Ok(m.clone()),
            _ => self.fail("internal error: expected a map while compiling", span),
        }
    }

    fn builtin(&mut self, b: Builtin, a: Vec<Value>, ty: &Type, span: Span) -> R<Value> {
        let int = |i: usize| match a.get(i) {
            Some(Value::Int(n)) => *n,
            _ => 0,
        };
        let flt = |i: usize| match a.get(i) {
            Some(Value::Float(x)) => *x,
            _ => 0.0,
        };
        let s0 = || a.first().map(bytes).unwrap_or(&[]);
        let s1 = || a.get(1).map(bytes).unwrap_or(&[]);
        let strs = |items: Vec<&[u8]>, l: &Value| list_val(items.into_iter().map(|x| like(l, x)).collect());
        Ok(match b {
            Builtin::Print | Builtin::Input => return self.cannot("printing and reading input", span),
            Builtin::Args => return self.cannot("`args()`", span),
            Builtin::Exit => return self.cannot("`exit()`", span),
            Builtin::RealtimeNs | Builtin::MonotonicNs | Builtin::SleepNs => return self.cannot("the clock", span),
            Builtin::Fs(_) => return self.cannot("the file system", span),
            Builtin::Rt(i) => return self.rt(i, a, span),
            Builtin::HashOf | Builtin::HashCombine => return self.cannot("`hash()`", span),
            Builtin::Panic => return self.panic(String::from_utf8_lossy(s0()).into_owned(), span),
            Builtin::Assert => {
                if !matches!(a.first(), Some(Value::Bool(true))) {
                    return self.panic("assertion failed", span);
                }
                Value::Unit
            }
            Builtin::Sqrt => Value::Float(flt(0).sqrt()),
            Builtin::MathPow => Value::Float(flt(0).powf(flt(1))),
            Builtin::MathAtan2 => Value::Float(flt(0).atan2(flt(1))),
            Builtin::MathUnary(m) => {
                let x = flt(0);
                Value::Float(match m {
                    MathFn::Sin => x.sin(),
                    MathFn::Cos => x.cos(),
                    MathFn::Tan => x.tan(),
                    MathFn::Asin => x.asin(),
                    MathFn::Acos => x.acos(),
                    MathFn::Atan => x.atan(),
                    MathFn::Sinh => x.sinh(),
                    MathFn::Cosh => x.cosh(),
                    MathFn::Tanh => x.tanh(),
                    MathFn::Exp => x.exp(),
                    MathFn::Ln => x.ln(),
                    MathFn::Log2 => x.log2(),
                    MathFn::Log10 => x.log10(),
                    MathFn::Floor => x.floor(),
                    MathFn::Ceil => x.ceil(),
                    MathFn::Round => x.round(),
                    MathFn::Trunc => x.trunc(),
                    MathFn::Abs => x.abs(),
                    MathFn::Cbrt => x.cbrt(),
                })
            }
            Builtin::IntToFloat => Value::Float(int(0) as f64),
            Builtin::FloatToInt => Value::Int(flt(0) as i64),
            Builtin::StrToInt => match parse_int(s0()) {
                Some(n) => Value::Int(n),
                None => return self.panic(format!("cannot parse \"{}\" as Int", String::from_utf8_lossy(s0())), span),
            },
            Builtin::StrToFloat => match parse_float(s0()) {
                Some(x) => Value::Float(x),
                None => return self.panic(format!("cannot parse \"{}\" as Float", String::from_utf8_lossy(s0())), span),
            },
            Builtin::StrToIntOrNull => parse_int(s0()).map(Value::Int).unwrap_or(Value::Null),
            Builtin::StrToFloatOrNull => parse_float(s0()).map(Value::Float).unwrap_or(Value::Null),
            Builtin::StrToBoolOrNull => parse_bool(s0()).map(Value::Bool).unwrap_or(Value::Null),
            Builtin::StrChars => {
                let s = s0();
                let mut out = Vec::new();
                let mut i = 0;
                while i < s.len() {
                    let n = char_len_at(s, i);
                    out.push(&s[i..i + n]);
                    i += n;
                }
                strs(out, &a[0])
            }
            Builtin::StrCharCount => Value::Int(utf8_count(s0())),
            Builtin::StrCharAt => {
                let s = s0();
                let i = int(1) as usize;
                like(&a[0], &s[i..i + char_len_at(s, i)])
            }
            Builtin::StrSubstring => {
                let s = s0();
                let (start, end) = (int(1), int(2));
                if start < 0 || end < start || end > s.len() as i64 {
                    return self.panic(format!("substring({start}, {end}) out of range for a string of length {}", s.len()), span);
                }
                if !is_char_boundary(s, start) || !is_char_boundary(s, end) {
                    return self.panic(format!("substring({start}, {end}) would split a multi-byte character"), span);
                }
                like(&a[0], &s[start as usize..end as usize])
            }
            Builtin::StrContains => Value::Bool(find_from(s0(), s1(), 0).is_some()),
            Builtin::StrStartsWith => Value::Bool(s0().starts_with(s1())),
            Builtin::StrEndsWith => Value::Bool(s0().ends_with(s1())),
            Builtin::ToStr => str_val(&self.to_str(&a[0])),
            Builtin::StrLen => Value::Int(s0().len() as i64),
            Builtin::ListLen => Value::Int(match &a[0] {
                Value::List(l) => l.borrow().len() as i64,
                v => bytes(v).len() as i64,
            }),
            Builtin::ListPush => {
                self.list(&a[0], span)?.borrow_mut().push(a[1].clone());
                Value::Unit
            }
            Builtin::ListPop => match self.list(&a[0], span)?.borrow_mut().pop() {
                Some(v) => v,
                None => return self.panic("pop from empty list", span),
            },
            Builtin::Inc => Value::Int(int(0) + 1),
            Builtin::StrAppendOwned => {
                let mut v = s0().to_vec();
                v.extend_from_slice(s1());
                like(&a[0], &v)
            }
            Builtin::StrSplit => {
                let (s, sep) = (s0(), s1());
                if sep.is_empty() {
                    return self.panic("split() with an empty separator; use chars() to split into characters", span);
                }
                let mut out = Vec::new();
                let mut start = 0;
                while let Some(at) = find_from(s, sep, start) {
                    out.push(&s[start..at]);
                    start = at + sep.len();
                }
                out.push(&s[start..]);
                strs(out, &a[0])
            }
            Builtin::StrSplitWhitespace => {
                let s = s0();
                let mut out = Vec::new();
                let mut i = 0;
                while i < s.len() {
                    while i < s.len() && is_space(s[i]) {
                        i += 1;
                    }
                    let start = i;
                    while i < s.len() && !is_space(s[i]) {
                        i += 1;
                    }
                    if i > start {
                        out.push(&s[start..i]);
                    }
                }
                strs(out, &a[0])
            }
            Builtin::StrLines => {
                let s = s0();
                let mut out = Vec::new();
                let mut start = 0;
                for i in 0..s.len() {
                    if s[i] == b'\n' {
                        let end = if i > start && s[i - 1] == b'\r' { i - 1 } else { i };
                        out.push(&s[start..end]);
                        start = i + 1;
                    }
                }
                if start < s.len() {
                    out.push(&s[start..]);
                }
                strs(out, &a[0])
            }
            Builtin::StrTrim | Builtin::StrTrimStart | Builtin::StrTrimEnd => {
                let s = s0();
                let (mut x, mut y) = (0, s.len());
                if b != Builtin::StrTrimEnd {
                    while x < y && is_space(s[x]) {
                        x += 1;
                    }
                }
                if b != Builtin::StrTrimStart {
                    while y > x && is_space(s[y - 1]) {
                        y -= 1;
                    }
                }
                like(&a[0], &s[x..y])
            }
            Builtin::StrUpper => like(&a[0], &s0().to_ascii_uppercase()),
            Builtin::StrLower => like(&a[0], &s0().to_ascii_lowercase()),
            Builtin::StrReplace => {
                let (s, from) = (s0(), s1());
                let to = a.get(2).map(bytes).unwrap_or(&[]);
                if from.is_empty() {
                    return self.panic("replace() with an empty string to find", span);
                }
                let mut out = Vec::new();
                let mut start = 0;
                while let Some(at) = find_from(s, from, start) {
                    out.extend_from_slice(&s[start..at]);
                    out.extend_from_slice(to);
                    start = at + from.len();
                }
                out.extend_from_slice(&s[start..]);
                like(&a[0], &out)
            }
            Builtin::StrFind => find_from(s0(), s1(), 0).map(|i| Value::Int(i as i64)).unwrap_or(Value::Null),
            Builtin::StrRfind => {
                let (h, n) = (s0(), s1());
                let mut found = Value::Null;
                if n.len() <= h.len() {
                    for i in (0..=h.len() - n.len()).rev() {
                        if &h[i..i + n.len()] == n {
                            found = Value::Int(i as i64);
                            break;
                        }
                    }
                }
                found
            }
            Builtin::StrRepeat => {
                let n = int(1);
                if n < 0 {
                    return self.panic(format!("repeat({n}): the count can't be negative"), span);
                }
                if !s0().is_empty() && n > (1i64 << 40) / s0().len() as i64 {
                    return self.panic("repeat(): the result would be too large", span);
                }
                like(&a[0], &s0().repeat(n as usize))
            }
            Builtin::StrCount => {
                let (h, n) = (s0(), s1());
                if n.is_empty() {
                    Value::Int(h.len() as i64 + 1)
                } else {
                    let mut c = 0;
                    let mut at = find_from(h, n, 0);
                    while let Some(i) = at {
                        c += 1;
                        at = find_from(h, n, i + n.len());
                    }
                    Value::Int(c)
                }
            }
            Builtin::StrPadStart | Builtin::StrPadEnd => {
                let (s, width) = (s0(), int(1));
                let fill = a.get(2).map(bytes).unwrap_or(b" ");
                if utf8_count(fill) != 1 {
                    return self.panic("pad_start()/pad_end(): the fill must be exactly one character", span);
                }
                let have = utf8_count(s);
                if have >= width {
                    like(&a[0], s)
                } else {
                    let pad = fill.repeat((width - have) as usize);
                    let v = if b == Builtin::StrPadStart { [pad.as_slice(), s].concat() } else { [s, pad.as_slice()].concat() };
                    like(&a[0], &v)
                }
            }
            Builtin::ListJoin => {
                let l = self.list(&a[0], span)?;
                let parts: Vec<Vec<u8>> = l.borrow().iter().map(|x| bytes(x).to_vec()).collect();
                str_val(&String::from_utf8_lossy(&parts.join(s1())))
            }
            Builtin::ListInsert => {
                let l = self.list(&a[0], span)?;
                let i = int(1);
                let len = l.borrow().len() as i64;
                if i < 0 || i > len {
                    return self.panic(format!("insert index {i} out of bounds for list of length {len}"), span);
                }
                l.borrow_mut().insert(i as usize, a[2].clone());
                Value::Unit
            }
            Builtin::ListRemoveAt => {
                let l = self.list(&a[0], span)?;
                let i = int(1);
                let len = l.borrow().len() as i64;
                if i < 0 || i >= len {
                    return self.panic(format!("index {i} out of bounds for list of length {len}"), span);
                }
                l.borrow_mut().remove(i as usize)
            }
            Builtin::ListClear => {
                self.list(&a[0], span)?.borrow_mut().clear();
                Value::Unit
            }
            Builtin::ListReverse => {
                self.list(&a[0], span)?.borrow_mut().reverse();
                Value::Unit
            }
            Builtin::ListSort => {
                let l = self.list(&a[0], span)?;
                l.borrow_mut().sort_by(order);
                Value::Unit
            }
            Builtin::ListSortBy => {
                let l = self.list(&a[0], span)?;
                let keys = self.list(&a[1], span)?.borrow().clone();
                let mut pairs: Vec<(Value, Value)> = keys.into_iter().zip(l.borrow().iter().cloned()).collect();
                pairs.sort_by(|x, y| order(&x.0, &y.0));
                *l.borrow_mut() = pairs.into_iter().map(|(_, v)| v).collect();
                Value::Unit
            }
            Builtin::MapGet => {
                let m = self.map(&a[0], span)?;
                let k = self.key(&a[1], span)?;
                let found = m.borrow().get(&k).cloned();
                match found {
                    Some(v) => v,
                    None => return self.panic(self.missing_key(&a[1]), span),
                }
            }
            Builtin::MapGetOrNull => {
                let m = self.map(&a[0], span)?;
                let k = self.key(&a[1], span)?;
                m.borrow().get(&k).cloned().unwrap_or(Value::Null)
            }
            Builtin::MapGetOr => {
                let m = self.map(&a[0], span)?;
                let k = self.key(&a[1], span)?;
                m.borrow().get(&k).cloned().unwrap_or_else(|| a[2].clone())
            }
            Builtin::MapSet => {
                let m = self.map(&a[0], span)?;
                let k = self.key(&a[1], span)?;
                m.borrow_mut().set(k, a[1].clone(), a[2].clone());
                Value::Unit
            }
            Builtin::MapContains => match &a[0] {
                Value::Map(m) => {
                    let k = self.key(&a[1], span)?;
                    Value::Bool(m.borrow().get(&k).is_some())
                }
                // (`x in s` on strings.)
                _ => Value::Bool(find_from(s0(), s1(), 0).is_some()),
            },
            Builtin::MapRemove => {
                let m = self.map(&a[0], span)?;
                let k = self.key(&a[1], span)?;
                m.borrow_mut().remove(&k).unwrap_or(Value::Null)
            }
            Builtin::MapLen => Value::Int(self.map(&a[0], span)?.borrow().entries.len() as i64),
            Builtin::MapClear => {
                let m = self.map(&a[0], span)?;
                *m.borrow_mut() = MapVal::default();
                Value::Unit
            }
            Builtin::MapKeys => list_val(self.map(&a[0], span)?.borrow().entries.iter().map(|(k, _)| k.clone()).collect()),
            Builtin::MapValues => list_val(self.map(&a[0], span)?.borrow().entries.iter().map(|(_, v)| v.clone()).collect()),
            Builtin::MapNext => {
                let m = self.map(&a[0], span)?;
                let (i, len) = (int(1), int(2));
                let n = m.borrow().entries.len() as i64;
                if n != len {
                    return self.panic("map changed size during iteration", span);
                }
                Value::Int(if i < n { i } else { -1 })
            }
            Builtin::MapKeyAt => self.map(&a[0], span)?.borrow().entries[int(1) as usize].0.clone(),
            Builtin::MapValueAt => self.map(&a[0], span)?.borrow().entries[int(1) as usize].1.clone(),
            Builtin::StrRepr => str_val(&String::from_utf8_lossy(&repr(s0()))),
            Builtin::IsNull => Value::Bool(matches!(a[0], Value::Null)),
            Builtin::Wrap | Builtin::Unwrap => a[0].clone(),
        })
        .map(|v| {
            // (`Bytes` builtins share code with `String` ones: keep the kind asked for.)
            match (&v, ty) {
                (Value::Str(s), Type::Bytes) => Value::Bytes(Rc::from(s.as_bytes())),
                _ => v,
            }
        })
    }

    /// Runtime intrinsics of the standard library: the few that only compute.
    fn rt(&mut self, i: u16, a: Vec<Value>, span: Span) -> R<Value> {
        let name = crate::stdlib::RT[i as usize].0;
        let s0 = a.first().map(bytes).unwrap_or(&[]);
        Ok(match name {
            "str_to_bool" => match parse_bool(s0) {
                Some(b) => Value::Bool(b),
                None => {
                    let shown = String::from_utf8_lossy(s0);
                    return self.panic(format!("cannot parse \"{shown}\" as Bool (use true or false, yes or no, on or off, 1 or 0)"), span);
                }
            },
            "str_encode" => Value::Bytes(Rc::from(s0)),
            "bytes_decode" => match std::str::from_utf8(s0) {
                Ok(s) => str_val(s),
                Err(_) => return self.panic("decode(): the bytes are not valid UTF-8 (use decode_or_null() or decode_lossy())", span),
            },
            "bytes_decode_lossy" => str_val(&String::from_utf8_lossy(s0)),
            "bytes_hex" => str_val(&s0.iter().map(|b| format!("{b:02x}")).collect::<String>()),
            _ => {
                let shown = name.split('_').next().unwrap_or(name);
                return self.cannot(&format!("the `{shown}` module's system calls (`{name}`)"), span);
            }
        })
    }
}

/// How `sort` orders `Int`s, `Float`s and `String`s.
fn order(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        _ => bytes(a).cmp(bytes(b)),
    }
}
