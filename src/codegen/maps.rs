//! Code for maps (hash tables in the runtime, entries laid out here), and
//! the other built-ins added with them: string and list utilities, hashing,
//! math and files.

use super::*;

impl<'a> Codegen<'a> {
    /// The runtime's record for a map type: key kind, entry size, and for
    /// user-type keys the generated hash and comparison functions.
    pub(super) fn map_ti(&mut self, t: &Type) -> String {
        if !self.map_types.contains(t) {
            self.map_types.push(t.clone());
        }
        format!("@tarn.mapti.{}", mangle(t))
    }

    /// A key as a runtime call argument (`i64 k` or `ptr k`).
    fn key_arg(&mut self, k: &Type, v: &str) -> String {
        match k {
            Type::Bool => {
                let z = self.fresh();
                self.emit(format!("{z} = zext i1 {v} to i64"));
                format!("i64 {z}")
            }
            _ => format!("{} {v}", key_llty(k)),
        }
    }

    fn key_suffix(k: &Type) -> &'static str {
        if key_kind(k) == 0 { "i" } else { "p" }
    }

    /// Pointer to field `idx` (1: key, 2: value) of entry `e`.
    fn entry_field(&mut self, e: &str, k: &Type, v: &Type, idx: usize) -> String {
        let p = self.fresh();
        self.emit(format!("{p} = getelementptr inbounds {}, ptr {e}, i32 0, i32 {idx}", entry_ty(k, v)));
        p
    }

    fn load_key(&mut self, e: &str, k: &Type, v: &Type) -> String {
        let p = self.entry_field(e, k, v, 1);
        let raw = self.fresh();
        self.emit(format!("{raw} = load {}, ptr {p}", key_llty(k)));
        if *k == Type::Bool {
            let b = self.fresh();
            self.emit(format!("{b} = trunc i64 {raw} to i1"));
            return b;
        }
        raw
    }

    fn load_value(&mut self, e: &str, k: &Type, v: &Type) -> String {
        let p = self.entry_field(e, k, v, 2);
        let r = self.fresh();
        self.emit(format!("{r} = load {}, ptr {p}", llty(v)));
        r
    }

    /// The entry of `key` in map `m` (of type `mt`), or null.
    fn map_find(&mut self, m: &str, mt: &Type, key: &str) -> String {
        let (k, _) = map_kv(mt);
        let ti = self.map_ti(mt);
        let ka = self.key_arg(&k, key);
        let e = self.fresh();
        self.emit(format!("{e} = call ptr @tarn_map_find_{}(ptr {m}, ptr {ti}, {ka})", Self::key_suffix(&k)));
        e
    }

    /// A `V` as a `V?` (a nullable `V` or a reference stays as it is).
    fn wrap_value(&mut self, v: &Type, x: &str) -> String {
        if v.is_nullable() || is_ref(v) {
            return x.to_string();
        }
        let r = self.fresh();
        let inner = llty(v);
        let zero = match v {
            Type::Float => "0.0",
            Type::Bool => "false",
            _ => "0",
        };
        self.emit(format!("{r} = insertvalue {{ i1, {inner} }} {{ i1 true, {inner} {zero} }}, {inner} {x}, 1"));
        r
    }

    /// `null` of type `t?`.
    fn null_of(t: &Type) -> &'static str {
        if is_pair(&t.clone().nullable()) { "zeroinitializer" } else { "null" }
    }

    /// Two-way join: `cond` picks `then(self)` or `els(self)`, which emit code
    /// and return a value of LLVM type `ty` (empty for no value).
    fn select_blocks(&mut self, cond: &str, ty: &str, then: impl FnOnce(&mut Self) -> String, els: impl FnOnce(&mut Self) -> String) -> String {
        let (lt, le, ld) = (self.new_label("if.then"), self.new_label("if.else"), self.new_label("if.done"));
        self.emit_term(format!("br i1 {cond}, label %{lt}, label %{le}"));
        self.start_block(&lt);
        let a = then(self);
        let from_a = self.cur_label.clone();
        self.br(&ld);
        self.start_block(&le);
        let b = els(self);
        let from_b = self.cur_label.clone();
        self.br(&ld);
        self.start_block(&ld);
        if ty.is_empty() {
            return String::new();
        }
        let r = self.fresh();
        self.emit(format!("{r} = phi {ty} [ {a}, %{from_a} ], [ {b}, %{from_b} ]"));
        r
    }

    /// The key as text for "key ... not found" (or `null`), and whether to quote it.
    fn key_text(&mut self, k: &Type, key: &str) -> (String, i64) {
        match k {
            Type::Str => (key.to_string(), 1),
            Type::Int | Type::Bool | Type::Enum(_, false) => (self.emit_to_string(k, key), 0),
            _ => ("null".into(), 0),
        }
    }

    /// Built-ins that aren't handled in `builtin` itself.
    pub(super) fn extra_builtin(&mut self, b: Builtin, vals: &[String], tys: &[Type], span: Span) -> String {
        let loc = self.loc(span);
        let r = self.fresh();
        let call_i1 = |c: &mut Self, f: &str, args: &str| -> String {
            let raw = c.fresh();
            c.emit(format!("{raw} = call i64 @{f}({args})"));
            let r = c.fresh();
            c.emit(format!("{r} = icmp ne i64 {raw}, 0"));
            r
        };
        match b {
            // ---------- maps ----------
            Builtin::MapLen => {
                let p = self.fresh();
                self.emit(format!("{p} = getelementptr inbounds %TarnMap, ptr {}, i32 0, i32 1", vals[0]));
                self.emit(format!("{r} = load i64, ptr {p}"));
            }
            Builtin::MapGet => {
                let (k, v) = map_kv(&tys[0]);
                let e = self.map_find(&vals[0], &tys[0], &vals[1]);
                let miss = self.fresh();
                self.emit(format!("{miss} = icmp eq ptr {e}, null"));
                let (bad, ok) = (self.new_label("panic"), self.new_label("ok"));
                self.emit_term(format!("br i1 {miss}, label %{bad}, label %{ok}"));
                self.start_block(&bad);
                let (shown, quoted) = self.key_text(&k, &vals[1]);
                self.emit(format!("call void @tarn_panic_key(ptr {shown}, i64 {quoted}, {loc})"));
                self.emit_term("unreachable");
                self.start_block(&ok);
                let x = self.load_value(&e, &k, &v);
                if is_ref(&v) {
                    self.dup_value(&v, &x);
                }
                return x;
            }
            Builtin::MapGetOrNull | Builtin::MapGetOr => {
                let (k, v) = map_kv(&tys[0]);
                let e = self.map_find(&vals[0], &tys[0], &vals[1]);
                let hit = self.fresh();
                self.emit(format!("{hit} = icmp ne ptr {e}, null"));
                let or_null = b == Builtin::MapGetOrNull;
                let ty = if or_null { llty(&v.clone().nullable()) } else { llty(&v) };
                let default = vals.get(2).cloned();
                return self.select_blocks(
                    &hit,
                    ty,
                    |c| {
                        let x = c.load_value(&e, &k, &v);
                        if is_ref(&v) {
                            c.dup_value(&v, &x);
                        }
                        if or_null { c.wrap_value(&v, &x) } else { x }
                    },
                    |c| match default {
                        None => Self::null_of(&v).to_string(),
                        Some(d) => {
                            if is_ref(&v) {
                                c.dup_value(&v, &d);
                            }
                            d
                        }
                    },
                );
            }
            Builtin::MapContains => {
                let e = self.map_find(&vals[0], &tys[0], &vals[1]);
                self.emit(format!("{r} = icmp ne ptr {e}, null"));
            }
            Builtin::MapSet => {
                let (k, v) = map_kv(&tys[0]);
                let ti = self.map_ti(&tys[0]);
                let flag = format!("%isnew{}", self.tmp);
                let _ = writeln!(self.allocas, "  {flag} = alloca i64");
                let ka = self.key_arg(&k, &vals[1]);
                let e = self.fresh();
                self.emit(format!("{e} = call ptr @tarn_map_upsert_{}(ptr {}, ptr {ti}, {ka}, ptr {flag})", Self::key_suffix(&k), vals[0]));
                let isnew = self.fresh();
                self.emit(format!("{isnew} = load i64, ptr {flag}"));
                let c = self.fresh();
                self.emit(format!("{c} = icmp ne i64 {isnew}, 0"));
                let vp = self.entry_field(&e, &k, &v, 2);
                let (val, key) = (vals[2].clone(), vals[1].clone());
                let vl = llty(&v);
                self.select_blocks(
                    &c,
                    "",
                    |c| {
                        c.emit(format!("store {vl} {val}, ptr {vp}"));
                        String::new()
                    },
                    |c| {
                        // The map keeps its key; the new one and the old value go.
                        let old = c.fresh();
                        c.emit(format!("{old} = load {vl}, ptr {vp}"));
                        c.emit(format!("store {vl} {val}, ptr {vp}"));
                        if is_ref(&v) {
                            let rel = c.release_fn(&v);
                            c.emit(format!("call void {rel}(ptr {old})"));
                        }
                        if is_ref(&k) {
                            let rel = c.release_fn(&k);
                            c.emit(format!("call void {rel}(ptr {key})"));
                        }
                        String::new()
                    },
                );
                return String::new();
            }
            Builtin::MapRemove => {
                let (k, v) = map_kv(&tys[0]);
                let ti = self.map_ti(&tys[0]);
                let ka = self.key_arg(&k, &vals[1]);
                let e = self.fresh();
                self.emit(format!("{e} = call ptr @tarn_map_remove_{}(ptr {}, ptr {ti}, {ka})", Self::key_suffix(&k), vals[0]));
                let hit = self.fresh();
                self.emit(format!("{hit} = icmp ne ptr {e}, null"));
                let ty = llty(&v.clone().nullable());
                return self.select_blocks(
                    &hit,
                    ty,
                    |c| {
                        // The entry's references are ours now: drop the key, return the value.
                        if is_ref(&k) {
                            let key = c.load_key(&e, &k, &v);
                            let rel = c.release_fn(&k);
                            c.emit(format!("call void {rel}(ptr {key})"));
                        }
                        let x = c.load_value(&e, &k, &v);
                        c.wrap_value(&v, &x)
                    },
                    |_| Self::null_of(&v).to_string(),
                );
            }
            Builtin::MapClear => {
                let f = self.map_clear_fn(&tys[0]);
                self.emit(format!("call void {f}(ptr {})", vals[0]));
                return String::new();
            }
            Builtin::MapNext => {
                let ti = self.map_ti(&tys[0]);
                self.emit(format!("{r} = call i64 @tarn_map_next(ptr {}, ptr {ti}, i64 {}, i64 {}, {loc})", vals[0], vals[1], vals[2]));
            }
            Builtin::MapKeyAt | Builtin::MapValueAt => {
                let (k, v) = map_kv(&tys[0]);
                let ep = self.fresh();
                self.emit(format!("{ep} = getelementptr inbounds %TarnMap, ptr {}, i32 0, i32 4", vals[0]));
                let ents = self.fresh();
                self.emit(format!("{ents} = load ptr, ptr {ep}"));
                let e = self.fresh();
                self.emit(format!("{e} = getelementptr inbounds {}, ptr {ents}, i64 {}", entry_ty(&k, &v), vals[1]));
                let (x, t) = if b == Builtin::MapKeyAt { (self.load_key(&e, &k, &v), k) } else { (self.load_value(&e, &k, &v), v) };
                if is_ref(&t) {
                    self.dup_value(&t, &x);
                }
                return x;
            }
            Builtin::MapKeys | Builtin::MapValues => unreachable!("lowered to a loop"),

            // ---------- hashing ----------
            Builtin::HashOf => return self.hash_value(&tys[0], &vals[0]),
            Builtin::HashCombine => {
                let mut h = "0".to_string();
                for p in vals {
                    let n = self.fresh();
                    self.emit(format!("{n} = call i64 @tarn_hash_combine(i64 {h}, i64 {p})"));
                    h = n;
                }
                return h;
            }

            // ---------- strings ----------
            Builtin::StrRepr => self.emit(format!("{r} = call ptr @tarn_str_repr(ptr {})", vals[0])),
            Builtin::StrSplit => self.emit(format!("{r} = call ptr @tarn_str_split(ptr {}, ptr {}, {loc})", vals[0], vals[1])),
            Builtin::StrSplitWhitespace => self.emit(format!("{r} = call ptr @tarn_str_split_whitespace(ptr {})", vals[0])),
            Builtin::StrLines => self.emit(format!("{r} = call ptr @tarn_str_lines(ptr {})", vals[0])),
            Builtin::StrTrim | Builtin::StrTrimStart | Builtin::StrTrimEnd => {
                let (a, z) = match b {
                    Builtin::StrTrim => (1, 1),
                    Builtin::StrTrimStart => (1, 0),
                    _ => (0, 1),
                };
                self.emit(format!("{r} = call ptr @tarn_str_trim(ptr {}, i64 {a}, i64 {z})", vals[0]));
            }
            Builtin::StrUpper | Builtin::StrLower => {
                let up = i64::from(b == Builtin::StrUpper);
                self.emit(format!("{r} = call ptr @tarn_str_case(ptr {}, i64 {up})", vals[0]));
            }
            Builtin::StrReplace => self.emit(format!("{r} = call ptr @tarn_str_replace(ptr {}, ptr {}, ptr {}, {loc})", vals[0], vals[1], vals[2])),
            Builtin::StrFind | Builtin::StrRfind => {
                let f = if b == Builtin::StrFind { "tarn_str_find" } else { "tarn_str_rfind" };
                let at = self.fresh();
                self.emit(format!("{at} = call i64 @{f}(ptr {}, ptr {})", vals[0], vals[1]));
                let has = self.fresh();
                self.emit(format!("{has} = icmp sge i64 {at}, 0"));
                let half = self.fresh();
                self.emit(format!("{half} = insertvalue {{ i1, i64 }} undef, i1 {has}, 0"));
                self.emit(format!("{r} = insertvalue {{ i1, i64 }} {half}, i64 {at}, 1"));
            }
            Builtin::StrCount => self.emit(format!("{r} = call i64 @tarn_str_count(ptr {}, ptr {})", vals[0], vals[1])),
            Builtin::StrRepeat => self.emit(format!("{r} = call ptr @tarn_str_repeat(ptr {}, i64 {}, {loc})", vals[0], vals[1])),
            Builtin::StrPadStart | Builtin::StrPadEnd => {
                let start = i64::from(b == Builtin::StrPadStart);
                self.emit(format!("{r} = call ptr @tarn_str_pad(ptr {}, i64 {}, ptr {}, i64 {start}, {loc})", vals[0], vals[1], vals[2]));
            }

            // ---------- lists ----------
            Builtin::ListJoin => self.emit(format!("{r} = call ptr @tarn_str_list_join(ptr {}, ptr {})", vals[0], vals[1])),
            Builtin::ListInsert => {
                let elem = list_elem(&tys[0]);
                self.emit(format!("call void @tarn_list_insert_slot_check(ptr {}, i64 {}, {loc})", vals[0], vals[1]));
                let slot = self.fresh();
                self.emit(format!("{slot} = call ptr @tarn_list_insert_slot(ptr {}, i64 {}, i64 {})", vals[0], vals[1], elem_size(&elem)));
                let md = self.elem_tbaa(&elem);
                self.emit(format!("store {} {}, ptr {slot}{md}", llty(&elem), vals[2]));
                return String::new();
            }
            Builtin::ListRemoveAt => {
                let elem = list_elem(&tys[0]);
                let out = format!("%removed{}", self.tmp);
                let _ = writeln!(self.allocas, "  {out} = alloca {}", llty(&elem));
                self.emit(format!("call void @tarn_list_remove_at(ptr {}, i64 {}, i64 {}, ptr {out}, {loc})", vals[0], vals[1], elem_size(&elem)));
                self.emit(format!("{r} = load {}, ptr {out}", llty(&elem)));
            }
            Builtin::ListClear => {
                let elem = list_elem(&tys[0]);
                if is_ref(&elem) {
                    let f = self.list_clear_fn(&tys[0]);
                    self.emit(format!("call void {f}(ptr {})", vals[0]));
                } else {
                    let lp = self.fresh();
                    self.emit(format!("{lp} = getelementptr inbounds %TarnList, ptr {}, i32 0, i32 1", vals[0]));
                    let md = self.tbaa("list.len");
                    self.emit(format!("store i64 0, ptr {lp}{md}"));
                }
                return String::new();
            }
            Builtin::ListReverse => {
                let elem = list_elem(&tys[0]);
                self.emit(format!("call void @tarn_list_reverse(ptr {}, i64 {})", vals[0], elem_size(&elem)));
                return String::new();
            }
            Builtin::ListSort => {
                let kind = match list_elem(&tys[0]) {
                    Type::Int => 0,
                    Type::Float => 1,
                    _ => 2,
                };
                self.emit(format!("call void @tarn_list_sort(ptr {}, i64 {kind})", vals[0]));
                return String::new();
            }

            // ---------- math ----------
            Builtin::MathUnary(f) => {
                use crate::tast::MathFn::*;
                let callee = match f {
                    Sin => "llvm.sin.f64",
                    Cos => "llvm.cos.f64",
                    Tan => "tan",
                    Asin => "asin",
                    Acos => "acos",
                    Atan => "atan",
                    Sinh => "sinh",
                    Cosh => "cosh",
                    Tanh => "tanh",
                    Exp => "llvm.exp.f64",
                    Ln => "llvm.log.f64",
                    Log2 => "llvm.log2.f64",
                    Log10 => "llvm.log10.f64",
                    Floor => "llvm.floor.f64",
                    Ceil => "llvm.ceil.f64",
                    Round => "llvm.round.f64",
                    Trunc => "llvm.trunc.f64",
                    Abs => "llvm.fabs.f64",
                    Cbrt => "cbrt",
                };
                self.emit(format!("{r} = call double @{callee}(double {})", vals[0]));
            }
            Builtin::MathPow => self.emit(format!("{r} = call double @llvm.pow.f64(double {}, double {})", vals[0], vals[1])),
            Builtin::MathAtan2 => self.emit(format!("{r} = call double @atan2(double {}, double {})", vals[0], vals[1])),

            // ---------- files ----------
            Builtin::Fs(op) => {
                use crate::tast::FsOp::*;
                match op {
                    Read => self.emit(format!("{r} = call ptr @tarn_fs_read(ptr {})", vals[0])),
                    Write => {
                        let z = self.fresh();
                        self.emit(format!("{z} = zext i1 {} to i64", vals[2]));
                        return call_i1(self, "tarn_fs_write", &format!("ptr {}, ptr {}, i64 {z}", vals[0], vals[1]));
                    }
                    Kind => self.emit(format!("{r} = call i64 @tarn_fs_kind(ptr {})", vals[0])),
                    ListDir => self.emit(format!("{r} = call ptr @tarn_fs_list_dir(ptr {})", vals[0])),
                    MakeDir => return call_i1(self, "tarn_fs_make_dir", &format!("ptr {}", vals[0])),
                    Remove => return call_i1(self, "tarn_fs_remove", &format!("ptr {}", vals[0])),
                    Rename => return call_i1(self, "tarn_fs_rename", &format!("ptr {}, ptr {}", vals[0], vals[1])),
                    LastError => self.emit(format!("{r} = call ptr @tarn_fs_last_error()")),
                    Cwd => self.emit(format!("{r} = call ptr @tarn_fs_cwd()")),
                    ReadStdin => self.emit(format!("{r} = call ptr @tarn_fs_read_stdin()")),
                }
            }
            _ => unreachable!("built-in {b:?} is handled in `builtin`"),
        }
        r
    }

    /// `hash(x)` for a value of a type with a built-in hash (or `T?` of one: `null` hashes to 0).
    fn hash_value(&mut self, t: &Type, v: &str) -> String {
        let r = self.fresh();
        match t {
            Type::Int | Type::Enum(_, false) => self.emit(format!("{r} = call i64 @tarn_hash_int(i64 {v})")),
            Type::Bool => {
                let z = self.fresh();
                self.emit(format!("{z} = zext i1 {v} to i64"));
                self.emit(format!("{r} = call i64 @tarn_hash_int(i64 {z})"));
            }
            Type::Float => self.emit(format!("{r} = call i64 @tarn_hash_float(double {v})")),
            Type::Str => self.emit(format!("{r} = call i64 @tarn_hash_str(ptr {v})")),
            Type::Nullable(inner) if is_pair(t) => {
                let has = self.fresh();
                self.emit(format!("{has} = extractvalue {} {v}, 0", llty(t)));
                let x = self.fresh();
                self.emit(format!("{x} = extractvalue {} {v}, 1", llty(t)));
                let h = self.hash_value(inner, &x);
                self.emit(format!("{r} = select i1 {has}, i64 {h}, i64 0"));
            }
            Type::Nullable(inner) => {
                let has = self.fresh();
                self.emit(format!("{has} = icmp ne ptr {v}, null"));
                let inner = (**inner).clone();
                let v = v.to_string();
                return self.select_blocks(&has, "i64", |c| c.hash_value(&inner, &v), |_| "0".into());
            }
            _ => unreachable!("no built-in hash for {t:?}"),
        }
        r
    }

    /// `clear()` of a map type: release every key and value, then empty it.
    fn map_clear_fn(&mut self, t: &Type) -> String {
        self.map_ti(t);
        format!("@tarn.mapclear.{}", mangle(t))
    }

    fn list_clear_fn(&mut self, t: &Type) -> String {
        if !self.list_clears.contains(t) {
            self.list_clears.push(t.clone());
        }
        format!("@tarn.listclear.{}", mangle(t))
    }

    /// The body of a `void (ptr %p, ...)` function running `action` (which can
    /// use `%e`, the entry) for every live entry of map `%p`.
    fn map_each(k: &Type, v: &Type, action: &str) -> String {
        let et = entry_ty(k, v);
        format!(
            "entry:\n  %usedp = getelementptr inbounds %TarnMap, ptr %p, i32 0, i32 2\n  %used = load i64, ptr %usedp\n  %entsp = getelementptr inbounds %TarnMap, ptr %p, i32 0, i32 4\n  %ents = load ptr, ptr %entsp\n  br label %cond\ncond:\n  %i = phi i64 [ 0, %entry ], [ %i1, %next ]\n  %c = icmp slt i64 %i, %used\n  br i1 %c, label %body, label %done\nbody:\n  %e = getelementptr inbounds {et}, ptr %ents, i64 %i\n  %h = load i64, ptr %e\n  %live = icmp sge i64 %h, 0\n  br i1 %live, label %act, label %next\nact:\n{action}  br label %next\nnext:\n  %i1 = add i64 %i, 1\n  br label %cond\ndone:\n"
        )
    }

    /// Code (for `map_each`) that loads the key (`%k`) and/or value (`%v`) of
    /// entry `%e` and passes those of a reference type to `f(type, name)`.
    fn entry_refs(&mut self, k: &Type, v: &Type, mut f: impl FnMut(&mut Self, &Type, &str) -> Option<String>) -> String {
        let et = entry_ty(k, v);
        let mut out = String::new();
        for (idx, t, name) in [(1, k, "%k"), (2, v, "%v")] {
            if !is_ref(t) {
                continue;
            }
            if let Some(code) = f(self, t, name) {
                let _ = writeln!(out, "  {name}p = getelementptr inbounds {et}, ptr %e, i32 0, i32 {idx}\n  {name} = load ptr, ptr {name}p\n{code}");
            }
        }
        out
    }

    /// `drop` (and the cycle collector's functions, for a cyclic map type).
    pub(super) fn map_rc_functions(&mut self, t: &Type, cyclic: bool, out: &mut String) {
        let m = mangle(t);
        let (k, v) = map_kv(t);
        let release_all = self.entry_refs(&k, &v, |c, ty, name| {
            let rel = c.release_fn(ty);
            Some(format!("  call void {rel}(ptr {name})"))
        });
        if !cyclic {
            if is_ref(&k) || is_ref(&v) {
                let _ = writeln!(
                    out,
                    "define internal void @tarn.drop.{m}(ptr %p) #0 {{\n{}  call void @tarn_map_free(ptr %p)\n  ret void\n}}\n",
                    Self::map_each(&k, &v, &release_all)
                );
            }
            return;
        }
        let trace = self.entry_refs(&k, &v, |c, ty, name| {
            c.is_cyclic(ty).then(|| format!("  call void %visit(ptr {name}, ptr @tarn.ti.{})", mangle(ty.non_null())))
        });
        let acyclic = self.entry_refs(&k, &v, |c, ty, name| {
            (!c.is_cyclic(ty)).then(|| {
                let rel = c.release_fn(ty);
                format!("  call void {rel}(ptr {name})")
            })
        });
        let _ = writeln!(out, "define internal void @tarn.trace.{m}(ptr %p, ptr %visit) #0 {{\n{}  ret void\n}}\n", Self::map_each(&k, &v, &trace));
        let _ = writeln!(
            out,
            "define internal void @tarn.drop.{m}(ptr %p) #0 {{\n{}  call void @tarn_cyclic_free(ptr %p, ptr @tarn.ti.{m})\n  ret void\n}}\n",
            Self::map_each(&k, &v, &release_all)
        );
        let _ = writeln!(out, "define internal void @tarn.white.{m}(ptr %p) #0 {{\n{}  call void @tarn.mem.{m}(ptr %p)\n  ret void\n}}\n", Self::map_each(&k, &v, &acyclic));
        let _ = writeln!(out, "define internal void @tarn.mem.{m}(ptr %p) #0 {{\n  call void @tarn_map_free(ptr %p)\n  ret void\n}}\n");
    }

    /// Type records of map types, hash/compare functions for user-type keys,
    /// and `clear()` functions. Runs after `rc_functions`, but may add release
    /// functions, so those are generated again for anything new.
    pub(super) fn map_functions(&mut self) -> String {
        let mut out = String::new();
        let mut keys_done: Vec<Type> = Vec::new();
        let mut i = 0;
        while i < self.map_types.len() {
            let t = self.map_types[i].clone();
            i += 1;
            let m = mangle(&t);
            let (k, v) = map_kv(&t);
            let kind = key_kind(&k);
            let (hash, eq) = if kind == 2 {
                let km = mangle(&k);
                if !keys_done.contains(&k) {
                    keys_done.push(k.clone());
                    out.push_str(&self.key_functions(&k));
                }
                (format!("@tarn.keyhash.{km}"), format!("@tarn.keyeq.{km}"))
            } else {
                ("null".into(), "null".into())
            };
            let _ = writeln!(
                out,
                "@tarn.mapti.{m} = internal constant {{ i64, i64, ptr, ptr }} {{ i64 {kind}, i64 ptrtoint (ptr getelementptr ({}, ptr null, i32 1) to i64), ptr {hash}, ptr {eq} }}\n",
                entry_ty(&k, &v)
            );
            let release_all = self.entry_refs(&k, &v, |c, ty, name| {
                let rel = c.release_fn(ty);
                Some(format!("  call void {rel}(ptr {name})"))
            });
            let _ = writeln!(
                out,
                "define internal void @tarn.mapclear.{m}(ptr %p) #0 {{\n{}  call void @tarn_map_reset(ptr %p)\n  ret void\n}}\n",
                Self::map_each(&k, &v, &release_all)
            );
        }
        for t in self.list_clears.clone() {
            let elem = list_elem(&t);
            let rel = self.release_fn(&elem);
            let _ = writeln!(
                out,
                "define internal void @tarn.listclear.{}(ptr %p) #0 {{\nentry:\n  %lenp = getelementptr inbounds %TarnList, ptr %p, i32 0, i32 1\n  %len = load i64, ptr %lenp\n  %dp = getelementptr inbounds %TarnList, ptr %p, i32 0, i32 3\n  %data = load ptr, ptr %dp\n  store i64 0, ptr %lenp\n  br label %cond\ncond:\n  %i = phi i64 [ 0, %entry ], [ %i1, %body ]\n  %c = icmp slt i64 %i, %len\n  br i1 %c, label %body, label %done\nbody:\n  %ep = getelementptr inbounds ptr, ptr %data, i64 %i\n  %e = load ptr, ptr %ep\n  call void {rel}(ptr %e)\n  %i1 = add i64 %i, 1\n  br label %cond\ndone:\n  ret void\n}}\n",
                mangle(&t)
            );
        }
        // Release functions first needed here.
        out.push_str(&self.rc_functions());
        out
    }

    /// `@tarn.keyhash.K(ptr) -> i64` and `@tarn.keyeq.K(ptr stored, ptr key) -> i64`
    /// for map keys of a user type, calling its `fn hash` and `fn ==`.
    fn key_functions(&mut self, k: &Type) -> String {
        let km = mangle(k);
        let sid = match k {
            Type::Struct(s) | Type::Enum(s, _) => *s,
            _ => unreachable!(),
        };
        let Some(mk) = self.prog.map_keys.iter().find(|m| m.sid == sid).copied() else {
            // No map with these keys is ever built (only the type is mentioned).
            return format!(
                "define internal i64 @tarn.keyhash.{km}(ptr %k) #0 {{\n  ret i64 0\n}}\n\ndefine internal i64 @tarn.keyeq.{km}(ptr %a, ptr %b) #0 {{\n  ret i64 0\n}}\n\n"
            );
        };
        let caller = |f: &Func| if f.track_caller { ", ptr @.file.0, i32 0, i32 0" } else { "" };
        let hf = &self.prog.funcs[mk.hash];
        let ef = &self.prog.funcs[mk.eq];
        let (hsym, hloc, hown) = (hf.symbol.clone(), caller(hf), hf.param_owned.first().copied().unwrap_or(false));
        let (esym, eloc) = (ef.symbol.clone(), caller(ef));
        let (eown_a, eown_b) = (ef.param_owned.first().copied().unwrap_or(false), ef.param_owned.get(1).copied().unwrap_or(false));
        let inc = |c: &mut Self, p: &str| -> String {
            let md = c.tbaa("rc");
            let step = if c.is_cyclic(k) { "8" } else { "1" };
            let mask = if c.is_cyclic(k) { format!("\n  {p}.m = and i64 {p}.n, -4") } else { String::new() };
            let new = if c.is_cyclic(k) { format!("{p}.m") } else { format!("{p}.n") };
            format!("  {p}.c = load i64, ptr {p}{md}\n  {p}.n = add i64 {p}.c, {step}{mask}\n  store i64 {new}, ptr {p}{md}\n")
        };
        let rel = self.release_fn(k);
        let mut out = String::new();
        let hdup = if hown { inc(self, "%k") } else { String::new() };
        let _ = writeln!(out, "define internal i64 @tarn.keyhash.{km}(ptr %k) #0 {{\n{hdup}  %h = call i64 @{esc}(ptr %k{hloc})\n  ret i64 %h\n}}\n", esc = hsym);
        // The stored key is held for the call, so `==` can't free it even if it changes the map.
        let adup = inc(self, "%a");
        let bdup = if eown_b { inc(self, "%b") } else { String::new() };
        let arel = if eown_a { String::new() } else { format!("  call void {rel}(ptr %a)\n") };
        let _ = writeln!(
            out,
            "define internal i64 @tarn.keyeq.{km}(ptr %a, ptr %b) #0 {{\n{adup}{bdup}  %r = call i1 @{esym}(ptr %a, ptr %b{eloc})\n{arel}  %z = zext i1 %r to i64\n  ret i64 %z\n}}\n"
        );
        out
    }
}
