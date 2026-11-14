//! Exhaustiveness and reachability of `match` cases.
//!
//! This is the "usefulness" algorithm from Luc Maranget's *Warnings for
//! pattern matching* (2007), the one Rust and OCaml use. A pattern vector is
//! *useful* against a matrix of earlier patterns if some value matches it
//! and none of the rows. Then:
//!
//! - a `case` is unreachable when it isn't useful against the cases before it;
//! - a `match` is exhaustive when the wildcard `_` isn't useful against all
//!   its cases. When it is, the algorithm also returns a *witness*: a value
//!   no case matches, which the error message shows (`Rect(_, _)`).
//!
//! Cases with an `if` guard may not match, so they never count as covering.

use crate::tast::{TPat, TPatKind, TStruct};
use crate::types::Type;

/// A pattern, reduced to what matters for coverage.
#[derive(Debug, Clone, PartialEq)]
pub enum Pat {
    Wild,
    Ctor(Ctor, Vec<Pat>),
    Or(Vec<Pat>),
}

/// The ways a value can be built: a variant (by index), a struct, a literal,
/// or for a `T?` either `null` or a non-null `T`.
#[derive(Debug, Clone, PartialEq)]
pub enum Ctor {
    Variant(usize),
    Struct,
    Bool(bool),
    Int(i64),
    Str(String),
    Null,
    Some,
}

/// Lower a checked pattern (bindings match anything).
pub fn of_tpat(p: &TPat) -> Pat {
    match &p.kind {
        TPatKind::Wild => Pat::Wild,
        TPatKind::Bind(_, inner) => of_tpat(inner),
        TPatKind::Variant(_, k, args) => Pat::Ctor(Ctor::Variant(*k), args.iter().map(of_tpat).collect()),
        TPatKind::Struct(_, args) => Pat::Ctor(Ctor::Struct, args.iter().map(of_tpat).collect()),
        TPatKind::Int(v) => Pat::Ctor(Ctor::Int(*v), Vec::new()),
        TPatKind::Str(v) => Pat::Ctor(Ctor::Str(v.clone()), Vec::new()),
        TPatKind::Bool(v) => Pat::Ctor(Ctor::Bool(*v), Vec::new()),
        TPatKind::Null => Pat::Ctor(Ctor::Null, Vec::new()),
        TPatKind::Some(inner) => Pat::Ctor(Ctor::Some, vec![of_tpat(inner)]),
        TPatKind::Or(alts) => Pat::Or(alts.iter().map(of_tpat).collect()),
    }
}

/// The value space, as given by the program's structs and enums.
pub struct Space<'a> {
    pub structs: &'a [TStruct],
}

