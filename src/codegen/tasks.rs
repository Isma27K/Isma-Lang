//! Tasks, channels and mutexes in the generated code.
//!
//! Tasks share no objects, so a value that goes to another task (captured
//! by `spawn`, sent on a channel, put in or taken out of a `Mutex`, a task's
//! result) goes through `@tarn.xfer.<type>(ptr value, ptr memo)`: an object
//! nobody else references is *moved* as it is (its children are walked in
//! turn, in place), and one this task still references is *copied* (the
//! copy then holds moved or copied children). Copies made in one walk are
//! remembered in `memo`, so parts reached twice stay shared and cycles stay
//! cycles. Immortal objects (string literals, data-less variants, function
//! constants) never change, so they go as they are. Handles (`Type::Handle`)
//! are the shared objects themselves: they go as they are, and their counts
//! are atomic.
//!
//! The runtime calls back into code generated here: a task's body
//! (`@tarn.task.run.<R>`), and what runs while a Mutex is held
//! (`@tarn.lock.run.<T>.<R>`, `update`, `get`, `set`).

use super::*;

/// Bytes of a slot holding one value of type `t` (a channel's, a mutex's, a task's result).
fn slot_size(t: &Type) -> u64 {
    if is_pair(t) { 16 } else { 8 }
}

impl<'a> Codegen<'a> {
    /// `@tarn.xfer.<m>` for reference type `t` (made once).
    pub(super) fn xfer_fn(&mut self, t: &Type) -> String {
        if !self.xfer_types.contains(t) {
            self.xfer_types.push(t.clone());
        }
        format!("@tarn.xfer.{}", mangle(t))
    }

    /// `@tarn.dup.<m>`: one more reference to a value of reference type `t` (null-safe for `T?`).
    pub(super) fn dup_fn(&mut self, t: &Type) -> String {
        if !self.dup_types.contains(t) {
            self.dup_types.push(t.clone());
        }
        format!("@tarn.dup.{}", mangle(t))
    }

    /// `@tarn.slot.release.<m>`, which releases the value in a slot (for the
    /// runtime), or `null` for a type without references.
    fn slot_release_fn(&mut self, t: &Type) -> String {
        if !is_ref(t) {
            return "null".into();
        }
        if !self.slot_types.contains(t) {
            self.slot_types.push(t.clone());
        }
        format!("@tarn.slot.release.{}", mangle(t))
    }

    /// An `alloca` of a slot in the current function's entry block.
    fn slot_alloca(&mut self) -> String {
        let a = self.fresh();
        let _ = writeln!(self.allocas, "  {a} = alloca [2 x i64], align 8");
        a
    }

    /// Move or copy `v` (of type `t`, owned) for another task, in the
    /// current function: the result is owned, and shares nothing with this task.
    pub(super) fn emit_xfer(&mut self, t: &Type, v: &str, span: Span) -> String {
        if !is_ref(t) {
            return v.to_string();
        }
        let memo = self.fresh();
        let _ = writeln!(self.allocas, "  {memo} = alloca ptr");
        self.emit(format!("store ptr null, ptr {memo}"));
        let loc = self.loc(span);
        self.emit(format!("call void @tarn_xfer_at({loc})"));
        let f = self.xfer_fn(t);
        let r = self.fresh();
        self.emit(format!("{r} = call ptr {f}(ptr {v}, ptr {memo})"));
        self.emit(format!("call void @tarn_xfer_done(ptr {memo})"));
        r
    }

