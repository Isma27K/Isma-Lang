use crate::span::Span;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum FPart {
    Lit(String),
    /// Raw expression source plus the byte offset of its first char in the file.
    Expr(String, u32),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Int(u64),
    Float(f64),
    Str(String),
    FStr(Vec<FPart>),
    Ident(String),

    // keywords
    Fn,
    Val,
    Var,
    If,
    Elif,
    Else,
    While,
    For,
    In,
    Break,
    Continue,
    Return,
    Pass,
    Struct,
    Import,
    True,
    False,
    And,
    Or,
    Not,
    Null,

    // punctuation
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Dot,
    DotDot,
    DotDotEq,
    Arrow,
    /// `?` (nullable types; `?:` is the elvis operator)
    Question,
    /// `?.` (safe call)
    QuestionDot,
    /// `!!` (non-null assertion)
    BangBang,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Assign,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,

    // layout
    Newline,
    Indent,
    Dedent,
    Eof,
}

impl Tok {
    pub fn keyword(s: &str) -> Option<Tok> {
        Some(match s {
            "fn" => Tok::Fn,
            "val" => Tok::Val,
            "var" => Tok::Var,
            "if" => Tok::If,
            "elif" => Tok::Elif,
            "else" => Tok::Else,
            "while" => Tok::While,
            "for" => Tok::For,
            "in" => Tok::In,
            "break" => Tok::Break,
            "continue" => Tok::Continue,
            "return" => Tok::Return,
            "pass" => Tok::Pass,
            "struct" => Tok::Struct,
            "import" => Tok::Import,
            "true" => Tok::True,
            "false" => Tok::False,
            "and" => Tok::And,
            "or" => Tok::Or,
            "not" => Tok::Not,
            "null" => Tok::Null,
            _ => return None,
        })
    }
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Tok::Int(n) => return write!(f, "integer `{n}`"),
            Tok::Float(x) => return write!(f, "float `{x}`"),
            Tok::Str(_) => "string literal",
            Tok::FStr(_) => "f-string",
            Tok::Ident(name) => return write!(f, "identifier `{name}`"),
            Tok::Fn => "`fn`",
            Tok::Val => "`val`",
            Tok::Var => "`var`",
            Tok::If => "`if`",
            Tok::Elif => "`elif`",
            Tok::Else => "`else`",
            Tok::While => "`while`",
            Tok::For => "`for`",
            Tok::In => "`in`",
            Tok::Break => "`break`",
            Tok::Continue => "`continue`",
            Tok::Return => "`return`",
            Tok::Pass => "`pass`",
            Tok::Struct => "`struct`",
            Tok::Import => "`import`",
            Tok::True => "`true`",
            Tok::False => "`false`",
            Tok::And => "`and`",
            Tok::Or => "`or`",
            Tok::Not => "`not`",
            Tok::Null => "`null`",
            Tok::Question => "`?`",
            Tok::QuestionDot => "`?.`",
            Tok::BangBang => "`!!`",
            Tok::LParen => "`(`",
            Tok::RParen => "`)`",
            Tok::LBracket => "`[`",
            Tok::RBracket => "`]`",
            Tok::Comma => "`,`",
            Tok::Colon => "`:`",
            Tok::Dot => "`.`",
            Tok::DotDot => "`..`",
            Tok::DotDotEq => "`..=`",
            Tok::Arrow => "`->`",
            Tok::Plus => "`+`",
            Tok::Minus => "`-`",
            Tok::Star => "`*`",
            Tok::Slash => "`/`",
            Tok::Percent => "`%`",
            Tok::Assign => "`=`",
            Tok::EqEq => "`==`",
            Tok::NotEq => "`!=`",
            Tok::Lt => "`<`",
            Tok::Le => "`<=`",
            Tok::Gt => "`>`",
            Tok::Ge => "`>=`",
            Tok::PlusEq => "`+=`",
            Tok::MinusEq => "`-=`",
            Tok::StarEq => "`*=`",
            Tok::SlashEq => "`/=`",
            Tok::PercentEq => "`%=`",
            Tok::Newline => "end of line",
            Tok::Indent => "indentation",
            Tok::Dedent => "end of block",
            Tok::Eof => "end of file",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}