impl Space<'_> {
    /// Every constructor of `t`, or `None` when there are infinitely many
    /// (numbers, strings, lists): only `_` or a binding covers those.
    fn ctors(&self, t: &Type) -> Option<Vec<Ctor>> {
        match t {
            Type::Bool => Some(vec![Ctor::Bool(false), Ctor::Bool(true)]),
            Type::Enum(id, _) => Some((0..self.structs[*id].variants().len()).map(Ctor::Variant).collect()),
            Type::Struct(_) => Some(vec![Ctor::Struct]),
            Type::Nullable(_) => Some(vec![Ctor::Null, Ctor::Some]),
            _ => None,
        }
    }

    /// The types of what `c` holds, when building a `t`.
    fn fields(&self, t: &Type, c: &Ctor) -> Vec<Type> {
        match (t, c) {
            (Type::Enum(id, _), Ctor::Variant(k)) => self.structs[*id].variants()[*k].fields.iter().map(|f| f.ty.clone()).collect(),
            (Type::Struct(id), Ctor::Struct) => self.structs[*id].fields.iter().map(|f| f.ty.clone()).collect(),
            (Type::Nullable(inner), Ctor::Some) => vec![(**inner).clone()],
            _ => Vec::new(),
        }
    }

    /// Is the vector `v` useful against `rows`? Returns a witness (a vector
    /// of patterns matched by `v` but by no row) if so.
    pub fn useful(&self, rows: &[Vec<Pat>], v: &[Pat], tys: &[Type]) -> Option<Vec<Pat>> {
        let Some(head) = v.first() else {
            return rows.is_empty().then(Vec::new);
        };
        let rows = expand_or(rows);
        let ty = &tys[0];
        match head {
            Pat::Or(alts) => alts.iter().find_map(|a| {
                let v2: Vec<Pat> = std::iter::once(a.clone()).chain(v[1..].iter().cloned()).collect();
                self.useful(&rows, &v2, tys)
            }),
            Pat::Ctor(c, args) => self.useful_ctor(&rows, c, args, &v[1..], tys),
            Pat::Wild => {
                let heads: Vec<&Ctor> = rows.iter().filter_map(|r| if let Pat::Ctor(c, _) = &r[0] { Some(c) } else { None }).collect();
                match self.ctors(ty) {
                    // Every constructor appears: the wildcard is useful iff it is for one of them.
                    Some(all) if !all.is_empty() && all.iter().all(|c| heads.contains(&c)) => all.iter().find_map(|c| {
                        let wilds = vec![Pat::Wild; self.fields(ty, c).len()];
                        self.useful_ctor(&rows, c, &wilds, &v[1..], tys)
                    }),
                    // Some constructor is missing: only rows starting with a wildcard matter.
                    all => {
                        let rest: Vec<Vec<Pat>> = rows.iter().filter(|r| r[0] == Pat::Wild).map(|r| r[1..].to_vec()).collect();
                        let w = self.useful(&rest, &v[1..], &tys[1..])?;
                        let missing = match all {
                            Some(all) => all.into_iter().find(|c| !heads.contains(&c)).map(|c| self.example(ty, c)).unwrap_or(Pat::Wild),
                            None => Pat::Wild,
                        };
                        Some(std::iter::once(missing).chain(w).collect())
                    }
                }
            }
        }
    }

    fn useful_ctor(&self, rows: &[Vec<Pat>], c: &Ctor, args: &[Pat], rest: &[Pat], tys: &[Type]) -> Option<Vec<Pat>> {
        let ftys = self.fields(&tys[0], c);
        let n = ftys.len();
        let m: Vec<Vec<Pat>> = rows
            .iter()
            .filter_map(|r| match &r[0] {
                Pat::Ctor(c2, a) if c2 == c => Some(a.iter().chain(&r[1..]).cloned().collect()),
                Pat::Wild => Some(std::iter::repeat_n(Pat::Wild, n).chain(r[1..].iter().cloned()).collect()),
                _ => None,
            })
            .collect();
        let v2: Vec<Pat> = args.iter().chain(rest).cloned().collect();
        let t2: Vec<Type> = ftys.into_iter().chain(tys[1..].iter().cloned()).collect();
        let w = self.useful(&m, &v2, &t2)?;
        let (fields, after) = w.split_at(n);
        Some(std::iter::once(Pat::Ctor(c.clone(), fields.to_vec())).chain(after.iter().cloned()).collect())
    }

    /// A value built with `c`, for an error message. A non-null `T?` shows a
    /// constructor of `T`, so that `null` and each variant get listed in turn.
    fn example(&self, ty: &Type, c: Ctor) -> Pat {
        let fields = self.fields(ty, &c);
        let args = match (&c, ty) {
            (Ctor::Some, Type::Nullable(inner)) => vec![match self.ctors(inner).and_then(|cs| cs.into_iter().next()) {
                Some(first) => self.example(inner, first),
                None => Pat::Wild,
            }],
            _ => vec![Pat::Wild; fields.len()],
        };
        Pat::Ctor(c, args)
    }

    /// Values of type `ty` no row matches (up to `limit`, plus one to tell if there are more).
    pub fn missing(&self, mut rows: Vec<Vec<Pat>>, ty: &Type, limit: usize) -> Vec<Pat> {
        let mut out = Vec::new();
        while out.len() <= limit {
            let Some(mut w) = self.useful(&rows, &[Pat::Wild], std::slice::from_ref(ty)) else { break };
            let p = w.remove(0);
            rows.push(vec![p.clone()]);
            out.push(p);
        }
        out
    }

    /// Isma source for a pattern of type `ty`.
    pub fn show(&self, p: &Pat, ty: &Type) -> String {
        match p {
            Pat::Wild => "_".into(),
            Pat::Or(alts) => alts.iter().map(|a| self.show(a, ty)).collect::<Vec<_>>().join(" | "),
            Pat::Ctor(c, args) => {
                let ftys = self.fields(ty, c);
                let shown: Vec<String> = args.iter().zip(&ftys).map(|(a, t)| self.show(a, t)).collect();
                match (c, ty) {
                    (Ctor::Variant(k), Type::Enum(id, _)) => {
                        let name = &self.structs[*id].variants()[*k].name;
                        if shown.is_empty() { name.clone() } else { format!("{name}({})", shown.join(", ")) }
                    }
                    (Ctor::Struct, Type::Struct(id)) => {
                        let name = crate::types::short_name(&self.structs[*id].name).to_string();
                        format!("{name}({})", shown.join(", "))
                    }
                    (Ctor::Some, _) => shown.into_iter().next().unwrap_or_else(|| "_".into()),
                    (Ctor::Null, _) => "null".into(),
                    (Ctor::Bool(b), _) => b.to_string(),
                    (Ctor::Int(v), _) => v.to_string(),
                    (Ctor::Str(s), _) => format!("{s:?}"),
                    _ => "_".into(),
                }
            }
        }
    }
}