    /// The code of a `SyncOp` (see `tast::SyncOp` for what each takes).
    pub(super) fn sync(&mut self, op: SyncOp, types: &[Type], args: &[Operand], span: Span) -> String {
        let vals: Vec<String> = args.iter().map(|a| self.operand(a)).collect();
        let loc = self.loc(span);
        match op {
            SyncOp::Spawn => {
                let fty = self.f.operand_ty(&args[0]);
                let f = self.emit_xfer(&fty, &vals[0], span);
                let run = self.task_run_fn(&types[0]);
                let rel = self.release_fn(&fty);
                // A function returning nothing gives the `Unit` value (a number).
                let rel_result = if types[0] == Type::Unit { "null".into() } else { self.slot_release_fn(&types[0]) };
                let h = self.fresh();
                self.emit(format!("{h} = call ptr @tarn_task_spawn(ptr {f}, ptr {run}, ptr {rel}, ptr {rel_result})"));
                h
            }
            SyncOp::TaskTake => {
                let t = &types[0];
                let slot = self.slot_alloca();
                self.emit(format!("call void @tarn_task_take(ptr {}, ptr {slot}, {loc})", vals[0]));
                let v = self.fresh();
                self.emit(format!("{v} = load {}, ptr {slot}", llty(t)));
                self.emit_xfer(t, &v, span)
            }
            SyncOp::ChanNew => {
                let t = &types[0];
                let rel = self.slot_release_fn(t);
                let h = self.fresh();
                self.emit(format!("{h} = call ptr @tarn_chan_new(i64 {}, i64 {}, ptr {rel}, {loc})", vals[0], slot_size(t)));
                h
            }
            SyncOp::ChanSend => {
                let t = &types[0];
                let v = self.emit_xfer(t, &vals[1], span);
                let slot = self.slot_alloca();
                self.emit(format!("store {} {v}, ptr {slot}", llty(t)));
                self.emit(format!("call void @tarn_chan_send(ptr {}, ptr {slot}, {loc})", vals[0]));
                String::new()
            }
            SyncOp::ChanReceive => {
                let t = types[0].clone();
                let slot = self.slot_alloca();
                let ok = self.fresh();
                self.emit(format!("{ok} = call i64 @tarn_chan_receive(ptr {}, ptr {slot}, {loc})", vals[0]));
                let okb = self.fresh();
                self.emit(format!("{okb} = icmp ne i64 {ok}, 0"));
                let lt = llty(&t);
                let v = self.fresh();
                self.emit(format!("{v} = load {lt}, ptr {slot}"));
                // The value as a `T?`: null once the channel is closed and empty.
                let opt = t.clone().nullable();
                let r = self.fresh();
                if is_ref(&t) || t.is_nullable() {
                    let null = if is_pair(&t) { "zeroinitializer" } else { "null" };
                    self.emit(format!("{r} = select i1 {okb}, {lt} {v}, {lt} {null}"));
                } else {
                    let ot = llty(&opt);
                    let (z, one) = (self.fresh(), self.fresh());
                    self.emit(format!("{z} = insertvalue {ot} zeroinitializer, i1 {okb}, 0"));
                    self.emit(format!("{one} = insertvalue {ot} {z}, {lt} {v}, 1"));
                    self.emit(format!("{r} = select i1 {okb}, {ot} {one}, {ot} zeroinitializer"));
                }
                r
            }
            SyncOp::MutexNew => {
                let t = &types[0];
                let v = self.emit_xfer(t, &vals[0], span);
                let slot = self.slot_alloca();
                self.emit(format!("store {} {v}, ptr {slot}", llty(t)));
                let rel = self.slot_release_fn(t);
                let h = self.fresh();
                self.emit(format!("{h} = call ptr @tarn_mutex_new(ptr {slot}, i64 {}, ptr {rel})", slot_size(t)));
                h
            }
            SyncOp::MutexLock | SyncOp::MutexUpdate => {
                let fty = self.f.operand_ty(&args[1]);
                let f = self.emit_xfer(&fty, &vals[1], span);
                let rel = self.release_fn(&fty);
                let run = if op == SyncOp::MutexLock { self.lock_fn("run", &types[0], Some(&types[1])) } else { self.lock_fn("update", &types[0], None) };
                let out = self.slot_alloca();
                self.emit(format!("call void @tarn_mutex_run(ptr {}, ptr {f}, ptr {rel}, ptr {run}, ptr {out}, {loc})", vals[0]));
                match types.get(1) {
                    Some(r) if op == SyncOp::MutexLock && *r != Type::Unit => {
                        let v = self.fresh();
                        self.emit(format!("{v} = load {}, ptr {out}", llty(r)));
                        v
                    }
                    _ => String::new(),
                }
            }
            SyncOp::MutexGet => {
                let t = &types[0];
                let run = self.lock_fn("get", t, None);
                let out = self.slot_alloca();
                self.emit(format!("call void @tarn_mutex_run(ptr {}, ptr null, ptr null, ptr {run}, ptr {out}, {loc})", vals[0]));
                let v = self.fresh();
                self.emit(format!("{v} = load {}, ptr {out}", llty(t)));
                v
            }
            SyncOp::MutexSet => {
                let t = &types[0];
                let v = self.emit_xfer(t, &vals[1], span);
                let run = self.lock_fn("set", t, None);
                let out = self.slot_alloca();
                self.emit(format!("store {} {v}, ptr {out}", llty(t)));
                self.emit(format!("call void @tarn_mutex_run(ptr {}, ptr null, ptr null, ptr {run}, ptr {out}, {loc})", vals[0]));
                String::new()
            }
        }
    }

