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
    /// The token position right after a block lambda's body: the line that
    /// holds the lambda already ended there, so no newline is expected.
    block_end: Option<usize>,
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
        Parser { tokens, pos: 0, prev_span: Span::default(), block_end: None, diags: Vec::new() }
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
        if self.eat(&Tok::Newline) || self.at(&Tok::Eof) || self.block_end == Some(self.pos) {
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
                // `@get("/")` lines, then what they're on.
                Tok::At => match self.attributed_item() {
                    Ok(Some(item)) => items.push(item),
                    Ok(None) => {}
                    Err(()) => self.synchronize(),
                },
                // `comptime fn`: only a keyword before `fn`.
                Tok::Ident(n) if n == "comptime" && matches!(self.peek_nth(1), Tok::Fn) => {
                    self.bump();
                    match self.fn_decl() {
                        Ok(mut f) => {
                            f.comptime = true;
                            items.push(Item::Fn(f));
                        }
                        Err(()) => self.synchronize(),
                    }
                }
                // `attribute` is only a keyword here, before a name.
                Tok::Ident(n) if n == "attribute" && matches!(self.peek_nth(1), Tok::Ident(_)) => match self.attribute_decl() {
                    Ok(a) => items.push(Item::Attribute(a)),
                    Err(()) => self.synchronize(),
                },
                Tok::Struct => match self.struct_decl() {
                    Ok(s) => items.push(Item::Struct(s)),
                    Err(()) => self.synchronize(),
                },
                Tok::Enum => match self.enum_decl() {
                    Ok(e) => items.push(Item::Enum(e)),
                    Err(()) => self.synchronize(),
                },
                Tok::Import => match self.import_decl() {
                    Ok(i) => items.push(Item::Import(i)),
                    Err(()) => self.synchronize(),
                },
                // `interface` is only a keyword here, before a name.
                Tok::Ident(n) if n == "interface" && matches!(self.peek_nth(1), Tok::Ident(_)) => match self.interface_decl() {
                    Ok(i) => items.push(Item::Interface(i)),
                    Err(()) => self.synchronize(),
                },
                Tok::Ident(n) if n == "from" && matches!(self.peek_nth(1), Tok::Ident(_) | Tok::Dot | Tok::DotDot) => match self.parse_from_import() {
                    Ok(i) => items.push(Item::Import(i)),
                    Err(()) => self.synchronize(),
                },
                Tok::Indent => {
                    let span = self.span();
                    self.diags.push(Diagnostic::error("unexpected indentation", span));
                    self.skip_block();
                }
                Tok::Dedent => {
                    self.bump();
                }
                Tok::Val => match self.let_stmt() {
                    Ok(Stmt { kind: StmtKind::Let { name, ty, value, .. }, span }) => items.push(Item::Const(ConstDecl { name, ty, value, span })),
                    Ok(_) => unreachable!(),
                    Err(()) => self.synchronize(),
                },
                Tok::Var => {
                    let span = self.span();
                    self.diags.push(
                        Diagnostic::error("Tarn has no global variables", span)
                            .with_primary("a top-level `var`")
                            .with_note("a top-level `val` is a constant (a number, `Bool` or `String`), but nothing at the top level can change")
                            .with_help("keep the value in `main` (or a struct) and pass it to the functions that need it"),
                    );
                    self.synchronize();
                }
                _ => {
                    let span = self.span();
                    let found = self.peek().clone();
                    self.diags.push(
                        Diagnostic::error(format!("expected `fn`, `struct`, `enum`, `interface`, `attribute` or `val`, found {found}"), span)
                            .with_note("only functions, structs, enums, interfaces, attributes and constants (`val`) may appear at the top level")
                            .with_help("put statements inside `fn main():`"),
                    );
                    self.synchronize();
                }
            }
        }
        Program { items }
    }

    // ---------- attributes ----------

    /// `@name`, `@name(args)` or `@module.name(args)`.
    fn attribute_use(&mut self) -> PResult<Attribute> {
        let start = self.expect(Tok::At)?;
        let first = self.expect_ident("an attribute name after `@`")?;
        let (module, name) = if self.eat(&Tok::Dot) { (Some(first), self.expect_ident("an attribute name")?) } else { (None, first) };
        let args = if self.at(&Tok::LParen) {
            self.bump();
            self.call_args()?
        } else {
            Vec::new()
        };
        Ok(Attribute { module, name, args, span: start.merge(self.prev_span) })
    }

    /// Attribute lines above a declaration, one per line.
    fn attribute_lines(&mut self) -> PResult<Vec<Attribute>> {
        let mut attrs = Vec::new();
        while self.at(&Tok::At) {
            attrs.push(self.attribute_use()?);
            self.expect_newline()?;
            while self.eat(&Tok::Newline) {}
        }
        Ok(attrs)
    }

    fn misplaced_attributes<T>(&mut self, attrs: &[Attribute]) -> PResult<T> {
        let span = attrs.first().map(|a| a.span).unwrap_or(self.span());
        let found = self.peek().clone();
        self.error_diag(
            Diagnostic::error(format!("an attribute must be followed by a function, struct, enum or field, found {found}"), span)
                .with_primary("this attribute isn't on anything")
                .with_note("attributes go on the lines just above a declaration (or before a parameter's name)"),
        )
    }

    /// `@get("/")` lines at the top level, and the declaration under them.
    fn attributed_item(&mut self) -> PResult<Option<Item>> {
        let attrs = self.attribute_lines()?;
        let comptime = matches!(self.peek(), Tok::Ident(n) if n == "comptime") && matches!(self.peek_nth(1), Tok::Fn);
        if comptime {
            self.bump();
        }
        match self.peek() {
            Tok::Fn => {
                let mut f = self.fn_decl()?;
                f.attrs = attrs;
                f.comptime = comptime;
                Ok(Some(Item::Fn(f)))
            }
            Tok::Struct => {
                let mut s = self.struct_decl()?;
                s.attrs = attrs;
                Ok(Some(Item::Struct(s)))
            }
            Tok::Enum => {
                let mut e = self.enum_decl()?;
                e.attrs = attrs;
                Ok(Some(Item::Enum(e)))
            }
            _ => self.misplaced_attributes(&attrs),
        }
    }

    /// `attribute get(path: String)`, or `attribute skip`.
    fn attribute_decl(&mut self) -> PResult<AttributeDecl> {
        let start = self.bump().span; // `attribute`
        let name = self.expect_ident("attribute name")?;
        let mut fields = Vec::new();
        if self.eat(&Tok::LParen) {
            while !self.at(&Tok::RParen) {
                if matches!(self.peek(), Tok::Var | Tok::Val) {
                    let span = self.span();
                    return self.error_diag(
                        Diagnostic::error("an attribute's fields are always immutable", span)
                            .with_primary("remove this")
                            .with_note("an attribute's value is worked out while compiling and never changes"),
                    );
                }
                fields.push(self.field_head(false)?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen)?;
        }
        if self.at(&Tok::Colon) {
            let span = self.span();
            return self.error_diag(
                Diagnostic::error("an attribute has no body", span)
                    .with_help(format!("list its fields in parentheses: `attribute {}(path: String)`", name.name)),
            );
        }
        let span = start.merge(self.prev_span);
        self.expect_newline()?;
        Ok(AttributeDecl { name, fields, span })
    }

    /// `import time`, `import shapes.circle`, `import geometry as geo`
    fn import_decl(&mut self) -> PResult<Import> {
        let start = self.expect(Tok::Import)?;
        if self.at(&Tok::Dot) || self.at(&Tok::DotDot) {
            let span = self.span();
            return self.error_diag(
                Diagnostic::error("relative imports use `from`", span)
                    .with_primary("not allowed after `import`")
                    .with_help("write `from . import name` (this folder) or `from .. import name` (the folder above)"),
            );
        }
        let path = self.module_path("a module name, e.g. `import time`")?;
        if self.at(&Tok::Comma) {
            let span = self.span();
            return self.error_diag(
                Diagnostic::error("import one module per line", span).with_help("write each `import` on its own line"),
            );
        }
        let alias = self.import_alias()?;
        let span = start.merge(self.prev_span);
        self.expect_newline()?;
        Ok(Import { level: 0, path, alias, names: None, span })
    }

    /// `from geometry import Point, area as a`, `from .. import util`
    fn parse_from_import(&mut self) -> PResult<Import> {
        let start = self.bump().span; // `from`
        // `..` lexes as one token (the range operator).
        let mut level = 0;
        loop {
            match self.peek() {
                Tok::Dot => level += 1,
                Tok::DotDot => level += 2,
                _ => break,
            }
            self.bump();
        }
        let path = if level > 0 && self.at(&Tok::Import) { Vec::new() } else { self.module_path("a module name after `from`")? };
        if !self.at(&Tok::Import) {
            return self.unexpected("`import`");
        }
        self.bump();
        if self.at(&Tok::Star) {
            let span = self.span();
            return self.error_diag(
                Diagnostic::error("`import *` is not supported", span)
                    .with_primary("name what you need instead")
                    .with_help("name what you use, e.g. `from geometry import Point, area`, or import the module: `import geometry`"),
            );
        }
        let parens = self.eat(&Tok::LParen);
        let mut names = Vec::new();
        loop {
            if parens && self.at(&Tok::RParen) && !names.is_empty() {
                break;
            }
            let name = self.expect_ident("a name to import")?;
            let alias = self.import_alias()?;
            names.push(ImportName { name, alias });
            if !self.eat(&Tok::Comma) {
                break;
            }
            if !parens && matches!(self.peek(), Tok::Newline | Tok::Eof) {
                break; // trailing comma
            }
        }
        if parens {
            self.expect(Tok::RParen)?;
        }
        let span = start.merge(self.prev_span);
        self.expect_newline()?;
        Ok(Import { level, path, alias: None, names: Some(names), span })
    }

    /// Is there an `import` keyword before the end of the current line?
    fn import_on_this_line(&self) -> bool {
        self.tokens[self.pos..].iter().take_while(|t| !matches!(t.tok, Tok::Newline | Tok::Eof)).any(|t| matches!(t.tok, Tok::Import))
    }

    /// `a.b.c`
    fn module_path(&mut self, what: &str) -> PResult<Vec<Ident>> {
        let mut path = vec![self.expect_ident(what)?];
        while self.eat(&Tok::Dot) {
            path.push(self.expect_ident("a module name after `.`")?);
        }
        Ok(path)
    }

    /// `as name`
    fn import_alias(&mut self) -> PResult<Option<Ident>> {
        if matches!(self.peek(), Tok::Ident(n) if n == "as") {
            self.bump();
            return Ok(Some(self.expect_ident("a name after `as`")?));
        }
        Ok(None)
    }

    /// A function's name: an identifier, or an operator (`fn +(self, other: T)`).
    fn fn_name(&mut self) -> PResult<Ident> {
        let start = self.span();
        let name = match self.peek() {
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::EqEq => "==",
            Tok::Spaceship => "<=>",
            Tok::LBracket => {
                self.bump();
                self.expect(Tok::RBracket)?;
                let name = if self.eat(&Tok::Assign) { "[]=" } else { "[]" };
                return Ok(Ident { name: name.into(), span: start.merge(self.prev_span) });
            }
            Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge | Tok::NotEq => {
                let found = self.peek().clone();
                return self.error_diag(
                    Diagnostic::error(format!("{found} can't be defined on its own"), start)
                        .with_primary("not an operator function")
                        .with_help(if matches!(found, Tok::NotEq) {
                            "define `fn ==(self, other: T) -> Bool`: `!=` is its opposite"
                        } else {
                            "define `fn <=>(self, other: T) -> Int` (negative, 0 or positive): it gives `<`, `<=`, `>` and `>=`"
                        }),
                );
            }
            _ => return self.expect_ident("function name"),
        };
        self.bump();
        Ok(Ident { name: name.into(), span: start })
    }

    /// `[T, U]` after the name of a generic function, struct or enum (nothing
    /// if there's no `[`). A parameter can have bounds: `[T: Shape + Named]`.
    fn type_params(&mut self) -> PResult<Vec<TypeParam>> {
        if !self.at(&Tok::LBracket) {
            return Ok(Vec::new());
        }
        let open = self.bump().span;
        let mut params = Vec::new();
        while !self.at(&Tok::RBracket) {
            let name = self.expect_ident("a type parameter name, like `T`")?;
            let mut bounds = Vec::new();
            if self.eat(&Tok::Colon) {
                loop {
                    bounds.push(self.type_expr()?);
                    if !self.eat(&Tok::Plus) {
                        break;
                    }
                }
            }
            params.push(TypeParam { name, bounds });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RBracket)?;
        if params.is_empty() {
            return self.error_diag(
                Diagnostic::error("empty list of type parameters", open.merge(self.prev_span))
                    .with_help("name at least one, like `[T]`, or remove the brackets"),
            );
        }
        Ok(params)
    }

    /// `(Shape, Named)` after a type's name: the interfaces it says it fits
    /// (or, for an interface, the ones whose methods it includes).
    fn interface_list(&mut self) -> PResult<Vec<TypeExpr>> {
        if !self.eat(&Tok::LParen) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        while !self.at(&Tok::RParen) {
            out.push(self.type_expr()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        Ok(out)
    }

    fn fn_decl(&mut self) -> PResult<FnDecl> {
        let mut f = self.fn_sig()?;
        f.body = self.block()?;
        Ok(f)
    }

    /// `fn name[T](params) -> Ret`, without the body (left empty).
    fn fn_sig(&mut self) -> PResult<FnDecl> {
        let start = self.expect(Tok::Fn)?;
        let name = self.fn_name()?;
        let type_params = if is_operator_name(&name.name) { Vec::new() } else { self.type_params()? };
        if self.at(&Tok::LBracket) && is_operator_name(&name.name) {
            let span = self.span();
            return self.error_diag(
                Diagnostic::error("operator functions can't have type parameters", span)
                    .with_note("they can use the type parameters of the generic type they belong to"),
            );
        }
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
                // `@query page: Int`
                let mut attrs = Vec::new();
                while self.at(&Tok::At) {
                    attrs.push(self.attribute_use()?);
                }
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
                params.push(Param { name: pname, ty, attrs });
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        let ret = if self.eat(&Tok::Arrow) { Some(self.type_expr()?) } else { None };
        let sig_span = start.merge(self.prev_span);
        Ok(FnDecl { name, type_params, self_param, params, ret, body: Vec::new(), sig_span, attrs: Vec::new(), comptime: false })
    }

    /// `interface Shape:` and its method signatures, one per line.
    fn interface_decl(&mut self) -> PResult<InterfaceDecl> {
        let start = self.bump().span; // `interface`
        let name = self.expect_ident("interface name")?;
        let type_params = self.type_params()?;
        if let Some(b) = type_params.iter().find_map(|t| t.bounds.first()) {
            let span = b.span;
            return self.error_diag(
                Diagnostic::error("an interface's type parameters can't have bounds", span)
                    .with_primary("remove this")
                    .with_help("put the bound on the generic code that uses the interface"),
            );
        }
        let extends = self.interface_list()?;
        self.expect(Tok::Colon)?;
        self.expect(Tok::Newline)?;
        if !self.at(&Tok::Indent) {
            return self.unexpected("an indented list of methods");
        }
        self.bump();
        let mut methods = Vec::new();
        let mut defaults = Vec::new();
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
                Tok::Fn => match self.interface_method() {
                    Ok(f) if !f.body.is_empty() => defaults.push(f),
                    Ok(f) => methods.push(f),
                    Err(()) => self.synchronize(),
                },
                Tok::Val | Tok::Var | Tok::Ident(_) => {
                    let span = self.span();
                    self.diags.push(
                        Diagnostic::error("interfaces list methods, not fields", span)
                            .with_primary("a field")
                            .with_help("ask for a method instead, e.g. `fn name(self) -> String`, and give each type that method"),
                    );
                    self.synchronize();
                }
                _ => {
                    let _ = self.unexpected::<()>("a method signature (`fn name(self) -> Type`)");
                    self.synchronize();
                }
            }
        }
        let end = self.prev_span;
        self.eat(&Tok::Dedent);
        Ok(InterfaceDecl { span: start.merge(end), name, type_params, extends, methods, defaults })
    }

    /// `fn area(self) -> Float` in an interface: a signature on a line of its
    /// own (a method each type must have), or a default method with a body.
    fn interface_method(&mut self) -> PResult<FnDecl> {
        let mut f = self.fn_sig()?;
        if self.at(&Tok::Colon) {
            f.body = self.block()?;
            if f.body.is_empty() {
                // (`pass`: a default method that does nothing.)
                f.body.push(Stmt { kind: StmtKind::Pass, span: self.prev_span });
            }
            return Ok(f);
        }
        self.expect_newline()?;
        Ok(f)
    }

    fn struct_decl(&mut self) -> PResult<StructDecl> {
        self.expect(Tok::Struct)?;
        let name = self.expect_ident("struct name")?;
        let type_params = self.type_params()?;
        let implements = self.interface_list()?;
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
                Tok::Ident(n) if n == "comptime" && matches!(self.peek_nth(1), Tok::Fn) => {
                    self.bump();
                    match self.fn_decl() {
                        Ok(mut f) => {
                            f.comptime = true;
                            methods.push(f);
                        }
                        Err(()) => self.synchronize(),
                    }
                }
                Tok::At => {
                    let r = self.attribute_lines().and_then(|attrs| {
                        let comptime = matches!(self.peek(), Tok::Ident(n) if n == "comptime") && matches!(self.peek_nth(1), Tok::Fn);
                        if comptime {
                            self.bump();
                        }
                        match self.peek() {
                            Tok::Fn => {
                                let mut f = self.fn_decl()?;
                                f.attrs = attrs;
                                f.comptime = comptime;
                                methods.push(f);
                            }
                            Tok::Val | Tok::Var | Tok::Ident(_) => {
                                let mut f = self.field_decl()?;
                                f.attrs = attrs;
                                fields.push(f);
                            }
                            _ => return self.misplaced_attributes(&attrs),
                        }
                        Ok(())
                    });
                    if r.is_err() {
                        self.synchronize();
                    }
                }
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
        Ok(StructDecl { name, type_params, implements, fields, methods, attrs: Vec::new() })
    }

    fn enum_decl(&mut self) -> PResult<EnumDecl> {
        self.expect(Tok::Enum)?;
        let name = self.expect_ident("enum name")?;
        let type_params = self.type_params()?;
        let implements = self.interface_list()?;
        self.expect(Tok::Colon)?;
        self.expect(Tok::Newline)?;
        if !self.at(&Tok::Indent) {
            return self.unexpected("an indented list of variants");
        }
        self.bump();
        let mut variants = Vec::new();
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
                Tok::At => {
                    let r = self.attribute_lines().and_then(|attrs| {
                        if !self.at(&Tok::Fn) {
                            if matches!(self.peek(), Tok::Ident(_)) {
                                let span = attrs[0].span;
                                return self.error_diag(
                                    Diagnostic::error("attributes on enum variants aren't supported yet", span)
                                        .with_note("attributes go on functions, methods, structs, enums, fields and parameters"),
                                );
                            }
                            return self.misplaced_attributes(&attrs);
                        }
                        let mut f = self.fn_decl()?;
                        f.attrs = attrs;
                        methods.push(f);
                        Ok(())
                    });
                    if r.is_err() {
                        self.synchronize();
                    }
                }
                Tok::Ident(_) => match self.variant_decl() {
                    Ok(v) => variants.push(v),
                    Err(()) => self.synchronize(),
                },
                Tok::Val | Tok::Var => {
                    let span = self.span();
                    self.diags.push(
                        Diagnostic::error("enums don't have fields of their own", span)
                            .with_help("give the data to a variant: `Circle(radius: Float)`"),
                    );
                    self.synchronize();
                }
                _ => {
                    let _ = self.unexpected::<()>("a variant or method");
                    self.synchronize();
                }
            }
        }
        self.eat(&Tok::Dedent);
        Ok(EnumDecl { name, type_params, implements, variants, methods, attrs: Vec::new() })
    }

    /// `Circle(radius: Float)` or `Empty`
    fn variant_decl(&mut self) -> PResult<VariantDecl> {
        let name = self.expect_ident("variant name")?;
        let mut fields = Vec::new();
        if self.eat(&Tok::LParen) {
            while !self.at(&Tok::RParen) {
                if matches!(self.peek(), Tok::Var | Tok::Val) {
                    let span = self.span();
                    return self.error_diag(
                        Diagnostic::error("variant fields are always immutable", span)
                            .with_primary("remove this")
                            .with_note("to change a value, build a new one: `Shape.Circle(r * 2.0)`"),
                    );
                }
                let fname = self.expect_ident("field name")?;
                if !self.at(&Tok::Colon) {
                    let span = fname.span;
                    return self.error_diag(
                        Diagnostic::error(format!("field `{}` needs a type annotation", fname.name), span)
                            .with_help(format!("write it as `{}: Int` (or whichever type it is)", fname.name)),
                    );
                }
                self.bump();
                let ty = self.type_expr()?;
                if self.at(&Tok::Assign) {
                    let span = self.span();
                    return self.error_diag(
                        Diagnostic::error("variant fields can't have default values", span)
                            .with_primary("only struct fields can")
                            .with_help("add a static method that builds the variant with the usual values"),
                    );
                }
                fields.push(FieldDecl { name: fname, ty, mutable: false, default: None, attrs: Vec::new() });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen)?;
        }
        let span = name.span.merge(self.prev_span);
        self.expect_newline()?;
        Ok(VariantDecl { name, fields, span })
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
        let f = self.field_head(mutable)?;
        self.expect_newline()?;
        Ok(f)
    }

    /// `name: Type` or `name: Type = default` (a struct's or an attribute's field).
    fn field_head(&mut self, mutable: bool) -> PResult<FieldDecl> {
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
        // `port: Int = 8080`: a hidden static method gives the value.
        let default = if self.at(&Tok::Assign) {
            let eq = self.bump().span;
            let value = self.expr()?;
            let at = Span { start: eq.start, end: eq.start };
            let span = value.span;
            let body = vec![Stmt { kind: StmtKind::Return(Some(value)), span }];
            let name = Ident { name: format!("__default_{}", name.name), span: at };
            Some(Box::new(FnDecl {
                name,
                type_params: Vec::new(),
                self_param: None,
                params: Vec::new(),
                ret: Some(ty.clone()),
                body,
                sig_span: at,
                attrs: Vec::new(),
                comptime: false,
            }))
        } else {
            None
        };
        Ok(FieldDecl { name, ty, mutable, default, attrs: Vec::new() })
    }

    fn type_expr(&mut self) -> PResult<TypeExpr> {
        // `(fn(Int) -> Int)?`: parentheses group a type (to make a function type nullable).
        if self.at(&Tok::LParen) {
            let start = self.bump().span;
            let mut inner = self.type_expr()?;
            // `(Int, String)`: a tuple type.
            if self.eat(&Tok::Comma) {
                let mut elems = vec![inner];
                while !self.at(&Tok::RParen) {
                    elems.push(self.type_expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RParen)?;
                let nullable = self.eat(&Tok::Question);
                let span = start.merge(self.prev_span);
                let name = Ident { name: TUPLE_TYPE.into(), span };
                return Ok(TypeExpr { module: None, name, args: elems, nullable, func: None, span });
            }
            self.expect(Tok::RParen)?;
            if self.eat(&Tok::Question) {
                if inner.nullable {
                    let span = self.prev_span;
                    return self.error_diag(
                        Diagnostic::error("a type can only be made nullable once", span)
                            .with_primary("remove this")
                            .with_help("`T?` already allows `null`"),
                    );
                }
                inner.nullable = true;
            }
            inner.span = start.merge(self.prev_span);
            return Ok(inner);
        }
        if self.at(&Tok::Fn) {
            return self.fn_type();
        }
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
        Ok(TypeExpr { module, name, args, nullable, func: None, span })
    }

    /// `fn(Int, String) -> Bool`, or `fn(Int)` for a function returning nothing.
    fn fn_type(&mut self) -> PResult<TypeExpr> {
        let start = self.expect(Tok::Fn)?;
        self.expect(Tok::LParen)?;
        let mut params = Vec::new();
        while !self.at(&Tok::RParen) {
            if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_nth(1), Tok::Colon) {
                let span = self.span();
                return self.error_diag(
                    Diagnostic::error("a function type lists only the parameters' types", span)
                        .with_help("write `fn(Int, String) -> Bool`, without parameter names"),
                );
            }
            params.push(self.type_expr()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        let ret = if self.eat(&Tok::Arrow) { Some(self.type_expr()?) } else { None };
        let span = start.merge(self.prev_span);
        let name = Ident { name: "fn".into(), span: start };
        Ok(TypeExpr { module: None, name, args: Vec::new(), nullable: false, func: Some(Box::new(FnTypeExpr { params, ret })), span })
    }

    /// A lambda: `fn(x) -> x * 2`, `fn(a: Int, b: Int) -> a + b`, or one with
    /// a block body, which must end its line:
    ///
    /// ```text
    /// val f = fn(x: Int) -> Int:      val g = fn(name: String):
    ///     return x * 2                    print(name)
    /// ```
    fn lambda(&mut self) -> PResult<Expr> {
        let start = self.expect(Tok::Fn)?;
        self.expect(Tok::LParen)?;
        let mut params = Vec::new();
        while !self.at(&Tok::RParen) {
            let name = self.expect_ident("a parameter name")?;
            let ty = if self.eat(&Tok::Colon) { Some(self.type_expr()?) } else { None };
            params.push(LambdaParam { name, ty });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        let head = start.merge(self.prev_span);
        // `fn(x: Int) -> Int:` + a block, or `fn(x) -> <expression>`: decided
        // by whether a type and then `:` at the end of the line follow `->`.
        let (ret, block) = if self.eat(&Tok::Arrow) {
            let (pos, prev, ndiags) = (self.pos, self.prev_span, self.diags.len());
            match self.type_expr() {
                // (`fn(x) -> x ?: 0`: a `?` right before the `:` is `?:`, the
                // elvis operator, unless the line ends there.)
                Ok(t) if self.at(&Tok::Colon)
                    && (matches!(self.peek_nth(1), Tok::Newline) || !(t.nullable && self.prev_span.end == self.span().start)) =>
                {
                    (Some(t), true)
                }
                _ => {
                    self.pos = pos;
                    self.prev_span = prev;
                    self.diags.truncate(ndiags);
                    (None, false)
                }
            }
        } else if self.at(&Tok::Colon) {
            (None, true)
        } else {
            let span = self.span();
            return self.error_diag(
                Diagnostic::error(format!("expected `->` or `:` after the lambda's parameters, found {}", self.peek()), span)
                    .with_help("write `fn(x) -> x * 2`, or end the line with `:` and put a block under it"),
            );
        };
        if !block {
            let body = self.expr()?;
            // `fn(m) -> m[k] = v`: an assignment (the lambda returns nothing).
            let op = match self.peek() {
                Tok::Assign => Some(None),
                Tok::PlusEq => Some(Some(BinOp::Add)),
                Tok::MinusEq => Some(Some(BinOp::Sub)),
                Tok::StarEq => Some(Some(BinOp::Mul)),
                Tok::SlashEq => Some(Some(BinOp::Div)),
                Tok::PercentEq => Some(Some(BinOp::Mod)),
                _ => None,
            };
            if let Some(op) = op {
                self.bump();
                let value = self.expr()?;
                let span = start.merge(value.span);
                let stmt = Stmt { kind: StmtKind::Assign { target: body, op, value }, span };
                let lam = Lambda { params, ret: None, body: LambdaBody::Block(vec![stmt]), head };
                return Ok(Expr { kind: ExprKind::Lambda(Box::new(lam)), span });
            }
            let span = start.merge(body.span);
            let lam = Lambda { params, ret: None, body: LambdaBody::Expr(Box::new(body)), head };
            return Ok(Expr { kind: ExprKind::Lambda(Box::new(lam)), span });
        }
        if !matches!(self.peek_nth(1), Tok::Newline) {
            let span = self.span();
            return self.error_diag(
                Diagnostic::error("a lambda's block must start on a new line", span)
                    .with_note("a block body can't be inside parentheses or follow other code on its line")
                    .with_help("put the lambda in a variable first (`val f = fn(x: Int):` and the block under it), or write a one-line lambda: `fn(x) -> ...`"),
            );
        }
        let body = self.block()?;
        self.block_end = Some(self.pos);
        let span = start.merge(self.prev_span);
        let lam = Lambda { params, ret, body: LambdaBody::Block(body), head };
        Ok(Expr { kind: ExprKind::Lambda(Box::new(lam)), span })
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
        if matches!(self.peek(), Tok::Ident(n) if n == "comptime")
            && (matches!(self.peek_nth(1), Tok::Val | Tok::Var | Tok::If | Tok::For | Tok::Fn | Tok::Elif | Tok::Else | Tok::While)
                || matches!(self.peek_nth(1), Tok::Ident(m) if m == "match"))
        {
            return self.comptime_stmt();
        }
        if matches!(self.peek(), Tok::Ident(n) if n == "match")
            && let Some(m) = self.try_match_stmt()
        {
            return m;
        }
        if matches!(self.peek(), Tok::Ident(n) if n == "from") && self.import_on_this_line() {
            let span = self.span();
            return self.error_diag(
                Diagnostic::error("`import` must be at the top level of the file", span)
                    .with_help("move it to the top of the file, above your functions"),
            );
        }
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
            // `fn(...)` starts a lambda (an expression); `fn name(...)` would be a nested function.
            Tok::Fn if matches!(self.peek_nth(1), Tok::LParen) => self.simple_stmt(),
            Tok::Fn => {
                let span = self.span();
                let name = match self.peek_nth(1) {
                    Tok::Ident(n) => n.clone(),
                    _ => "f".into(),
                };
                self.error_diag(
                    Diagnostic::error("functions can't be declared inside other functions", span)
                        .with_help(format!("declare it at the top level, or use a lambda: `val {name} = fn(x: Int) -> Int:`")),
                )
            }
            Tok::Struct | Tok::Enum => {
                let span = self.span();
                let what = if self.at(&Tok::Struct) { "structs" } else { "enums" };
                self.error(format!("{what} must be declared at the top level"), span)
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

    /// `comptime val`, `comptime if`, `comptime for` or `comptime match`.
    fn comptime_stmt(&mut self) -> PResult<Stmt> {
        let start = self.bump().span; // `comptime`
        let kind = match self.peek() {
            Tok::Val => {
                let Stmt { kind, span } = self.let_stmt()?;
                let StmtKind::Let { name, ty, value, .. } = kind else {
                    return self.error("`comptime val` takes a single name", span);
                };
                ComptimeStmt::Let { name, ty, value }
            }
            Tok::Var => {
                let span = self.span();
                return self.error_diag(
                    Diagnostic::error("a value worked out while compiling can't change", span)
                        .with_help("write `comptime val`"),
                );
            }
            Tok::If => {
                let Stmt { kind, .. } = self.if_stmt()?;
                let StmtKind::If { branches, else_block } = kind else { unreachable!() };
                ComptimeStmt::If { branches, else_block }
            }
            Tok::For => {
                self.bump();
                let var = self.expect_ident("loop variable")?;
                let second = if self.eat(&Tok::Comma) { Some(self.expect_ident("second loop variable")?) } else { None };
                self.expect(Tok::In)?;
                let iter = self.expr()?;
                if self.at(&Tok::DotDot) || self.at(&Tok::DotDotEq) {
                    let span = self.span();
                    return self.error_diag(
                        Diagnostic::error("`comptime for` goes over a list", span)
                            .with_help("write the numbers as a list (`[0, 1, 2]`), or loop over `fields(T)`"),
                    );
                }
                let body = self.block()?;
                ComptimeStmt::For { var, second, iter, body }
            }
            Tok::Ident(n) if n == "match" => {
                self.bump();
                let ty = self.type_expr()?;
                self.expect(Tok::Colon)?;
                if !self.eat(&Tok::Newline) || !self.at(&Tok::Indent) {
                    let span = self.span();
                    return self.error_diag(
                        Diagnostic::error("expected an indented list of `case`s", span)
                            .with_help("write each case on its own line:\n\ncomptime match T:\n    case Int:\n        ...\n    case _:\n        ..."),
                    );
                }
                self.bump();
                let mut arms = Vec::new();
                while !self.at(&Tok::Dedent) && !self.at(&Tok::Eof) {
                    if self.eat(&Tok::Newline) {
                        continue;
                    }
                    match self.type_arm() {
                        Ok(a) => arms.push(a),
                        Err(()) => self.synchronize(),
                    }
                }
                self.eat(&Tok::Dedent);
                ComptimeStmt::Match { ty, arms }
            }
            _ => {
                let span = self.span();
                let found = self.peek().clone();
                return self.error_diag(
                    Diagnostic::error(format!("expected `val`, `if`, `for` or `match` after `comptime`, found {found}"), span)
                        .with_note("`comptime fn` is only allowed at the top level and in structs"),
                );
            }
        };
        Ok(Stmt { kind: StmtKind::Comptime(Box::new(kind)), span: start.merge(self.prev_span) })
    }

    /// `case Int | Float:` in a `comptime match`: types, `_` for any.
    fn type_arm(&mut self) -> PResult<TypeArm> {
        if !matches!(self.peek(), Tok::Ident(n) if n == "case") {
            let span = self.span();
            let found = self.peek().clone();
            return self.error_diag(
                Diagnostic::error(format!("expected `case`, found {found}"), span)
                    .with_note("a `comptime match` contains only `case` lines, each with a type and an indented block"),
            );
        }
        let start = self.bump().span;
        let mut pats = vec![self.type_expr()?];
        while self.eat(&Tok::Pipe) {
            pats.push(self.type_expr()?);
        }
        let span = start.merge(self.prev_span);
        let body = self.block()?;
        Ok(TypeArm { pats, body, span })
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
        // `val (q, r) = divmod(17, 5)`
        if self.at(&Tok::LParen) {
            let pat = self.closed_pattern()?;
            let ty = if self.eat(&Tok::Colon) { Some(self.type_expr()?) } else { None };
            self.expect(Tok::Assign)?;
            let value = self.expr()?;
            let span = start.merge(self.prev_span);
            self.expect_newline()?;
            return Ok(Stmt { kind: StmtKind::LetTuple { mutable, pat, ty, value }, span });
        }
        let name = self.expect_ident("variable name")?;
        let ty = if self.eat(&Tok::Colon) { Some(self.type_expr()?) } else { None };
        if !self.at(&Tok::Assign) {
            let kw = if mutable { "var" } else { "val" };
            let span = name.span;
            return self.error_diag(
                Diagnostic::error(format!("`{}` must be initialized", name.name), span)
                    .with_note("every variable in Tarn needs an initial value")
                    .with_help(format!("write `{kw} {} = <value>`", name.name)),
            );
        }
        self.bump();
        let value = self.expr()?;
        let span = start.merge(self.prev_span);
        self.expect_newline()?;
        Ok(Stmt { kind: StmtKind::Let { mutable, name, ty, value }, span })
    }

    /// `match` is only a keyword at the start of a `match <value>:` line (as
    /// in Python), so it can still be used as a name. `None`: not a match.
    fn try_match_stmt(&mut self) -> Option<PResult<Stmt>> {
        let (pos, ndiags, prev) = (self.pos, self.diags.len(), self.prev_span);
        let start = self.bump().span;
        let looks_like_match = !matches!(
            self.peek(),
            Tok::Assign | Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq | Tok::PercentEq | Tok::Dot | Tok::Newline | Tok::Eof
        );
        let scrutinee = if looks_like_match { self.expr().ok() } else { None };
        match scrutinee {
            Some(scrutinee) if self.at(&Tok::Colon) => Some(self.match_body(start, scrutinee)),
            _ => {
                self.pos = pos;
                self.prev_span = prev;
                self.diags.truncate(ndiags);
                None
            }
        }
    }

    /// `match code:` where a value is expected: like a statement `match`,
    /// it must end its line (its cases follow, indented). `None`: `match`
    /// is just a name here.
    fn try_match_expr(&mut self) -> Option<PResult<Expr>> {
        let (pos, ndiags, prev) = (self.pos, self.diags.len(), self.prev_span);
        let start = self.bump().span;
        let looks_like_match = starts_operand(self.peek()) || matches!(self.peek(), Tok::LParen | Tok::LBracket | Tok::LBrace | Tok::Minus);
        let scrutinee = if looks_like_match { self.expr().ok() } else { None };
        match scrutinee {
            Some(scrutinee) if self.at(&Tok::Colon) => {
                if !matches!(self.peek_nth(1), Tok::Newline) {
                    let span = self.span();
                    return Some(self.error_diag(
                        Diagnostic::error("a `match` must end its line", span)
                            .with_note("its cases go on the lines below it, indented, so it can't be inside parentheses or followed by more code")
                            .with_help("put its value in a variable first: `val x = match ...:`, with the cases under it"),
                    ));
                }
                Some(self.match_body(start, scrutinee).map(|st| {
                    self.block_end = Some(self.pos);
                    let StmtKind::Match { scrutinee, arms } = st.kind else { unreachable!() };
                    Expr { kind: ExprKind::Match { scrutinee: Box::new(scrutinee), arms }, span: st.span }
                }))
            }
            _ => {
                self.pos = pos;
                self.prev_span = prev;
                self.diags.truncate(ndiags);
                None
            }
        }
    }

    fn match_body(&mut self, start: Span, scrutinee: Expr) -> PResult<Stmt> {
        self.expect(Tok::Colon)?;
        if !self.eat(&Tok::Newline) || !self.at(&Tok::Indent) {
            let span = self.span();
            return self.error_diag(
                Diagnostic::error("expected an indented list of `case`s", span)
                    .with_help("write each case on its own line:\n\nmatch shape:\n    case Circle(r):\n        print(r)\n    case _:\n        pass"),
            );
        }
        self.bump();
        let mut arms = Vec::new();
        while !self.at(&Tok::Dedent) && !self.at(&Tok::Eof) {
            if self.eat(&Tok::Newline) {
                continue;
            }
            match self.match_arm() {
                Ok(a) => arms.push(a),
                Err(()) => self.synchronize(),
            }
        }
        self.eat(&Tok::Dedent);
        Ok(Stmt { kind: StmtKind::Match { scrutinee, arms }, span: start.merge(self.prev_span) })
    }

    fn match_arm(&mut self) -> PResult<MatchArm> {
        if !matches!(self.peek(), Tok::Ident(n) if n == "case") {
            let span = self.span();
            let found = self.peek().clone();
            return self.error_diag(
                Diagnostic::error(format!("expected `case`, found {found}"), span)
                    .with_primary("expected `case`")
                    .with_note("a `match` contains only `case` lines, each with an indented block"),
            );
        }
        let start = self.bump().span;
        let pattern = self.pattern()?;
        let guard = if self.eat(&Tok::If) { Some(self.expr()?) } else { None };
        let span = start.merge(self.prev_span);
        let body = self.block()?;
        Ok(MatchArm { pattern, guard, body, span })
    }

    // ---------- patterns ----------

    /// `p`, `p | q | ...`, optionally followed by `as name`.
    fn pattern(&mut self) -> PResult<Pattern> {
        let first = self.closed_pattern()?;
        let mut p = if self.at(&Tok::Pipe) {
            let mut alts = vec![first];
            while self.eat(&Tok::Pipe) {
                alts.push(self.closed_pattern()?);
            }
            let span = alts[0].span.merge(alts.last().unwrap().span);
            Pattern { kind: PatternKind::Or(alts), span }
        } else {
            first
        };
        if matches!(self.peek(), Tok::Ident(n) if n == "as") {
            self.bump();
            let name = self.expect_ident("a name after `as`")?;
            let span = p.span.merge(name.span);
            p = Pattern { kind: PatternKind::As(Box::new(p), name), span };
        }
        Ok(p)
    }

    fn closed_pattern(&mut self) -> PResult<Pattern> {
        let start = self.span();
        let kind = match self.peek().clone() {
            Tok::Ident(n) if n == "_" => {
                self.bump();
                PatternKind::Wild
            }
            Tok::Ident(_) => {
                let mut path = vec![self.expect_ident("a pattern")?];
                while self.eat(&Tok::Dot) {
                    path.push(self.expect_ident("a name after `.`")?);
                }
                if self.eat(&Tok::LParen) {
                    let mut args = Vec::new();
                    while !self.at(&Tok::RParen) {
                        let name = if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_nth(1), Tok::Assign) {
                            let id = self.expect_ident("field name")?;
                            self.bump();
                            Some(id)
                        } else {
                            None
                        };
                        let pat = self.pattern()?;
                        args.push(PatArg { name, pat });
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::RParen)?;
                    PatternKind::Ctor { path, args: Some(args) }
                } else if path.len() == 1 && !path[0].name.starts_with(|c: char| c.is_ascii_uppercase()) {
                    // A lowercase name captures the value; capitalized names are variants.
                    PatternKind::Bind(path.pop().unwrap())
                } else {
                    PatternKind::Ctor { path, args: None }
                }
            }
            Tok::Int(v) => {
                self.bump();
                if v > i64::MAX as u64 {
                    return self.error("integer literal is too large for `Int` (64-bit)", start);
                }
                PatternKind::Int(v as i64)
            }
            Tok::Minus => {
                self.bump();
                match self.peek().clone() {
                    Tok::Int(v) => {
                        self.bump();
                        PatternKind::Int(if v == 1u64 << 63 { i64::MIN } else { -(v as i64) })
                    }
                    Tok::Float(v) => {
                        self.bump();
                        PatternKind::Float(-v)
                    }
                    _ => return self.unexpected("a number after `-`"),
                }
            }
            Tok::Float(v) => {
                self.bump();
                PatternKind::Float(v)
            }
            Tok::Str(v) => {
                self.bump();
                PatternKind::Str(v)
            }
            Tok::True | Tok::False => PatternKind::Bool(matches!(self.bump().tok, Tok::True)),
            Tok::Null => {
                self.bump();
                PatternKind::Null
            }
            Tok::LParen => {
                self.bump();
                let p = self.pattern()?;
                // `(x, 0)`: a tuple pattern.
                if self.eat(&Tok::Comma) {
                    let mut elems = vec![p];
                    while !self.at(&Tok::RParen) {
                        elems.push(self.pattern()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::RParen)?;
                    return Ok(Pattern { kind: PatternKind::Tuple(elems), span: start.merge(self.prev_span) });
                }
                self.expect(Tok::RParen)?;
                return Ok(Pattern { kind: p.kind, span: start.merge(self.prev_span) });
            }
            Tok::FStr(_) => return self.error("f-strings can't be used as patterns; use a plain string", start),
            _ => return self.unexpected("a pattern"),
        };
        Ok(Pattern { kind, span: start.merge(self.prev_span) })
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
                        Diagnostic::error("`else if` is not valid Tarn", span).with_help("use `elif`"),
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
        // `for (k, v) in pairs:`, `for i, (k, v) in pairs:`
        let mut pats: [Option<Pattern>; 2] = [None, None];
        let mut loop_var = |p: &mut Self, k: usize, what: &str| -> PResult<Ident> {
            if p.at(&Tok::LParen) {
                let pat = p.closed_pattern()?;
                let id = Ident { name: format!("_for{k}"), span: pat.span };
                pats[k] = Some(pat);
                Ok(id)
            } else {
                p.expect_ident(what)
            }
        };
        let var = loop_var(self, 0, "loop variable")?;
        let second = if self.eat(&Tok::Comma) { Some(loop_var(self, 1, "second loop variable")?) } else { None };
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
        Ok(Stmt { kind: StmtKind::For { var, second, iter, body, pats }, span: start.merge(self.prev_span) })
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

    /// `a <=> b` gives a number, so it binds tighter than `==` and `<`:
    /// `a <=> b == 0` compares its result.
    fn spaceship_expr(&mut self) -> PResult<Expr> {
        let l = self.elvis_expr()?;
        if self.eat(&Tok::Spaceship) {
            let r = self.elvis_expr()?;
            return Ok(Self::binary(BinOp::Cmp, l, r));
        }
        Ok(l)
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

    /// `in` or `not in` (Python's membership tests, at the level of comparisons).
    fn at_in(&self) -> Option<bool> {
        match self.peek() {
            Tok::In => Some(false),
            Tok::Not if matches!(self.peek_nth(1), Tok::In) => Some(true),
            _ => None,
        }
    }

    fn cmp_expr(&mut self) -> PResult<Expr> {
        let l = self.spaceship_expr()?;
        if let Some(negated) = self.at_in() {
            self.bump();
            if negated {
                self.bump();
            }
            let r = self.spaceship_expr()?;
            let span = l.span.merge(r.span);
            let e = Expr { kind: ExprKind::In { item: Box::new(l), container: Box::new(r), negated }, span };
            if self.cmp_op().is_some() || self.at_in().is_some() {
                let span = self.span();
                return self.error_diag(
                    Diagnostic::error("comparison operators cannot be chained", span)
                        .with_label(e.span, "this test already produces a `Bool`")
                        .with_help("split it up, e.g. `a in xs and b in xs`"),
                );
            }
            return Ok(e);
        }
        if let Some(op) = self.cmp_op() {
            self.bump();
            let r = self.spaceship_expr()?;
            let e = Self::binary(op, l, r);
            if self.cmp_op().is_some() || self.at_in().is_some() {
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
                    .with_help("Tarn has no `c ? a : b` operator; use an `if` statement"),
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
                    let args = self.call_args()?;
                    let span = e.span.merge(self.prev_span);
                    e = Expr { kind: ExprKind::Call { callee: Box::new(e), args }, span };
                }
                Tok::Dot => {
                    self.bump();
                    // `t.0`: an element of a tuple.
                    if let Tok::Int(n) = self.peek().clone() {
                        let at = self.bump().span;
                        let name = Ident { name: n.to_string(), span: at };
                        let span = e.span.merge(at);
                        e = Expr { kind: ExprKind::Field(Box::new(e), name), span };
                        continue;
                    }
                    let name = self.expect_ident("field or method name")?;
                    let span = e.span.merge(name.span);
                    e = Expr { kind: ExprKind::Field(Box::new(e), name), span };
                }
                Tok::QuestionDot => {
                    self.bump();
                    // `t?.0`
                    if let Tok::Int(n) = self.peek().clone() {
                        let at = self.bump().span;
                        let name = Ident { name: n.to_string(), span: at };
                        let span = e.span.merge(at);
                        e = Expr { kind: ExprKind::SafeField(Box::new(e), name), span };
                        continue;
                    }
                    let name = self.expect_ident("field or method name after `?.`")?;
                    let span = e.span.merge(name.span);
                    e = Expr { kind: ExprKind::SafeField(Box::new(e), name), span };
                }
                Tok::BangBang => {
                    let end = self.bump().span;
                    let span = e.span.merge(end);
                    e = Expr { kind: ExprKind::NotNull(Box::new(e)), span };
                }
                // `x?` (but not the `?` of `x ?: y`, nor a `c ? a : b` someone tried).
                Tok::Question if !self.at_elvis() && !starts_operand(self.peek_nth(1)) => {
                    let end = self.bump().span;
                    let span = e.span.merge(end);
                    e = Expr { kind: ExprKind::Try(Box::new(e)), span };
                }
                Tok::LBracket => {
                    if let Some(args) = self.try_type_args(&e) {
                        let span = e.span.merge(self.prev_span);
                        e = Expr { kind: ExprKind::TypeApply(Box::new(e), args), span };
                        continue;
                    }
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

    /// The arguments of a call, after its `(`, up to and including the `)`.
    fn call_args(&mut self) -> PResult<Vec<Arg>> {
        let mut args = Vec::new();
        while !self.at(&Tok::RParen) {
            let name = if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_nth(1), Tok::Assign) {
                let id = self.expect_ident("argument name")?;
                self.bump(); // `=`
                Some(id)
            } else {
                None
            };
            let mut value = self.expr()?;
            // `decode(f) for f in fields(T) if cond`: one argument per element.
            if self.at(&Tok::For) {
                self.bump();
                let var = self.expect_ident("loop variable")?;
                self.expect(Tok::In)?;
                let iter = self.expr()?;
                let cond = if self.eat(&Tok::If) { Some(Box::new(self.expr()?)) } else { None };
                let span = value.span.merge(self.prev_span);
                value = Expr { kind: ExprKind::Spread { value: Box::new(value), var, iter: Box::new(iter), cond }, span };
            }
            args.push(Arg { name, value });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        Ok(args)
    }

    /// `[Int]` in `max[Int](a, b)` or `Tree[Int].Leaf`: explicit type
    /// arguments. They look like an index, so this is decided by what follows:
    /// `(` (a call: an indexed value is never callable), or `.` after a type
    /// name. Anything else is put back and parsed as an index.
    fn try_type_args(&mut self, base: &Expr) -> Option<Vec<TypeExpr>> {
        let last = match &base.kind {
            ExprKind::Name(n) => n.as_str(),
            ExprKind::Field(m, n) if matches!(m.kind, ExprKind::Name(_)) => n.name.as_str(),
            _ => return None,
        };
        let type_like = last.starts_with(|c: char| c.is_ascii_uppercase());
        let (pos, prev, ndiags) = (self.pos, self.prev_span, self.diags.len());
        self.bump();
        let mut args = Vec::new();
        let ok = loop {
            match self.type_expr() {
                Ok(t) => args.push(t),
                Err(()) => break false,
            }
            if !self.eat(&Tok::Comma) {
                break self.eat(&Tok::RBracket);
            }
        };
        if ok && (self.at(&Tok::LParen) || type_like && self.at(&Tok::Dot)) {
            return Some(args);
        }
        // `type_name(List[User])`, `fields(Box[Int])`: a type as an argument.
        let all_types = args.iter().all(|t| t.func.is_some() || t.name.name == TUPLE_TYPE || t.name.name.starts_with(|c: char| c.is_ascii_uppercase()));
        if ok && type_like && all_types && (self.at(&Tok::RParen) || self.at(&Tok::Comma)) {
            return Some(args);
        }
        self.pos = pos;
        self.prev_span = prev;
        self.diags.truncate(ndiags);
        None
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
            Tok::Ident(name) if name == "match" => {
                if let Some(m) = self.try_match_expr() {
                    return m;
                }
                self.bump();
                ExprKind::Name(name)
            }
            Tok::Ident(name) => {
                self.bump();
                ExprKind::Name(name)
            }
            Tok::Fn => return self.lambda(),
            Tok::LParen => {
                self.bump();
                if self.at(&Tok::RParen) {
                    let at = self.span();
                    return self.error_diag(
                        Diagnostic::error("`()` isn't a value", span.merge(at))
                            .with_note("a tuple has two or more elements: `(1, \"a\")`")
                            .with_help("for a function that returns nothing, leave out the `return` value"),
                    );
                }
                let e = self.expr()?;
                // `(1, "a")`: a tuple.
                if self.eat(&Tok::Comma) {
                    let mut elems = vec![e];
                    while !self.at(&Tok::RParen) {
                        elems.push(self.expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::RParen)?;
                    let span = span.merge(self.prev_span);
                    if elems.len() == 1 {
                        return self.error_diag(
                            Diagnostic::error("a tuple needs at least two elements", span).with_help("for one value, leave out the parentheses and the comma"),
                        );
                    }
                    return Ok(Expr { kind: ExprKind::Tuple(elems), span });
                }
                self.expect(Tok::RParen)?;
                return Ok(Expr { kind: e.kind, span: span.merge(self.prev_span) });
            }
            Tok::LBracket => {
                self.bump();
                let mut elems = Vec::new();
                while !self.at(&Tok::RBracket) {
                    elems.push(self.expr()?);
                    if self.at(&Tok::For) {
                        let at = self.span();
                        return self.error_diag(
                            Diagnostic::error("Tarn has no list comprehensions", at)
                                .with_primary("a `for` inside `[...]`")
                                .with_help("use `xs.map(fn(x) -> ...)` (and `filter`), or a `for` loop that pushes to a list"),
                        );
                    }
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RBracket)?;
                ExprKind::List(elems)
            }
            Tok::LBrace => {
                self.bump();
                let mut entries = Vec::new();
                while !self.at(&Tok::RBrace) {
                    let k = self.expr()?;
                    if !self.at(&Tok::Colon) {
                        let span = self.span();
                        return self.error_diag(
                            Diagnostic::error(format!("expected `:` after a map key, found {}", self.peek()), span)
                                .with_primary("expected `:`")
                                .with_note("map literals pair each key with its value: `{\"a\": 1, \"b\": 2}`")
                                .with_help("Tarn has no set literals; for a list, use `[...]`"),
                        );
                    }
                    self.bump();
                    let v = self.expr()?;
                    entries.push((k, v));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RBrace)?;
                ExprKind::Map(entries)
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
    matches!(t, Tok::Dot | Tok::LParen | Tok::LBracket | Tok::QuestionDot | Tok::BangBang | Tok::Question)
}

/// Can `t` start an operand (so a `?` before it can't be the postfix `?`)?
fn starts_operand(t: &Tok) -> bool {
    matches!(t, Tok::Ident(_) | Tok::Int(_) | Tok::Float(_) | Tok::Str(_) | Tok::FStr(_) | Tok::True | Tok::False | Tok::Null | Tok::Not)
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
            ExprKind::Try(e) => format!("{}?", show(e)),
            ExprKind::Elvis(a, b) => format!("({} ?: {})", show(a), show(b)),
            ExprKind::Index(e, i) => format!("{}[{}]", show(e), show(i)),
            ExprKind::TypeApply(e, ts) => {
                let ts: Vec<String> = ts.iter().map(|t| if t.args.is_empty() { t.name.name.clone() } else { format!("{}[..]", t.name.name) }).collect();
                format!("{}<{}>", show(e), ts.join(", "))
            }
            ExprKind::Map(es) => {
                let es: Vec<String> = es.iter().map(|(k, v)| format!("{}: {}", show(k), show(v))).collect();
                format!("{{{}}}", es.join(", "))
            }
            ExprKind::In { item, container, negated } => format!("({} {}in {})", show(item), if *negated { "not " } else { "" }, show(container)),
            ExprKind::Lambda(l) => {
                let ps: Vec<String> = l.params.iter().map(|p| match &p.ty {
                    Some(t) => format!("{}: {}", p.name.name, t.name.name),
                    None => p.name.name.clone(),
                }).collect();
                match &l.body {
                    LambdaBody::Expr(b) => format!("fn({}) -> {}", ps.join(", "), show(b)),
                    LambdaBody::Block(b) => format!("fn({}) {{{} statements}}", ps.join(", "), b.len()),
                }
            }
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
    fn maps_in_and_two_loop_variables() {
        assert_eq!(expr_of("f({a: 1, b: {c: 2}})"), "f({a: 1, b: {c: 2}})");
        assert_eq!(expr_of("f({})"), "f({})");
        assert_eq!(expr_of("not a in b or c not in d + e"), "((not (a in b)) or (c not in (d + e)))");
        let (_, d) = parse("fn main():\n    a in b in c\n");
        assert!(d.iter().any(|d| d.message.contains("chained")));
        let p = parse_ok("fn main():\n    for k, v in m:\n        pass\n");
        match &first_fn(&p).body[0].kind {
            StmtKind::For { var, second: Some(v), .. } => assert_eq!((var.name.as_str(), v.name.as_str()), ("k", "v")),
            other => panic!("{other:?}"),
        }
        // A multi-line map literal, and `{` after a value is still a block brace.
        parse_ok("fn main():\n    val m = {\n        1: 2,\n        3: 4,\n    }\n");
        let (_, d) = parse("fn main() {\n    pass\n");
        assert!(d.iter().any(|d| d.message.contains("not `{`")));
    }

    #[test]
    fn top_level_constants() {
        let p = parse_ok("val pi: Float = 3.14\nval name = \"x\"\n");
        assert!(matches!(&p.items[0], Item::Const(c) if c.name.name == "pi" && c.ty.is_some()));
        assert!(matches!(&p.items[1], Item::Const(c) if c.name.name == "name" && c.ty.is_none()));
        let (_, d) = parse("var x = 1\n");
        assert!(d.iter().any(|d| d.message.contains("no global variables")));
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
    fn enums_and_match() {
        let p = parse_ok(
            "enum Shape:\n    Circle(radius: Float)\n    Rect(w: Float, h: Float)\n    Empty\n\n    fn area(self) -> Float:\n        return 0.0\n\nfn main():\n    match s:\n        case Circle(r) if r > 1.0:\n            pass\n        case Shape.Rect(w, h=_) | Empty as e:\n            pass\n        case -1 | \"x\" | null | _:\n            pass\n    val match = 1\n    match += 1\n",
        );
        match &p.items[0] {
            Item::Enum(e) => {
                assert_eq!(e.variants.len(), 3);
                assert_eq!(e.variants[1].fields.len(), 2);
                assert!(e.variants[2].fields.is_empty());
                assert_eq!(e.methods.len(), 1);
            }
            _ => panic!(),
        }
        let Item::Fn(f) = &p.items[1] else { panic!() };
        match &f.body[0].kind {
            StmtKind::Match { arms, .. } => {
                assert_eq!(arms.len(), 3);
                assert!(arms[0].guard.is_some());
                assert!(matches!(&arms[1].pattern.kind, PatternKind::As(inner, _) if matches!(&inner.kind, PatternKind::Or(a) if a.len() == 2)));
                assert!(matches!(&arms[2].pattern.kind, PatternKind::Or(a) if a.len() == 4));
            }
            other => panic!("{other:?}"),
        }
        // `match` is still usable as a name
        assert!(matches!(f.body[1].kind, StmtKind::Let { .. }));
        assert!(matches!(f.body[2].kind, StmtKind::Assign { .. }));
    }

    #[test]
    fn lambdas_and_function_types() {
        // A lambda's body is the rest of the expression.
        assert_eq!(expr_of("xs.map(fn(x) -> x * 2)"), "xs.map(fn(x) -> (x * 2))");
        assert_eq!(expr_of("f(fn(a: Int, b) -> a + b, 1)"), "f(fn(a: Int, b) -> (a + b), 1)");
        assert_eq!(expr_of("xs.map(fn(x) -> x ?: 0)"), "xs.map(fn(x) -> (x ?: 0))");
        assert_eq!(expr_of("(fn() -> 1)()"), "fn() -> 1()");
        // A block body ends its line; the next statement follows.
        let p = parse_ok("fn main():\n    val f = fn(x: Int) -> Int?:\n        return x\n    print(f(1))\n");
        let Item::Fn(m) = &p.items[0] else { panic!() };
        assert_eq!(m.body.len(), 2);
        let StmtKind::Let { value, .. } = &m.body[0].kind else { panic!() };
        assert_eq!(show(value), "fn(x: Int) {1 statements}");
        // Function types, made nullable with parentheses.
        let p = parse_ok("fn run(f: fn(Int, String) -> Bool, g: (fn())?, h: fn() -> Int?):\n    pass\n");
        let Item::Fn(f) = &p.items[0] else { panic!() };
        let ft = f.params[0].ty.func.as_ref().unwrap();
        assert_eq!((ft.params.len(), ft.ret.as_ref().unwrap().name.name.as_str()), (2, "Bool"));
        assert!(f.params[1].ty.nullable && f.params[1].ty.func.as_ref().unwrap().ret.is_none());
        assert!(!f.params[2].ty.nullable && f.params[2].ty.func.as_ref().unwrap().ret.as_ref().unwrap().nullable);
        // A block can't start inside parentheses.
        let (_, d) = parse("fn main():\n    xs.map(fn(x: Int) -> Int:\n        return x\n    )\n");
        assert!(d[0].message.contains("must start on a new line"), "{d:?}");
        let (_, d) = parse("fn main():\n    fn helper():\n        pass\n");
        assert!(d[0].message.contains("inside other functions"), "{d:?}");
    }

    #[test]
    fn generics_syntax() {
        let p = parse_ok("struct Pair[A, B]:\n    a: A\n    b: B\n\nfn max[T](a: T, b: T) -> T:\n    return a\n");
        let Item::Struct(st) = &p.items[0] else { panic!() };
        assert_eq!(st.type_params.iter().map(|p| p.name.name.as_str()).collect::<Vec<_>>(), ["A", "B"]);
        let Item::Fn(f) = &p.items[1] else { panic!() };
        assert_eq!(f.type_params[0].name.name, "T");
        // `[...]` is a type argument list before `(`, or before `.` after a type name; an index otherwise.
        assert_eq!(expr_of("max[Int](a, b)"), "max<Int>(a, b)");
        assert_eq!(expr_of("geo.Pair[Int, List[String]](a, b)"), "geo.Pair<Int, List[..]>(a, b)");
        assert_eq!(expr_of("Tree[Int].Leaf"), "Tree<Int>.Leaf");
        assert_eq!(expr_of("xs[i].name"), "xs[i].name");
        assert_eq!(expr_of("xs[i + 1]"), "xs[(i + 1)]");
        assert_eq!(expr_of("grid[0]"), "grid[0]");
    }

    #[test]
    fn interfaces_and_bounds() {
        let p = parse_ok(
            "interface Shape:\n    # its area\n    fn area(self) -> Float\n    fn scale(self, k: Float)\n\ninterface Both(Shape, Named):\n    pass\n\nstruct Sq[T](Shape, geo.Named):\n    s: Float\n\nenum E(Shape):\n    A\n\nfn f[T: Shape + geo.Named, U](x: T, y: U) -> T:\n    return x\n\nfn main():\n    val interface = 1\n    print(interface)\n",
        );
        let Item::Interface(i) = &p.items[0] else { panic!("{:?}", p.items[0]) };
        assert_eq!(i.name.name, "Shape");
        assert_eq!(i.methods.iter().map(|m| m.name.name.as_str()).collect::<Vec<_>>(), ["area", "scale"]);
        assert!(i.methods.iter().all(|m| m.body.is_empty() && m.self_param.is_some()));
        let Item::Interface(both) = &p.items[1] else { panic!() };
        assert_eq!(both.extends.iter().map(|t| t.name.name.as_str()).collect::<Vec<_>>(), ["Shape", "Named"]);
        let Item::Struct(st) = &p.items[2] else { panic!() };
        assert_eq!(st.implements.len(), 2);
        assert_eq!(st.implements[1].module.as_ref().map(|m| m.name.as_str()), Some("geo"));
        let Item::Enum(e) = &p.items[3] else { panic!() };
        assert_eq!(e.implements[0].name.name, "Shape");
        let Item::Fn(f) = &p.items[4] else { panic!() };
        assert_eq!(f.type_params[0].bounds.iter().map(|b| b.name.name.as_str()).collect::<Vec<_>>(), ["Shape", "Named"]);
        assert!(f.type_params[1].bounds.is_empty());
        let (_, d) = parse("interface Shape:\n    sides: Int\n");
        assert!(d[0].message.contains("interfaces list methods, not fields"), "{d:?}");
        // Generic interfaces, and default methods (with bodies).
        let p = parse_ok("interface Source[T](Named):\n    fn next(self) -> T?\n    fn take(self, n: Int) -> List[T]:\n        return []\n    fn nothing(self): pass\n");
        let Item::Interface(i) = &p.items[0] else { panic!() };
        assert_eq!(i.type_params[0].name.name, "T");
        assert_eq!(i.methods.iter().map(|m| m.name.name.as_str()).collect::<Vec<_>>(), ["next"]);
        assert_eq!(i.defaults.iter().map(|m| m.name.name.as_str()).collect::<Vec<_>>(), ["take", "nothing"]);
        let (_, d) = parse("interface Box[T: Shape]:\n    fn get(self) -> T\n");
        assert!(d[0].message.contains("type parameters can't have bounds"), "{d:?}");
    }

    #[test]
    fn compile_time_syntax() {
        let p = parse_ok(
            "attribute get(path: String, verb: String = \"GET\")\nattribute skip\n\n@get(\"/a\")\n@mylib.tag\ncomptime fn f(x: Int) -> Int:\n    return x\n\nstruct S:\n    @skip\n    x: Int\n    @get(\"/b\")\n    fn m(self, @skip y: Int):\n        pass\n\nfn main():\n    comptime val n = f(1)\n    comptime if n > 0:\n        pass\n    elif n < 0:\n        pass\n    comptime for i, x in fields(S):\n        pass\n    comptime match T:\n        case Int | List[_]:\n            pass\n        case U?:\n            pass\n    print(S(1 for f in fields(S) if f.name != \"y\"))\n    print(type_name(Map[String, Int]))\n    val comptime = 1\n    print(comptime)\n",
        );
        let Item::Attribute(a) = &p.items[0] else { panic!("{:?}", p.items[0]) };
        assert_eq!(a.fields.len(), 2);
        assert!(a.fields[1].default.is_some());
        let Item::Fn(f) = &p.items[2] else { panic!() };
        assert!(f.comptime);
        assert_eq!(f.attrs.len(), 2);
        assert_eq!(f.attrs[1].module.as_ref().map(|m| m.name.as_str()), Some("mylib"));
        let Item::Struct(st) = &p.items[3] else { panic!() };
        assert_eq!(st.fields[0].attrs.len(), 1);
        assert_eq!(st.methods[0].attrs.len(), 1);
        assert_eq!(st.methods[0].params[0].attrs[0].name.name, "skip");
        let main = &p.items[4];
        let Item::Fn(main) = main else { panic!() };
        assert!(matches!(&main.body[0].kind, StmtKind::Comptime(c) if matches!(**c, ComptimeStmt::Let { .. })));
        assert!(matches!(&main.body[1].kind, StmtKind::Comptime(c) if matches!(&**c, ComptimeStmt::If { branches, .. } if branches.len() == 2)));
        assert!(matches!(&main.body[2].kind, StmtKind::Comptime(c) if matches!(&**c, ComptimeStmt::For { second: Some(_), .. })));
        assert!(matches!(&main.body[3].kind, StmtKind::Comptime(c) if matches!(&**c, ComptimeStmt::Match { arms, .. } if arms[0].pats.len() == 2 && arms[1].pats[0].nullable)));
        let StmtKind::Expr(e) = &main.body[4].kind else { panic!() };
        let ExprKind::Call { args, .. } = &e.kind else { panic!() };
        let ExprKind::Call { args, .. } = &args[0].value.kind else { panic!() };
        assert!(matches!(&args[0].value.kind, ExprKind::Spread { cond: Some(_), .. }));
        let StmtKind::Expr(e) = &main.body[5].kind else { panic!() };
        let ExprKind::Call { args, .. } = &e.kind else { panic!() };
        let ExprKind::Call { args, .. } = &args[0].value.kind else { panic!() };
        assert!(matches!(&args[0].value.kind, ExprKind::TypeApply(_, t) if t.len() == 2));
    }

    #[test]
    fn type_system_syntax() {
        // static methods, field defaults, tuples, `match` values
        let p = parse_ok(
            "struct App:\n    port: Int = 8080\n    fn new() -> App:\n        return App()\n\nfn main():\n    val (a, (b, _)) = (1, (2, 3))\n    val t: (Int, String)? = null\n    print(t.0.1)\n    for i, (k, v) in pairs:\n        pass\n    val x = match a:\n        case (0, y): y\n        case _: 1\n    print(x)\n",
        );
        let Item::Struct(s) = &p.items[0] else { panic!() };
        assert!(s.fields[0].default.is_some() && s.methods[0].self_param.is_none());
        let Item::Fn(f) = &p.items[1] else { panic!() };
        assert!(matches!(&f.body[0].kind, StmtKind::LetTuple { pat, .. } if matches!(&pat.kind, PatternKind::Tuple(ps) if ps.len() == 2)));
        assert!(matches!(&f.body[1].kind, StmtKind::Let { ty: Some(t), .. } if t.name.name == TUPLE_TYPE && t.nullable && t.args.len() == 2));
        assert_eq!(show(match &f.body[2].kind {
            StmtKind::Expr(e) => e,
            _ => panic!(),
        }), "print(t.0.1)");
        assert!(matches!(&f.body[3].kind, StmtKind::For { pats: [None, Some(_)], .. }));
        assert!(matches!(&f.body[4].kind, StmtKind::Let { value: Expr { kind: ExprKind::Match { .. }, .. }, .. }));
        // `match` stays a name where it can't start a `match`.
        parse_ok("fn main():\n    val match = 1\n    print(match + 1)\n");
        let (_, d) = parse("fn main():\n    print(match x:\n        case _: 1)\n");
        assert!(d.iter().any(|d| d.message.contains("a `match` must end its line")), "{d:?}");
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
