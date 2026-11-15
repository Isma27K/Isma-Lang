//! Function values (closures).
//!
//! A closure is a heap object `{ rc, ti, code, captures... }`: its count, the
//! type info of its lambda (how to release, trace and free its captures),
//! the closure code to call, and the captured values. Calling one passes the
//! closure itself as the code's first argument. A function value without
//! captures (a named function, or a lambda using nothing from around it) is
//! a constant: an immortal global, never allocated.
//!
//! Values of one function type can come from different lambdas, so the
//! generic machinery (release, the cycle collector's tracing) goes through
//! dispatchers that read the object's own type info. Whether a function type
//! can be part of a cycle is decided for the type (see `cycles.rs`), so its
//! count is kept like other cyclic values' when it can.

use super::*;

impl<'a> Codegen<'a> {
    /// Index of field `idx` of struct `sid` in its LLVM type: after the count,
    /// and for a closure after its type info and code too.
    pub(super) fn field_slot(&self, sid: StructId, idx: usize) -> usize {
        if self.prog.structs[sid].closure.is_some() { idx + 3 } else { idx + 1 }
    }

    /// A function value: closure code `func` with captured values `args`.
    pub(super) fn closure_value(&mut self, func: FuncId, sid: Option<StructId>, args: &[Operand]) -> String {
        self.closure_rt = true;
        let symbol = self.prog.funcs[func].symbol.clone();
        let Some(sid) = sid.filter(|_| !args.is_empty()) else {
            // No captures: a constant.
            if !self.fn_consts.contains(&func) {
                self.fn_consts.push(func);
                let _ = writeln!(
                    self.globals,
                    "@tarn.fnval.{func} = internal global {{ i64, ptr, ptr }} {{ i64 {RC_IMMORTAL}, ptr @tarn.ti.fnconst, ptr @{symbol} }}"
                );
            }
            return format!("@tarn.fnval.{func}");
        };
        if !self.closure_classes.contains(&sid) {
            self.closure_classes.push(sid);
        }
        let vals: Vec<(String, Type)> = args.iter().map(|a| (self.operand(a), self.f.operand_ty(a))).collect();
        let lt = format!("%struct.{}", self.type_names[sid]);
        let szp = self.fresh();
        self.emit(format!("{szp} = getelementptr {lt}, ptr null, i32 1"));
        let sz = self.fresh();
        self.emit(format!("{sz} = ptrtoint ptr {szp} to i64"));
        let p = self.fresh();
        self.emit(format!("{p} = call ptr @tarn_alloc(i64 {sz})"));
        let fty = self.prog.structs[sid].closure.clone().expect("closure struct");
        let one = if self.is_cyclic(&fty) { 8 } else { 1 };
        let md = self.tbaa("rc");
        self.emit(format!("store i64 {one}, ptr {p}{md}"));
        let tip = self.fresh();
        self.emit(format!("{tip} = getelementptr inbounds {lt}, ptr {p}, i32 0, i32 1"));
        let md = self.tbaa("closure.ti");
        self.emit(format!("store ptr @tarn.ti.cl{sid}, ptr {tip}{md}"));
        let cp = self.fresh();
        self.emit(format!("{cp} = getelementptr inbounds {lt}, ptr {p}, i32 0, i32 2"));
        let md = self.tbaa("closure.code");
        self.emit(format!("store ptr @{symbol}, ptr {cp}{md}"));
        for (i, (v, ty)) in vals.iter().enumerate() {
            let fp = self.fresh();
            self.emit(format!("{fp} = getelementptr inbounds {lt}, ptr {p}, i32 0, i32 {}", i + 3));
            let md = self.field_tbaa(sid, i);
            self.emit(format!("store {} {v}, ptr {fp}{md}", llty(ty)));
        }
        p
    }

    /// Call the function value `f` with `args`.
    pub(super) fn call_indirect(&mut self, f: &Operand, args: &[Operand]) -> String {
        let Type::Func(_, ret) = self.f.operand_ty(f).non_null().clone() else { unreachable!("calling a non-function") };
        let c = self.operand(f);
        let mut vals = vec![format!("ptr {c}")];
        for a in args {
            let ty = llty(&self.f.operand_ty(a));
            let v = self.operand(a);
            vals.push(format!("{ty} {v}"));
        }
        let cp = self.fresh();
        self.emit(format!("{cp} = getelementptr inbounds i8, ptr {c}, i64 16"));
        let code = self.fresh();
        let md = self.tbaa("closure.code");
        self.emit(format!("{code} = load ptr, ptr {cp}{md}"));
        if *ret == Type::Unit {
            self.emit(format!("call void {code}({})", vals.join(", ")));
            String::new()
        } else {
            let r = self.fresh();
            self.emit(format!("{r} = call {} {code}({})", llty(&ret), vals.join(", ")));
            r
        }
    }