    /// `@tarn.task.run.<R>(ptr closure, ptr out)`: call the task's function
    /// value and store what it returns.
    fn task_run_fn(&mut self, r: &Type) -> String {
        let name = format!("@tarn.task.run.{}", if *r == Type::Unit { "unit".into() } else { mangle(r) });
        if self.sync_helpers.iter().any(|(n, _)| *n == name) {
            return name;
        }
        let call = if *r == Type::Unit {
            "  call void %code(ptr %c)\n  store i64 0, ptr %out\n".to_string()
        } else {
            format!("  %r = call {lt} %code(ptr %c)\n  store {lt} %r, ptr %out\n", lt = llty(r))
        };
        let body = format!("define internal void {name}(ptr %c, ptr %out) #3 {{\n  %cp = getelementptr inbounds i8, ptr %c, i64 16\n  %code = load ptr, ptr %cp\n{call}  ret void\n}}\n");
        self.sync_helpers.push((name.clone(), body));
        name
    }

    /// What runs while a Mutex of `T`s is held: `run` calls the function
    /// value on the value (and moves or copies the result `R` out),
    /// `update` replaces the value with the function's result, `get` copies
    /// it out, `set` puts the value given in `out` in its place.
    fn lock_fn(&mut self, what: &str, t: &Type, r: Option<&Type>) -> String {
        let rn = r.map(|r| if *r == Type::Unit { "unit".to_string() } else { mangle(r) }).unwrap_or_default();
        let name = format!("@tarn.lock.{what}.{}{}", mangle_any(t), if rn.is_empty() { String::new() } else { format!(".{rn}") });
        if self.sync_helpers.iter().any(|(n, _)| *n == name) {
            return name;
        }
        let lt = llty(t);
        let mut b = String::new();
        let _ = writeln!(b, "define internal void {name}(ptr %c, ptr %slot, ptr %out) #3 {{");
        let xfer_out = |c: &mut Self, ty: &Type, v: &str, b: &mut String| -> String {
            if !is_ref(ty) {
                return v.to_string();
            }
            let f = c.xfer_fn(ty);
            let _ = writeln!(b, "  %memo = alloca ptr\n  store ptr null, ptr %memo\n  %moved = call ptr {f}(ptr {v}, ptr %memo)\n  call void @tarn_xfer_done(ptr %memo)");
            "%moved".to_string()
        };
        match what {
            "run" | "update" => {
                let _ = writeln!(b, "  %v = load {lt}, ptr %slot\n  %cp = getelementptr inbounds i8, ptr %c, i64 16\n  %code = load ptr, ptr %cp");
                if what == "update" {
                    let _ = writeln!(b, "  %new = call {lt} %code(ptr %c, {lt} %v)\n  store {lt} %new, ptr %slot");
                    if is_ref(t) {
                        let rel = self.release_fn(t);
                        let _ = writeln!(b, "  call void {rel}(ptr %v)");
                    }
                } else {
                    let r = r.cloned().unwrap_or(Type::Unit);
                    if r == Type::Unit {
                        let _ = writeln!(b, "  call void %code(ptr %c, {lt} %v)");
                    } else {
                        let rt = llty(&r);
                        let _ = writeln!(b, "  %r = call {rt} %code(ptr %c, {lt} %v)");
                        let out = xfer_out(self, &r, "%r", &mut b);
                        let _ = writeln!(b, "  store {rt} {out}, ptr %out");
                    }
                }
            }
            "get" => {
                let _ = writeln!(b, "  %v = load {lt}, ptr %slot");
                let out = if is_ref(t) {
                    let dup = self.dup_fn(t);
                    let _ = writeln!(b, "  call void {dup}(ptr %v)");
                    xfer_out(self, t, "%v", &mut b)
                } else {
                    "%v".to_string()
                };
                let _ = writeln!(b, "  store {lt} {out}, ptr %out");
            }
            _ => {
                let _ = writeln!(b, "  %old = load {lt}, ptr %slot\n  %new = load {lt}, ptr %out\n  store {lt} %new, ptr %slot");
                if is_ref(t) {
                    let rel = self.release_fn(t);
                    let _ = writeln!(b, "  call void {rel}(ptr %old)");
                }
            }
        }
        b.push_str("  ret void\n}\n");
        self.sync_helpers.push((name.clone(), b));
        name
    }