/// Replace rows whose first pattern is `a | b` by one row per alternative.
fn expand_or(rows: &[Vec<Pat>]) -> Vec<Vec<Pat>> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        match r.first() {
            Some(Pat::Or(alts)) => {
                for a in alts {
                    let row: Vec<Pat> = std::iter::once(a.clone()).chain(r[1..].iter().cloned()).collect();
                    out.extend(expand_or(&[row]));
                }
            }
            _ => out.push(r.clone()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tast::{TField, TVariant};

    fn field(t: Type) -> TField {
        TField { name: "f".into(), ty: t, mutable: false }
    }

    /// enum Shape: Circle(Float) | Rect(Float, Float) | Empty
    /// enum Expr: Num(Int) | Neg(Expr) | Add(Expr, Expr)
    fn space() -> Vec<TStruct> {
        let variant = |n: &str, fs: Vec<Type>| TVariant { name: n.into(), fields: fs.into_iter().map(field).collect() };
        vec![
            TStruct {
                name: "Shape".into(),
                fields: vec![],
                variants: Some(vec![variant("Circle", vec![Type::Float]), variant("Rect", vec![Type::Float, Type::Float]), variant("Empty", vec![])]),
                generic: false,
            },
            TStruct {
                name: "Expr".into(),
                fields: vec![],
                variants: Some(vec![
                    variant("Num", vec![Type::Int]),
                    variant("Neg", vec![Type::Enum(1, true)]),
                    variant("Add", vec![Type::Enum(1, true), Type::Enum(1, true)]),
                ]),
                generic: false,
            },
        ]
    }

    fn v(k: usize, args: Vec<Pat>) -> Pat {
        Pat::Ctor(Ctor::Variant(k), args)
    }

    fn missing(structs: &[TStruct], rows: Vec<Pat>, ty: &Type) -> Vec<String> {
        let s = Space { structs };
        s.missing(rows.into_iter().map(|p| vec![p]).collect(), ty, 3).iter().map(|p| s.show(p, ty)).collect()
    }

    #[test]
    fn lists_the_missing_variants() {
        let st = space();
        let shape = Type::Enum(0, true);
        assert_eq!(missing(&st, vec![v(0, vec![Pat::Wild])], &shape), vec!["Rect(_, _)", "Empty"]);
        assert!(missing(&st, vec![v(0, vec![Pat::Wild]), Pat::Or(vec![v(1, vec![Pat::Wild, Pat::Wild]), v(2, vec![])])], &shape).is_empty());
        assert!(missing(&st, vec![Pat::Wild], &shape).is_empty());
    }

    #[test]
    fn nested_patterns_and_literals() {
        let st = space();
        let expr = Type::Enum(1, true);
        let num = |p: Pat| v(0, vec![p]);
        // Num(0), Num(_), Neg(Num(_)), Add(_, _): Neg(Neg(_)) and Neg(Add(_, _)) are missing.
        let rows = vec![num(Pat::Ctor(Ctor::Int(0), vec![])), num(Pat::Wild), v(1, vec![num(Pat::Wild)]), v(2, vec![Pat::Wild, Pat::Wild])];
        assert_eq!(missing(&st, rows, &expr), vec!["Neg(Neg(_))", "Neg(Add(_, _))"]);
        // Numbers are infinite: a literal never completes them.
        assert_eq!(missing(&st, vec![num(Pat::Ctor(Ctor::Int(1), vec![]))], &expr), vec!["Neg(_)", "Add(_, _)", "Num(_)"]);
    }

    #[test]
    fn nullable_and_bool() {
        let st = space();
        let opt = Type::Enum(0, true).nullable();
        assert_eq!(missing(&st, vec![], &opt), vec!["null", "Circle(_)", "Rect(_, _)", "Empty"]);
        let some = |p: Pat| Pat::Ctor(Ctor::Some, vec![p]);
        assert_eq!(missing(&st, vec![some(Pat::Wild)], &opt), vec!["null"]);
        assert_eq!(missing(&st, vec![Pat::Ctor(Ctor::Bool(true), vec![])], &Type::Bool), vec!["false"]);
    }

    #[test]
    fn finds_unreachable_cases() {
        let st = space();
        let s = Space { structs: &st };
        let shape = [Type::Enum(0, true)];
        let rows = vec![vec![v(0, vec![Pat::Wild])], vec![Pat::Wild]];
        assert!(s.useful(&rows[..1], &[v(1, vec![Pat::Wild, Pat::Wild])], &shape).is_some());
        assert!(s.useful(&rows, &[v(2, vec![])], &shape).is_none());
        assert!(s.useful(&rows[..1], &[v(0, vec![Pat::Ctor(Ctor::Int(0), vec![])])], &shape).is_none());
    }
}