    /// The release function (and, when cyclic, the type info) of function type `t`.
    pub(super) fn func_rc_functions(&mut self, t: &Type, out: &mut String) {
        self.closure_rt = true;
        let m = mangle(t);
        if self.is_cyclic(t) {
            let _ = writeln!(
                out,
                "@tarn.ti.{m} = internal constant {{ ptr, ptr, ptr, ptr }} {{ ptr @tarn.closure.trace, ptr @tarn.closure.drop, ptr @tarn.closure.white, ptr @tarn.closure.mem }}\n"
            );
            let _ = writeln!(out, "define internal void @tarn.release.{m}(ptr %p) #2 {{\n  call void @tarn_release_cyclic(ptr %p, ptr @tarn.ti.{m})\n  ret void\n}}\n");
        } else {
            let md = self.tbaa("rc");
            let _ = writeln!(
                out,
                "define internal void @tarn.release.{m}(ptr %p) #2 {{\n  %rc = load i64, ptr %p{md}\n  %n = sub i64 %rc, 1\n  store i64 %n, ptr %p{md}\n  %z = icmp eq i64 %n, 0\n  br i1 %z, label %free, label %done\nfree:\n  call void @tarn_drop_slow(ptr %p, ptr @tarn.closure.drop)\n  br label %done\ndone:\n  ret void\n}}\n"
            );
        }
    }

    /// The dispatchers, and the type info of each lambda whose closures were built.
    pub(super) fn closure_functions(&mut self) -> String {
        let mut out = String::new();
        if self.closure_rt && !self.closure_rt_done {
            self.closure_rt_done = true;
            // Forward to the object's own type info (its second word).
            for (i, (name, params, args)) in
                [("trace", "ptr %p, ptr %visit", "ptr %p, ptr %visit"), ("drop", "ptr %p", "ptr %p"), ("white", "ptr %p", "ptr %p"), ("mem", "ptr %p", "ptr %p")].iter().enumerate()
            {
                let _ = writeln!(
                    out,
                    "define internal void @tarn.closure.{name}({params}) #0 {{\n  %tip = getelementptr inbounds i8, ptr %p, i64 8\n  %ti = load ptr, ptr %tip\n  %fp = getelementptr inbounds {{ ptr, ptr, ptr, ptr }}, ptr %ti, i32 0, i32 {i}\n  %f = load ptr, ptr %fp\n  call void %f({args})\n  ret void\n}}\n"
                );
            }
            // Constant function values never die: their record does nothing.
            out.push_str("define internal void @tarn.noop(ptr %p) #0 {\n  ret void\n}\n\n");
            out.push_str("define internal void @tarn.noop.trace(ptr %p, ptr %visit) #0 {\n  ret void\n}\n\n");
            out.push_str("@tarn.ti.fnconst = internal constant { ptr, ptr, ptr, ptr } { ptr @tarn.noop.trace, ptr @tarn.noop, ptr @tarn.noop, ptr @tarn.noop }\n\n");
        }
        while self.closures_done < self.closure_classes.len() {
            let sid = self.closure_classes[self.closures_done];
            self.closures_done += 1;
            self.closure_class(sid, &mut out);
        }
        out
    }

    /// Release, trace and free functions for the closures of one lambda (struct `sid`).
    fn closure_class(&mut self, sid: StructId, out: &mut String) {
        let fty = self.prog.structs[sid].closure.clone().expect("closure struct");
        let cyclic = self.is_cyclic(&fty);
        let lt = format!("%struct.{}", self.type_names[sid]);
        let caps: Vec<(usize, Type)> = self.prog.structs[sid].fields.iter().enumerate().filter(|(_, f)| is_ref(&f.ty)).map(|(i, f)| (i, f.ty.clone())).collect();
        let (mut all, mut trace, mut acyclic) = (String::new(), String::new(), String::new());
        for (k, (idx, cty)) in caps.iter().enumerate() {
            let rel = self.release_fn(cty);
            let load = format!("  %c{k}p = getelementptr inbounds {lt}, ptr %p, i32 0, i32 {}\n  %c{k} = load ptr, ptr %c{k}p\n", idx + 3);
            let _ = writeln!(all, "{load}  call void {rel}(ptr %c{k})");
            if self.is_cyclic(cty) {
                let _ = writeln!(trace, "{load}  call void %visit(ptr %c{k}, ptr @tarn.ti.{})", mangle(cty.non_null()));
            } else {
                let _ = writeln!(acyclic, "{load}  call void {rel}(ptr %c{k})");
            }
        }
        let free = if cyclic { format!("  call void @tarn_cyclic_free(ptr %p, ptr @tarn.ti.cl{sid})\n") } else { format!("  call void @tarn.cl{sid}.mem(ptr %p)\n") };
        let _ = writeln!(out, "define internal void @tarn.cl{sid}.drop(ptr %p) #0 {{\n{all}{free}  ret void\n}}\n");
        let _ = writeln!(out, "define internal void @tarn.cl{sid}.trace(ptr %p, ptr %visit) #0 {{\n{trace}  ret void\n}}\n");
        let _ = writeln!(out, "define internal void @tarn.cl{sid}.white(ptr %p) #0 {{\n{acyclic}  call void @tarn.cl{sid}.mem(ptr %p)\n  ret void\n}}\n");
        let _ = writeln!(
            out,
            "define internal void @tarn.cl{sid}.mem(ptr %p) #0 {{\n  %szp = getelementptr {lt}, ptr null, i32 1\n  %sz = ptrtoint ptr %szp to i64\n  call void @tarn_free(ptr %p, i64 %sz)\n  ret void\n}}\n"
        );
        let _ = writeln!(
            out,
            "@tarn.ti.cl{sid} = internal constant {{ ptr, ptr, ptr, ptr }} {{ ptr @tarn.cl{sid}.trace, ptr @tarn.cl{sid}.drop, ptr @tarn.cl{sid}.white, ptr @tarn.cl{sid}.mem }}\n"
        );
    }
}