    /// The functions queued by the code above (and by each other): call
    /// until it gives nothing more, along with `rc_functions`.
    pub(super) fn sync_functions(&mut self) -> String {
        let mut out = String::new();
        while self.sync_done < self.sync_helpers.len() {
            out.push_str(&self.sync_helpers[self.sync_done].1);
            out.push('\n');
            self.sync_done += 1;
        }
        while self.slot_done < self.slot_types.len() {
            let t = self.slot_types[self.slot_done].clone();
            self.slot_done += 1;
            let rel = self.release_fn(&t);
            let _ = writeln!(out, "define internal void @tarn.slot.release.{}(ptr %s) #3 {{\n  %v = load ptr, ptr %s\n  call void {rel}(ptr %v)\n  ret void\n}}\n", mangle(&t));
        }
        while self.dup_done < self.dup_types.len() {
            let t = self.dup_types[self.dup_done].clone();
            self.dup_done += 1;
            let m = mangle(&t);
            let inner = t.non_null().clone();
            let inc = if inner == Type::Handle {
                "  %old = atomicrmw add ptr %p, i64 1 monotonic\n".to_string()
            } else if self.is_cyclic(&inner) {
                "  %c = load i64, ptr %p\n  %m = add i64 %c, 8\n  %n = and i64 %m, -4\n  store i64 %n, ptr %p\n".to_string()
            } else {
                "  %c = load i64, ptr %p\n  %n = add i64 %c, 1\n  store i64 %n, ptr %p\n".to_string()
            };
            let _ = writeln!(
                out,
                "define internal void @tarn.dup.{m}(ptr %p) #2 {{\nentry:\n  %z = icmp eq ptr %p, null\n  br i1 %z, label %done, label %inc\ninc:\n{inc}  br label %done\ndone:\n  ret void\n}}\n"
            );
        }
        while self.xfer_done < self.xfer_types.len() {
            let t = self.xfer_types[self.xfer_done].clone();
            self.xfer_done += 1;
            let code = self.xfer_code(&t);
            out.push_str(&code);
        }
        out
    }

    /// The definition of `@tarn.xfer.<m>` for `t`.
    fn xfer_code(&mut self, t: &Type) -> String {
        let m = mangle(t);
        let head = format!("define internal ptr @tarn.xfer.{m}(ptr %p, ptr %memo) #3 {{\nentry:\n");
        match t {
            Type::Nullable(inner) => {
                let f = self.xfer_fn(inner);
                format!("{head}  %z = icmp eq ptr %p, null\n  br i1 %z, label %null, label %some\nnull:\n  ret ptr null\nsome:\n  %r = call ptr {f}(ptr %p, ptr %memo)\n  ret ptr %r\n}}\n\n")
            }
            // Shared already (a handle), or immutable and immortal (a constant function).
            Type::Handle => format!("{head}  ret ptr %p\n}}\n\n"),
            // A function or interface value: its own type info knows how.
            Type::Func(..) | Type::Iface(_) => format!(
                "{head}  %tip = getelementptr inbounds i8, ptr %p, i64 8\n  %ti = load ptr, ptr %tip\n  %fp = getelementptr inbounds ptr, ptr %ti, i64 4\n  %f = load ptr, ptr %fp\n  %r = call ptr %f(ptr %p, ptr %memo)\n  ret ptr %r\n}}\n\n"
            ),
            // Strings never change: a copy needn't be remembered.
            Type::Str | Type::Bytes => {
                let rel = self.release_fn(t);
                format!(
                    "{head}  %u = call i64 @tarn_xfer_unique(ptr %p, i64 0)\n  %uz = icmp ne i64 %u, 0\n  br i1 %uz, label %move, label %copy\nmove:\n  ret ptr %p\ncopy:\n  %c = call ptr @tarn_str_clone(ptr %p)\n  call void {rel}(ptr %p)\n  ret ptr %c\n}}\n\n"
                )
            }
            _ => self.xfer_object(t, &head),
        }
    }

