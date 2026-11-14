//! Recursive-descent parser (statements) + precedence climbing (expressions).

use crate::ast::*;
use crate::diagnostics::Diagnostic;
use crate::lexer;
use crate::span::Span;
use crate::token::{FPart, Tok, Token};
use std::mem::discriminant;

type PResult<T> = Result<T, ()>;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    prev_span: Span,
    diags: Vec<Diagnostic>,
}

/// Parse a whole source file.
pub fn parse(src: &str) -> (Program, Vec<Diagnostic>) {
    parse_at(src, 0)
}

/// Parse a file whose spans start at `base` (standard-library modules).
pub fn parse_at(src: &str, base: usize) -> (Program, Vec<Diagnostic>) {
    let (tokens, mut diags) = lexer::lex_at(src, base);
    let mut p = Parser::new(tokens);
    let program = p.program();
    diags.append(&mut p.diags);
    (program, diags)
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0, prev_span: Span::default(), diags: Vec::new() }
    }

    // ---------- token helpers ----------

    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].tok
    }

    fn peek_nth(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[i].tok
    }

    fn span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn at(&self, t: &Tok) -> bool {
        discriminant(self.peek()) == discriminant(t)
    }

    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        self.prev_span = t.span;
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.at(t) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn error<T>(&mut self, msg: impl Into<String>, span: Span) -> PResult<T> {
        self.diags.push(Diagnostic::error(msg, span));
        Err(())
    }

    fn error_diag<T>(&mut self, d: Diagnostic) -> PResult<T> {
        self.diags.push(d);
        Err(())
    }

    fn unexpected<T>(&mut self, expected: &str) -> PResult<T> {
        let span = self.span();
        let found = self.peek().clone();
        self.error_diag(
            Diagnostic::error(format!("expected {expected}, found {found}"), span)
                .with_primary(format!("expected {expected}")),
        )
    }

    fn expect(&mut self, t: Tok) -> PResult<Span> {
        if self.at(&t) {
            Ok(self.bump().span)
        } else {
            self.unexpected(&t.to_string())
        }
    }

    fn expect_ident(&mut self, what: &str) -> PResult<Ident> {
        if let Tok::Ident(name) = self.peek().clone() {
            let span = self.bump().span;
            Ok(Ident { name, span })
        } else {
            self.unexpected(what)
        }
    }

    fn expect_newline(&mut self) -> PResult<()> {
        if self.eat(&Tok::Newline) || self.at(&Tok::Eof) {
            Ok(())
        } else {
            self.unexpected("end of line")
        }
    }

    /// Skip an indented block starting at an INDENT token.
    fn skip_block(&mut self) {
        let mut depth = 0usize;
        loop {
            match self.peek() {
                Tok::Indent => depth += 1,
                Tok::Dedent => {
                    depth -= 1;
                    if depth == 0 {
                        self.bump();
                        return;
                    }
                }
                Tok::Eof => return,
                _ => {}
            }
            self.bump();
        }
    }

    /// Error recovery: skip the rest of the current line and any block under it.
    fn synchronize(&mut self) {
        loop {
            match self.peek() {
                Tok::Newline => {
                    self.bump();
                    break;
                }
                Tok::Eof | Tok::Dedent => return,
                Tok::Indent => {
                    self.skip_block();
                    return;
                }
                _ => {
                    self.bump();
                }
            }
        }
        if self.at(&Tok::Indent) {
            self.skip_block();
        }
    }

    // ---------- items ----------

    fn program(&mut self) -> Program {
        let mut items = Vec::new();
        loop {
            while self.eat(&Tok::Newline) {}
            match self.peek() {
                Tok::Eof => break,
                Tok::Fn => match self.fn_decl() {
                    Ok(f) => items.push(Item::Fn(f)),
                    Err(()) => self.synchronize(),
                },
                Tok::Struct => match self.struct_decl() {
                    Ok(s) => items.push(Item::Struct(s)),
                    Err(()) => self.synchronize(),
                },
                Tok::Import => {
                    self.bump();
                    match self.expect_ident("a module name, e.g. `import time`") {
                        Ok(name) => {
                            if self.at(&Tok::Dot) || self.at(&Tok::Comma) {
                                let span = self.span();
                                self.diags.push(
                                    Diagnostic::error("import one module per line", span)
                                        .with_help("write `import time` on its own line"),
                                );
                                self.synchronize();
                            } else if self.expect_newline().is_ok() {
                                items.push(Item::Import(name));
                            } else {
                                self.synchronize();
                            }
                        }
                        Err(()) => self.synchronize(),
                    }
                }
                Tok::Indent => {
                    let span = self.span();
                    self.diags.push(Diagnostic::error("unexpected indentation", span));
                    self.skip_block();
                }
                Tok::Dedent => {
                    self.bump();
                }
                Tok::Val | Tok::Var => {
                    let span = self.span();
                    self.diags.push(
                        Diagnostic::error("top-level variables are not supported yet", span)
                            .with_help("move this into a function such as `fn main():`"),
                    );
                    self.synchronize();
                }
                _ => {
                    let span = self.span();
                    let found = self.peek().clone();
                    self.diags.push(
                        Diagnostic::error(format!("expected `fn` or `struct`, found {found}"), span)
                            .with_note("only functions and structs may appear at the top level")
                            .with_help("put statements inside `fn main():`"),
                    );
                    self.synchronize();
                }
            }
        }
        Program { items }
    }

    fn fn_decl(&mut self) -> PResult<FnDecl> {
        let start = self.expect(Tok::Fn)?;
        let name = self.expect_ident("function name")?;
        self.expect(Tok::LParen)?;
        let mut self_param = None;
        let mut params = Vec::new();
        while !self.at(&Tok::RParen) {
            if matches!(self.peek(), Tok::Ident(n) if n == "self")
                && params.is_empty()
                && self_param.is_none()
                && !matches!(self.peek_nth(1), Tok::Colon)
            {
                self_param = Some(self.bump().span);
            } else {
                let pname = self.expect_ident("parameter name")?;
                if !self.at(&Tok::Colon) {
                    let span = pname.span;
                    return self.error_diag(
                        Diagnostic::error(format!("parameter `{}` needs a type annotation", pname.name), span)
                            .with_help(format!("write it as `{}: Int` (or whichever type it is)", pname.name)),
                    );
                }
                self.bump();
                let ty = self.type_expr()?;
                params.push(Param { name: pname, ty });
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        let ret = if self.eat(&Tok::Arrow) { Some(self.type_expr()?) } else { None };
        let sig_span = start.merge(self.prev_span);
        let body = self.block()?;
        Ok(FnDecl { name, self_param, params, ret, body, sig_span })
    }

    fn struct_decl(&mut self) -> PResult<StructDecl> {
        self.expect(Tok::Struct)?;
        let name = self.expect_ident("struct name")?;
        self.expect(Tok::Colon)?;
        self.expect(Tok::Newline)?;
        if !self.at(&Tok::Indent) {
            return self.unexpected("an indented struct body");
        }
        self.bump();
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        while !self.at(&Tok::Dedent) && !self.at(&Tok::Eof) {
            match self.peek() {
                Tok::Newline => {
                    self.bump();
                }
                Tok::Pass => {
                    self.bump();
                    if self.expect_newline().is_err() {
                        self.synchronize();
                    }
                }
                Tok::Fn => match self.fn_decl() {
                    Ok(f) => methods.push(f),
                    Err(()) => self.synchronize(),
                },
                Tok::Val | Tok::Var | Tok::Ident(_) => match self.field_decl() {
                    Ok(f) => fields.push(f),
                    Err(()) => self.synchronize(),
                },
                _ => {
                    let _ = self.unexpected::<()>("a field or method");
                    self.synchronize();
                }
            }
        }
        self.eat(&Tok::Dedent);
        Ok(StructDecl { name, fields, methods })
    }

    fn field_decl(&mut self) -> PResult<FieldDecl> {
        let mutable = match self.peek() {
            Tok::Var => {
                self.bump();
                true
            }
            Tok::Val => {
                self.bump();
                false
            }
            _ => false,
        };
        let name = self.expect_ident("field name")?;
        if !self.at(&Tok::Colon) {
            let span = name.span;
            return self.error_diag(
                Diagnostic::error(format!("field `{}` needs a type annotation", name.name), span)
                    .with_help(format!("write it as `{}: Int` (or whichever type it is)", name.name)),
            );
        }
        self.bump();
        let ty = self.type_expr()?;
        if self.at(&Tok::Assign) {
            let span = self.span();
            return self.error("default field values are not supported yet", span);
        }
        self.expect_newline()?;
        Ok(FieldDecl { name, ty, mutable })
    }

    fn type_expr(&mut self) -> PResult<TypeExpr> {
        let mut name = self.expect_ident("a type")?;
        let mut module = None;
        if self.at(&Tok::Dot) {
            // `time.DateTime`
            self.bump();
            let inner = self.expect_ident("a type name after `.`")?;
            module = Some(std::mem::replace(&mut name, inner));
        }
        let mut args = Vec::new();
        if self.eat(&Tok::LBracket) {
            loop {
                args.push(self.type_expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RBracket)?;
        }
        let nullable = self.eat(&Tok::Question);
        if nullable && self.at(&Tok::Question) {
            let span = self.span();
            return self.error_diag(
                Diagnostic::error("a type can only be made nullable once", span)
                    .with_primary("remove this")
                    .with_help("`T?` already allows `null`"),
            );
        }
        let start = module.as_ref().map(|m| m.span).unwrap_or(name.span);
        let span = start.merge(self.prev_span);
        Ok(TypeExpr { module, name, args, nullable, span })
    }

    // ---------- statements ----------

    fn block(&mut self) -> PResult<Block> {
        self.expect(Tok::Colon)?;
        if self.eat(&Tok::Newline) {
            if !self.at(&Tok::Indent) {
                let span = self.span();
                return self.error_diag(
                    Diagnostic::error("expected an indented block", span)
                        .with_help("indent the body with spaces, or write `pass` for an empty block"),
                );
            }
            self.bump();
            let mut stmts = Vec::new();
            while !self.at(&Tok::Dedent) && !self.at(&Tok::Eof) {
                match self.stmt() {
                    Ok(s) => stmts.push(s),
                    Err(()) => self.synchronize(),
                }
            }
            self.eat(&Tok::Dedent);
            Ok(stmts)
        } else {
            // `if x: return 1` style one-liner.
            Ok(vec![self.simple_stmt()?])
        }
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        match self.peek() {
            Tok::Val | Tok::Var => self.let_stmt(),
            Tok::If => self.if_stmt(),
            Tok::While => {
                let start = self.bump().span;
                let cond = self.expr()?;
                let body = self.block()?;
                Ok(Stmt { kind: StmtKind::While { cond, body }, span: start.merge(self.prev_span) })
            }
            Tok::For => self.for_stmt(),
            Tok::Fn => {
                let span = self.span();
                self.error("nested functions are not supported yet", span)
            }
            Tok::Struct => {
                let span = self.span();
                self.error("structs must be declared at the top level", span)
            }
            Tok::Import => {
                let span = self.span();
                self.error_diag(
                    Diagnostic::error("`import` must be at the top level of the file", span)
                        .with_help("move it to the top of the file, above your functions"),
                )
            }
            Tok::Indent => {
                let span = self.span();
                self.error("unexpected indentation", span)
            }
            Tok::Elif | Tok::Else => {
                let span = self.span();
                let t = self.peek().clone();
                self.error(format!("{t} without a matching `if`"), span)
            }
            _ => self.simple_stmt(),
        }
    }

    fn simple_stmt(&mut self) -> PResult<Stmt> {
        let start = self.span();
        let kind = match self.peek() {
            Tok::Break => {
                self.bump();
                StmtKind::Break
            }
            Tok::Continue => {
                self.bump();
                StmtKind::Continue
            }
            Tok::Pass => {
                self.bump();
                StmtKind::Pass
            }
            Tok::Return => {
                self.bump();
                if self.at(&Tok::Newline) || self.at(&Tok::Eof) || self.at(&Tok::Dedent) {
                    StmtKind::Return(None)
                } else {
                    StmtKind::Return(Some(self.expr()?))
                }
            }
            _ => {
                let target = self.expr()?;
                let op = match self.peek() {
                    Tok::Assign => Some(None),
                    Tok::PlusEq => Some(Some(BinOp::Add)),
                    Tok::MinusEq => Some(Some(BinOp::Sub)),
                    Tok::StarEq => Some(Some(BinOp::Mul)),
                    Tok::SlashEq => Some(Some(BinOp::Div)),
                    Tok::PercentEq => Some(Some(BinOp::Mod)),
                    _ => None,
                };
                match op {
                    Some(op) => {
                        self.bump();
                        let value = self.expr()?;
                        StmtKind::Assign { target, op, value }
                    }
                    None => StmtKind::Expr(target),
                }
            }
        };
        let span = start.merge(self.prev_span);
        self.expect_newline()?;
        Ok(Stmt { kind, span })
    }

    fn let_stmt(&mut self) -> PResult<Stmt> {
        let start = self.span();
        let mutable = matches!(self.bump().tok, Tok::Var);
        let name = self.expect_ident("variable name")?;
        let ty = if self.eat(&Tok::Colon) { Some(self.type_expr()?) } else { None };
        if !self.at(&Tok::Assign) {
            let kw = if mutable { "var" } else { "val" };
            let span = name.span;
            return self.error_diag(
                Diagnostic::error(format!("`{}` must be initialized", name.name), span)
                    .with_note("every variable in Isma needs an initial value")
                    .with_help(format!("write `{kw} {} = <value>`", name.name)),
            );
        }
        self.bump();
        let value = self.expr()?;
        let span = start.merge(self.prev_span);
        self.expect_newline()?;
        Ok(Stmt { kind: StmtKind::Let { mutable, name, ty, value }, span })
    }

    fn if_stmt(&mut self) -> PResult<Stmt> {
        let start = self.expect(Tok::If)?;
        let mut branches = Vec::new();
        let cond = self.expr()?;
        let body = self.block()?;
        branches.push((cond, body));
        let mut else_block = None;
        loop {
            if self.eat(&Tok::Elif) {
                let cond = self.expr()?;
                let body = self.block()?;
                branches.push((cond, body));
            } else if self.eat(&Tok::Else) {
                if self.at(&Tok::If) {
                    let span = self.span();
                    return self.error_diag(
                        Diagnostic::error("`else if` is not valid Isma", span).with_help("use `elif`"),
                    );
                }
                else_block = Some(self.block()?);
                break;
            } else {
                break;
            }
        }
        Ok(Stmt { kind: StmtKind::If { branches, else_block }, span: start.merge(self.prev_span) })
    }

    fn for_stmt(&mut self) -> PResult<Stmt> {
        let start = self.expect(Tok::For)?;
        let var = self.expect_ident("loop variable")?;
        self.expect(Tok::In)?;
        let first = self.expr()?;
        let iter = if self.at(&Tok::DotDot) || self.at(&Tok::DotDotEq) {
            let inclusive = matches!(self.bump().tok, Tok::DotDotEq);
            let end = self.expr()?;
            ForIter::Range { start: first, end, inclusive }
        } else {
            ForIter::Expr(first)
        };
        let body = self.block()?;
        Ok(Stmt { kind: StmtKind::For { var, iter, body }, span: start.merge(self.prev_span) })
    }

    // ---------- expressions ----------

    fn expr(&mut self) -> PResult<Expr> {
        self.or_expr()
    }

    fn binary(op: BinOp, l: Expr, r: Expr) -> Expr {
        let span = l.span.merge(r.span);
        Expr { kind: ExprKind::Binary(op, Box::new(l), Box::new(r)), span }
    }

    fn or_expr(&mut self) -> PResult<Expr> {
        let mut l = self.and_expr()?;
        while self.eat(&Tok::Or) {
            let r = self.and_expr()?;
            l = Self::binary(BinOp::Or, l, r);
        }
        Ok(l)
    }

    fn and_expr(&mut self) -> PResult<Expr> {
        let mut l = self.not_expr()?;
        while self.eat(&Tok::And) {
            let r = self.not_expr()?;
            l = Self::binary(BinOp::And, l, r);
        }
        Ok(l)
    }

    fn not_expr(&mut self) -> PResult<Expr> {
        if self.at(&Tok::Not) {
            let start = self.bump().span;
            let e = self.not_expr()?;
            let span = start.merge(e.span);
            return Ok(Expr { kind: ExprKind::Unary(UnOp::Not, Box::new(e)), span });
        }
        self.cmp_expr()
    }

    fn cmp_op(&self) -> Option<BinOp> {
        Some(match self.peek() {
            Tok::EqEq => BinOp::Eq,
            Tok::NotEq => BinOp::Ne,
            Tok::Lt => BinOp::Lt,
            Tok::Le => BinOp::Le,
            Tok::Gt => BinOp::Gt,
            Tok::Ge => BinOp::Ge,
            _ => return None,
        })
    }

    fn cmp_expr(&mut self) -> PResult<Expr> {
        let l = self.elvis_expr()?;
        if let Some(op) = self.cmp_op() {
            self.bump();
            let r = self.elvis_expr()?;
            let e = Self::binary(op, l, r);
            if self.cmp_op().is_some() {
                let span = self.span();
                return self.error_diag(
                    Diagnostic::error("comparison operators cannot be chained", span)
                        .with_label(e.span, "this comparison already produces a `Bool`")
                        .with_help("split it up, e.g. `a < b and b < c`"),
                );
            }
            return Ok(e);
        }
        Ok(l)
    }

    /// `?` immediately followed by `:`.
    fn at_elvis(&self) -> bool {
        self.at(&Tok::Question)
            && matches!(self.peek_nth(1), Tok::Colon)
            && self.tokens[self.pos].span.end == self.tokens[(self.pos + 1).min(self.tokens.len() - 1)].span.start
    }

    /// `a ?: b` binds tighter than comparisons and looser than `+`, and groups
    /// to the right: `a ?: b ?: c` is `a ?: (b ?: c)`.
    fn elvis_expr(&mut self) -> PResult<Expr> {
        let l = self.add_expr()?;
        if self.at_elvis() {
            self.bump();
            self.bump();
            let r = self.elvis_rhs()?;
            let span = l.span.merge(r.span);
            return Ok(Expr { kind: ExprKind::Elvis(Box::new(l), Box::new(r)), span });
        }
        if self.at(&Tok::Question) {
            let span = self.span();
            return self.error_diag(
                Diagnostic::error("unexpected `?`", span)
                    .with_primary("not valid here")
                    .with_note("`?` makes a type nullable (`Int?`); in expressions it is part of `?.` and `?:`")
                    .with_help("Isma has no `c ? a : b` operator; use an `if` statement"),
            );
        }
        Ok(l)
    }

    /// The right side of `?:`: a value, or `return`/`break`/`continue` to leave when the left side is null.
    fn elvis_rhs(&mut self) -> PResult<Expr> {
        let start = self.span();
        let jump = match self.peek() {
            Tok::Return => {
                self.bump();
                let value = if matches!(
                    self.peek(),
                    Tok::Newline | Tok::Eof | Tok::Dedent | Tok::RParen | Tok::RBracket | Tok::Comma | Tok::Colon
                ) {
                    None
                } else {
                    Some(Box::new(self.expr()?))
                };
                Jump::Return(value)
            }
            Tok::Break => {
                self.bump();
                Jump::Break
            }
            Tok::Continue => {
                self.bump();
                Jump::Continue
            }
            _ => return self.elvis_expr(),
        };
        Ok(Expr { kind: ExprKind::Jump(jump), span: start.merge(self.prev_span) })
    }

    fn add_expr(&mut self) -> PResult<Expr> {
        let mut l = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let r = self.mul_expr()?;
            l = Self::binary(op, l, r);
        }
        Ok(l)
    }

    fn mul_expr(&mut self) -> PResult<Expr> {
        let mut l = self.unary_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => break,
            };
            self.bump();
            let r = self.unary_expr()?;
            l = Self::binary(op, l, r);
        }
        Ok(l)
    }

    fn unary_expr(&mut self) -> PResult<Expr> {
        if self.at(&Tok::Minus) {
            let start = self.bump().span;
            // Fold negative literals (also makes `-9223372036854775808` work).
            match self.peek().clone() {
                Tok::Int(v) if matches!(self.peek_nth(1), t if !is_postfix_start(t)) => {
                    let span = start.merge(self.bump().span);
                    let value = if v == 1u64 << 63 { i64::MIN } else { -(v as i64) };
                    return Ok(Expr { kind: ExprKind::Int(value), span });
                }
                Tok::Float(v) if matches!(self.peek_nth(1), t if !is_postfix_start(t)) => {
                    let span = start.merge(self.bump().span);
                    return Ok(Expr { kind: ExprKind::Float(-v), span });
                }
                _ => {}
            }
            let e = self.unary_expr()?;
            let span = start.merge(e.span);
            return Ok(Expr { kind: ExprKind::Unary(UnOp::Neg, Box::new(e)), span });
        }
        self.postfix_expr()
    }

    fn postfix_expr(&mut self) -> PResult<Expr> {
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                Tok::LParen => {
                    self.bump();
                    let mut args = Vec::new();
                    while !self.at(&Tok::RParen) {
                        let name = if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_nth(1), Tok::Assign) {
                            let id = self.expect_ident("argument name")?;
                            self.bump(); // `=`
                            Some(id)
                        } else {
                            None
                        };
                        let value = self.expr()?;
                        args.push(Arg { name, value });
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::RParen)?;
                    let span = e.span.merge(self.prev_span);
                    e = Expr { kind: ExprKind::Call { callee: Box::new(e), args }, span };
                }
                Tok::Dot => {
                    self.bump();
                    let name = self.expect_ident("field or method name")?;
                    let span = e.span.merge(name.span);
                    e = Expr { kind: ExprKind::Field(Box::new(e), name), span };
                }
                Tok::QuestionDot => {
                    self.bump();
                    let name = self.expect_ident("field or method name after `?.`")?;
                    let span = e.span.merge(name.span);
                    e = Expr { kind: ExprKind::SafeField(Box::new(e), name), span };
                }
                Tok::BangBang => {
                    let end = self.bump().span;
                    let span = e.span.merge(end);
                    e = Expr { kind: ExprKind::NotNull(Box::new(e)), span };
                }
                Tok::LBracket => {
                    self.bump();
                    let idx = self.expr()?;
                    self.expect(Tok::RBracket)?;
                    let span = e.span.merge(self.prev_span);
                    e = Expr { kind: ExprKind::Index(Box::new(e), Box::new(idx)), span };
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn primary(&mut self) -> PResult<Expr> {
        let span = self.span();
        let kind = match self.peek().clone() {
            Tok::Int(v) => {
                self.bump();
                if v > i64::MAX as u64 {
                    return self.error("integer literal is too large for `Int` (64-bit)", span);
                }
                ExprKind::Int(v as i64)
            }
            Tok::Float(v) => {
                self.bump();
                ExprKind::Float(v)
            }
            Tok::Str(s) => {
                self.bump();
                ExprKind::Str(s)
            }
            Tok::True => {
                self.bump();
                ExprKind::Bool(true)
            }
            Tok::False => {
                self.bump();
                ExprKind::Bool(false)
            }
            Tok::Null => {
                self.bump();
                ExprKind::Null
            }
            Tok::FStr(parts) => {
                self.bump();
                let mut out = Vec::new();
                for part in parts {
                    match part {
                        FPart::Lit(s) => out.push(FStrPart::Lit(s)),
                        FPart::Expr(src, offset) => {
                            let e = self.sub_expr(&src, offset as usize);
                            out.push(FStrPart::Expr(e));
                        }
                    }
                }
                ExprKind::FStr(out)
            }
            Tok::Ident(name) => {
                self.bump();
                ExprKind::Name(name)
            }
            Tok::LParen => {
                self.bump();
                let e = self.expr()?;
                if self.at(&Tok::Comma) {
                    let span = self.span();
                    return self.error("tuples are not supported yet", span);
                }
                self.expect(Tok::RParen)?;
                return Ok(Expr { kind: e.kind, span: span.merge(self.prev_span) });
            }
            Tok::LBracket => {
                self.bump();
                let mut elems = Vec::new();
                while !self.at(&Tok::RBracket) {
                    elems.push(self.expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RBracket)?;
                ExprKind::List(elems)
            }
            _ => return self.unexpected("an expression"),
        };
        Ok(Expr { kind, span: span.merge(self.prev_span) })
    }

    /// Parse an f-string interpolation `{...}`.
    fn sub_expr(&mut self, src: &str, offset: usize) -> Expr {
        let (tokens, mut diags) = lexer::lex_expr(src, offset);
        self.diags.append(&mut diags);
        let mut sub = Parser::new(tokens);
        let result = sub.expr();
        if result.is_ok() && !sub.at(&Tok::Eof) {
            let _ = sub.unexpected::<()>("`}` (end of f-string expression)");
        }
        self.diags.append(&mut sub.diags);
        let span = Span::new(offset, offset + src.len());
        result.unwrap_or(Expr { kind: ExprKind::Error, span })
    }
}

fn is_postfix_start(t: &Tok) -> bool {
    matches!(t, Tok::Dot | Tok::LParen | Tok::LBracket | Tok::QuestionDot | Tok::BangBang)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> Program {
        let (p, d) = parse(src);
        assert!(d.is_empty(), "unexpected diagnostics: {:#?}", d);
        p
    }

    fn first_fn(p: &Program) -> &FnDecl {
        match &p.items[0] {
            Item::Fn(f) => f,
            _ => panic!("expected fn"),
        }
    }

    fn show(e: &Expr) -> String {
        match &e.kind {
            ExprKind::Int(v) => v.to_string(),
            ExprKind::Name(n) => n.clone(),
            ExprKind::Binary(op, l, r) => format!("({} {} {})", show(l), op.symbol(), show(r)),
            ExprKind::Unary(UnOp::Neg, e) => format!("(-{})", show(e)),
            ExprKind::Unary(UnOp::Not, e) => format!("(not {})", show(e)),
            ExprKind::Call { callee, args } => {
                let a: Vec<_> = args.iter().map(|a| show(&a.value)).collect();
                format!("{}({})", show(callee), a.join(", "))
            }
            ExprKind::Field(e, f) => format!("{}.{}", show(e), f.name),
            ExprKind::SafeField(e, f) => format!("{}?.{}", show(e), f.name),
            ExprKind::NotNull(e) => format!("{}!!", show(e)),
            ExprKind::Elvis(a, b) => format!("({} ?: {})", show(a), show(b)),
            ExprKind::Index(e, i) => format!("{}[{}]", show(e), show(i)),
            other => format!("{other:?}"),
        }
    }

    fn expr_of(src: &str) -> String {
        let p = parse_ok(&format!("fn main():\n    {src}\n"));
        match &first_fn(&p).body[0].kind {
            StmtKind::Expr(e) => show(e),
            other => panic!("expected expr stmt, got {other:?}"),
        }
    }

    #[test]
    fn precedence() {
        assert_eq!(expr_of("1 + 2 * 3 - 4"), "((1 + (2 * 3)) - 4)");
        assert_eq!(expr_of("a or b and not c == d"), "(a or (b and (not (c == d))))");
        assert_eq!(expr_of("-a * b"), "((-a) * b)");
        assert_eq!(expr_of("a.b(c)[d].e"), "a.b(c)[d].e");
        assert_eq!(expr_of("-5"), "-5");
    }

    #[test]
    fn chained_comparison_rejected() {
        let (_, d) = parse("fn main():\n    a < b < c\n");
        assert!(d.iter().any(|d| d.message.contains("chained")));
    }

    #[test]
    fn if_elif_else() {
        let p = parse_ok("fn main():\n    if a:\n        pass\n    elif b:\n        pass\n    else:\n        pass\n");
        match &first_fn(&p).body[0].kind {
            StmtKind::If { branches, else_block } => {
                assert_eq!(branches.len(), 2);
                assert!(else_block.is_some());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn struct_with_methods() {
        let p = parse_ok(
            "struct P:\n    var x: Int\n    y: Int\n\n    fn sum(self) -> Int:\n        return self.x + self.y\n",
        );
        match &p.items[0] {
            Item::Struct(s) => {
                assert_eq!(s.fields.len(), 2);
                assert!(s.fields[0].mutable);
                assert!(!s.fields[1].mutable);
                assert_eq!(s.methods.len(), 1);
                assert!(s.methods[0].self_param.is_some());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn recovers_from_multiple_errors() {
        let (_, d) = parse("fn main():\n    val = 3\n    print(1 +)\n    val ok = 1\n");
        assert_eq!(d.len(), 2, "{d:#?}");
    }

    #[test]
    fn null_safety_syntax() {
        assert_eq!(expr_of("a ?: b ?: c"), "(a ?: (b ?: c))");
        assert_eq!(expr_of("a?.b?.c(d)!!.e"), "a?.b?.c(d)!!.e");
        let p = parse_ok("fn f(x: Int?) -> List[String?]?:\n    val y = x?.foo?.bar(1)!! ?: return null\n    return a ?: b + 1 > 2\n");
        let f = first_fn(&p);
        assert!(f.params[0].ty.nullable);
        let ret = f.ret.as_ref().unwrap();
        assert!(ret.nullable && ret.args[0].nullable);
        match &f.body[0].kind {
            StmtKind::Let { value, .. } => match &value.kind {
                ExprKind::Elvis(l, r) => {
                    assert!(matches!(l.kind, ExprKind::NotNull(_)));
                    assert!(matches!(r.kind, ExprKind::Jump(Jump::Return(Some(_)))));
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        // `?:` binds tighter than `>` and looser than `+`.
        match &f.body[1].kind {
            StmtKind::Return(Some(e)) => match &e.kind {
                ExprKind::Binary(BinOp::Gt, l, _) => assert!(matches!(&l.kind, ExprKind::Elvis(_, r) if matches!(r.kind, ExprKind::Binary(BinOp::Add, ..)))),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ternary_is_rejected() {
        let (_, d) = parse("fn main():\n    val x = c ? 1 : 2\n");
        assert!(d.iter().any(|d| d.message.contains("unexpected `?`")), "{d:#?}");
    }

    #[test]
    fn one_line_blocks_and_ranges() {
        let p = parse_ok("fn main():\n    for i in 0..=10: print(i)\n    if x: return\n");
        assert_eq!(first_fn(&p).body.len(), 2);
    }
}