    /// `@tarn.xfer` of a struct, closure, box, enum, list or map: move it in
    /// place when it's ours alone, else copy it (once per walk: `memo`).
    fn xfer_object(&mut self, t: &Type, head: &str) -> String {
        // A closure or box is counted (and released) as a value of its
        // function or interface type, through its own type info.
        let rc_ty = match t {
            Type::Struct(sid) => self.prog.structs[*sid].closure.clone().unwrap_or_else(|| t.clone()),
            _ => t.clone(),
        };
        let cyclic = self.is_cyclic(&rc_ty) as i64;
        let rel = self.release_fn(&rc_ty);
        let dup = self.dup_fn(&rc_ty);
        let (moving, copying) = self.xfer_parts(t);
        let one = if cyclic == 1 { 8 } else { 1 };
        format!(
            "{head}  %u = call i64 @tarn_xfer_unique(ptr %p, i64 {cyclic})\n  %uz = icmp ne i64 %u, 0\n  br i1 %uz, label %move, label %copy\nmove:\n{moving}  ret ptr %p\ncopy:\n  %seen = call ptr @tarn_xfer_seen(ptr %memo, ptr %p)\n  %has = icmp ne ptr %seen, null\n  br i1 %has, label %again, label %fresh\nagain:\n  call void {dup}(ptr %seen)\n  call void {rel}(ptr %p)\n  ret ptr %seen\nfresh:\n{copying}  store i64 {one}, ptr %c\n  call void {rel}(ptr %p)\n  ret ptr %c\n}}\n\n"
        )
    }

    /// Move and copy code of `xfer_object`: the first walks `%p`'s children
    /// in place; the second makes `%c` (remembered before its children are
    /// walked, so a cycle back to `%p` finds it), then fills it in.
    fn xfer_parts(&mut self, t: &Type) -> (String, String) {
        let mut moving = String::new();
        let mut copying = String::new();
        let remember = "  call void @tarn_xfer_remember(ptr %memo, ptr %p, ptr %c)\n";
        // One field: `slot` in the original (move: in place) or `from` -> `to` (copy).
        let field_move = |c: &mut Self, ty: &Type, slot: &str, n: &str, out: &mut String| {
            if !is_ref(ty) {
                return;
            }
            let f = c.xfer_fn(ty);
            let _ = writeln!(out, "  %mv{n} = load ptr, ptr {slot}\n  %mv{n}x = call ptr {f}(ptr %mv{n}, ptr %memo)\n  store ptr %mv{n}x, ptr {slot}");
        };
        let field_copy = |c: &mut Self, ty: &Type, from: &str, to: &str, n: &str, out: &mut String| {
            let lt = llty(ty);
            let _ = writeln!(out, "  %cp{n} = load {lt}, ptr {from}");
            if is_ref(ty) {
                let dup = c.dup_fn(ty);
                let f = c.xfer_fn(ty);
                let _ = writeln!(out, "  call void {dup}(ptr %cp{n})\n  %cp{n}x = call ptr {f}(ptr %cp{n}, ptr %memo)\n  store ptr %cp{n}x, ptr {to}");
            } else {
                let _ = writeln!(out, "  store {lt} %cp{n}, ptr {to}");
            }
        };
        match t {
            Type::Struct(sid) => {
                let sid = *sid;
                let s = self.prog.structs[sid].clone();
                let lt = format!("%struct.{}", self.type_names[sid]);
                let base = if s.closure.is_some() { 3 } else { 1 };
                if s.drop.is_some() {
                    let name = self.string_cstr(&s.name);
                    let _ = writeln!(copying, "  call void @tarn_xfer_cant_copy(ptr {name})\n  unreachable\nnever:");
                }
                let _ = writeln!(copying, "  %szp = getelementptr {lt}, ptr null, i32 1\n  %sz = ptrtoint ptr %szp to i64\n  %c = call ptr @tarn_alloc(i64 %sz)");
                if s.closure.is_some() {
                    // A closure's (or box's) type info and code (or method table) go as they are.
                    let _ = writeln!(copying, "  %ti0 = getelementptr inbounds i8, ptr %p, i64 8\n  %ti1 = load ptr, ptr %ti0\n  %ti2 = getelementptr inbounds i8, ptr %c, i64 8\n  store ptr %ti1, ptr %ti2\n  %cd0 = getelementptr inbounds i8, ptr %p, i64 16\n  %cd1 = load ptr, ptr %cd0\n  %cd2 = getelementptr inbounds i8, ptr %c, i64 16\n  store ptr %cd1, ptr %cd2");
                }
                copying.push_str(remember);
                for (i, f) in s.fields.iter().enumerate() {
                    let slot = format!("%fs{i}");
                    let _ = writeln!(moving, "  {slot} = getelementptr inbounds {lt}, ptr %p, i32 0, i32 {}", i + base);
                    field_move(self, &f.ty, &slot, &i.to_string(), &mut moving);
                    let (from, to) = (format!("%ff{i}"), format!("%ft{i}"));
                    let _ = writeln!(copying, "  {from} = getelementptr inbounds {lt}, ptr %p, i32 0, i32 {}\n  {to} = getelementptr inbounds {lt}, ptr %c, i32 0, i32 {}", i + base, i + base);
                    field_copy(self, &f.ty, &from, &to, &i.to_string(), &mut copying);
                }
            }
            Type::Enum(sid, true) => {
                let sid = *sid;
                let variants = self.prog.structs[sid].variants().to_vec();
                let mut mv = Vec::new();
                let mut cp = Vec::new();
                for (k, v) in variants.iter().enumerate() {
                    let lt = self.variant_llty(sid, k);
                    let mut m = String::new();
                    let mut c = String::new();
                    if v.fields.is_empty() {
                        // (Data-less variants are immortal: always moved, never copied.)
                        cp.push("  unreachable\n".to_string());
                        mv.push(String::new());
                        continue;
                    }
                    let _ = writeln!(c, "  %szp{k} = getelementptr {lt}, ptr null, i32 1\n  %sz{k} = ptrtoint ptr %szp{k} to i64\n  %c{k} = call ptr @tarn_alloc(i64 %sz{k})\n  %tg{k} = getelementptr inbounds i8, ptr %c{k}, i64 8\n  store i64 {k}, ptr %tg{k}\n  call void @tarn_xfer_remember(ptr %memo, ptr %p, ptr %c{k})");
                    for (i, f) in v.fields.iter().enumerate() {
                        let n = format!("{k}_{i}");
                        let slot = format!("%fs{n}");
                        let _ = writeln!(m, "  {slot} = getelementptr inbounds {lt}, ptr %p, i32 0, i32 {}", i + 2);
                        field_move(self, &f.ty, &slot, &n, &mut m);
                        let (from, to) = (format!("%ff{n}"), format!("%ft{n}"));
                        let _ = writeln!(c, "  {from} = getelementptr inbounds {lt}, ptr %p, i32 0, i32 {}\n  {to} = getelementptr inbounds {lt}, ptr %c{k}, i32 0, i32 {}", i + 2, i + 2);
                        field_copy(self, &f.ty, &from, &to, &n, &mut c);
                    }
                    let _ = writeln!(c, "  br label %copied\n");
                    mv.push(m);
                    cp.push(c);
                }
                moving = tag_switch("mtag", &mv, "  br label %moved\n", "moved", "moved");
                // Each copying branch jumps to `copied`, which picks the copy made.
                let mut sw = tag_switch("ctag", &cp, "", "copied", "ctag.bad");
                let incoming: Vec<String> = variants.iter().enumerate().filter(|(_, v)| !v.fields.is_empty()).map(|(k, _)| format!("[ %c{k}, %ctag.{k} ]")).collect();
                if incoming.is_empty() {
                    sw.push_str("  %c = inttoptr i64 0 to ptr\n");
                } else {
                    let _ = writeln!(sw, "  %c = phi ptr {}", incoming.join(", "));
                }
                copying = sw;
            }
            Type::List(e) => {
                let e = (**e).clone();
                let size = elem_size(&e);
                let walk = |body: &str, tag: &str, list: &str| {
                    format!(
                        "  %{tag}lenp = getelementptr inbounds %TarnList, ptr {list}, i32 0, i32 1\n  %{tag}len = load i64, ptr %{tag}lenp\n  %{tag}dp = getelementptr inbounds %TarnList, ptr {list}, i32 0, i32 3\n  %{tag}data = load ptr, ptr %{tag}dp\n  br label %{tag}cond\n{tag}cond:\n  %{tag}i = phi i64 [ 0, %{tag}start ], [ %{tag}i1, %{tag}body ]\n  %{tag}c = icmp slt i64 %{tag}i, %{tag}len\n  br i1 %{tag}c, label %{tag}body, label %{tag}done\n{tag}body:\n{body}  %{tag}i1 = add i64 %{tag}i, 1\n  br label %{tag}cond\n{tag}done:\n"
                    )
                };
                if is_ref(&e) {
                    let mut body = String::new();
                    let _ = writeln!(body, "  %mslot = getelementptr inbounds ptr, ptr %mdata, i64 %mi");
                    field_move(self, &e, "%mslot", "e", &mut body);
                    moving = format!("  br label %mstart\nmstart:\n{}", walk(&body, "m", "%p"));
                }
                let _ = writeln!(copying, "  %plenp = getelementptr inbounds %TarnList, ptr %p, i32 0, i32 1\n  %plen = load i64, ptr %plenp\n  %c = call ptr @tarn_list_new(i64 {size}, i64 %plen)\n  %clenp = getelementptr inbounds %TarnList, ptr %c, i32 0, i32 1\n  store i64 %plen, ptr %clenp");
                copying.push_str(remember);
                let _ = writeln!(copying, "  %cdp = getelementptr inbounds %TarnList, ptr %c, i32 0, i32 3\n  %cdata = load ptr, ptr %cdp");
                if is_ref(&e) {
                    let mut body = String::new();
                    let _ = writeln!(body, "  %from = getelementptr inbounds ptr, ptr %kdata, i64 %ki\n  %to = getelementptr inbounds ptr, ptr %cdata, i64 %ki");
                    field_copy(self, &e, "%from", "%to", "e", &mut body);
                    copying.push_str(&format!("  br label %kstart\nkstart:\n{}", walk(&body, "k", "%p")));
                } else {
                    let _ = writeln!(copying, "  %pdp = getelementptr inbounds %TarnList, ptr %p, i32 0, i32 3\n  %pdata = load ptr, ptr %pdp\n  %bytes = mul i64 %plen, {size}\n  call void @llvm.memcpy.p0.p0.i64(ptr %cdata, ptr %pdata, i64 %bytes, i1 false)");
                }
            }
            Type::Map(..) => {
                let (k, v) = map_kv(t);
                let et = entry_ty(&k, &v);
                let each = |c: &mut Self, list: &str, tag: &str, copy: bool| -> String {
                    let mut action = String::new();
                    for (idx, ty, nm) in [(1, &k, "k"), (2, &v, "v")] {
                        if !is_ref(ty) {
                            continue;
                        }
                        let slot = format!("%{tag}{nm}slot");
                        let _ = writeln!(action, "  {slot} = getelementptr inbounds {et}, ptr %{tag}e, i32 0, i32 {idx}");
                        if copy {
                            // (The clone's slot holds the original's key or value, not yet counted.)
                            let dup = c.dup_fn(ty);
                            let f = c.xfer_fn(ty);
                            let _ = writeln!(action, "  %{tag}{nm}x = load ptr, ptr {slot}\n  call void {dup}(ptr %{tag}{nm}x)\n  %{tag}{nm}y = call ptr {f}(ptr %{tag}{nm}x, ptr %memo)\n  store ptr %{tag}{nm}y, ptr {slot}");
                        } else {
                            field_move(c, ty, &slot, &format!("{tag}{nm}"), &mut action);
                        }
                    }
                    format!(
                        "  br label %{tag}start\n{tag}start:\n  %{tag}usedp = getelementptr inbounds %TarnMap, ptr {list}, i32 0, i32 2\n  %{tag}used = load i64, ptr %{tag}usedp\n  %{tag}entsp = getelementptr inbounds %TarnMap, ptr {list}, i32 0, i32 4\n  %{tag}ents = load ptr, ptr %{tag}entsp\n  br label %{tag}cond\n{tag}cond:\n  %{tag}i = phi i64 [ 0, %{tag}start ], [ %{tag}i1, %{tag}next ]\n  %{tag}c = icmp slt i64 %{tag}i, %{tag}used\n  br i1 %{tag}c, label %{tag}body, label %{tag}done\n{tag}body:\n  %{tag}e = getelementptr inbounds {et}, ptr %{tag}ents, i64 %{tag}i\n  %{tag}h = load i64, ptr %{tag}e\n  %{tag}live = icmp sge i64 %{tag}h, 0\n  br i1 %{tag}live, label %{tag}act, label %{tag}next\n{tag}act:\n{action}  br label %{tag}next\n{tag}next:\n  %{tag}i1 = add i64 %{tag}i, 1\n  br label %{tag}cond\n{tag}done:\n"
                    )
                };
                if is_ref(&k) || is_ref(&v) {
                    moving = each(self, "%p", "m", false);
                }
                let _ = writeln!(copying, "  %strp = getelementptr {et}, ptr null, i32 1\n  %stride = ptrtoint ptr %strp to i64\n  %c = call ptr @tarn_map_clone(ptr %p, i64 %stride)");
                copying.push_str(remember);
                if is_ref(&k) || is_ref(&v) {
                    copying.push_str(&each(self, "%c", "k", true));
                }
            }
            other => unreachable!("no xfer for {other:?}"),
        }
        (moving, copying)
    }

    /// A C string constant (for the runtime's messages).
    fn string_cstr(&mut self, s: &str) -> String {
        let name = format!("@.cstr.{}", self.cstrs);
        self.cstrs += 1;
        let b = s.as_bytes();
        let _ = writeln!(self.globals, "{name} = private unnamed_addr constant [{} x i8] c\"{}\\00\"", b.len() + 1, escape_bytes(b));
        name
    }

    /// The xfer function of closure class or box `sid` (for its type info).
    pub(super) fn closure_xfer_fn(&mut self, sid: StructId) -> String {
        self.xfer_fn(&Type::Struct(sid))
    }
}

/// `mangle` for any type a slot can hold (also plain ones).
fn mangle_any(t: &Type) -> String {
    match t {
        Type::Unit => "unit".into(),
        t => mangle(t),
    }
}

/// A switch on the tag of an enum value (`%p`) with one block per variant,
/// each followed by `after` and a jump to `join` (empty `after`: the blocks end themselves).
fn tag_switch(prefix: &str, blocks: &[String], after: &str, join: &str, default: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "  %{prefix}p = getelementptr inbounds i8, ptr %p, i64 8\n  %{prefix} = load i64, ptr %{prefix}p");
    let cases: Vec<String> = (0..blocks.len()).map(|k| format!("i64 {k}, label %{prefix}.{k}")).collect();
    let _ = writeln!(out, "  switch i64 %{prefix}, label %{default} [ {} ]", cases.join(" "));
    for (k, b) in blocks.iter().enumerate() {
        let _ = writeln!(out, "{prefix}.{k}:\n{b}{after}");
    }
    if default != join {
        let _ = writeln!(out, "{default}:\n  unreachable");
    }
    let _ = writeln!(out, "{join}:");
    out
}
