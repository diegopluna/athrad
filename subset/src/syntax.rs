//! Tokens and syntax trees for the subset, and the parser. Anything outside
//! the subset is a [`Fallback`]: not an error, a hand-off to `rustc`.

use crate::{Fallback, fallback};

/// Where a piece of source starts: 1-based line, and 1-based column in
/// characters, as `rustc` writes them into panic locations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub line: u32,
    pub col: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntTy {
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
}

impl IntTy {
    pub fn from_name(s: &str) -> Option<IntTy> {
        Some(match s {
            "i8" => IntTy::I8,
            "i16" => IntTy::I16,
            "i32" => IntTy::I32,
            "i64" => IntTy::I64,
            "i128" => IntTy::I128,
            "isize" => IntTy::Isize,
            "u8" => IntTy::U8,
            "u16" => IntTy::U16,
            "u32" => IntTy::U32,
            "u64" => IntTy::U64,
            "u128" => IntTy::U128,
            "usize" => IntTy::Usize,
            _ => return None,
        })
    }

    pub fn bits(self) -> u32 {
        match self {
            IntTy::I8 | IntTy::U8 => 8,
            IntTy::I16 | IntTy::U16 => 16,
            IntTy::I32 | IntTy::U32 => 32,
            IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => 64,
            IntTy::I128 | IntTy::U128 => 128,
        }
    }

    pub fn signed(self) -> bool {
        matches!(self, IntTy::I8 | IntTy::I16 | IntTy::I32 | IntTy::I64 | IntTy::I128 | IntTy::Isize)
    }

    pub fn min(self) -> i128 {
        if self.signed() { if self.bits() == 128 { i128::MIN } else { -(1i128 << (self.bits() - 1)) } } else { 0 }
    }

    /// The largest value, as a `u128`.
    pub fn max(self) -> u128 {
        match (self.signed(), self.bits()) {
            (true, b) => (1u128 << (b - 1)) - 1,
            (false, 128) => u128::MAX,
            (false, b) => (1u128 << b) - 1,
        }
    }
}

/// A type written in the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TyName {
    Int(IntTy),
    Bool,
    Unit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntConst {
    Max,
    Min,
    Bits,
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    /// Where the expression itself starts: what `rustc` reports for a
    /// panic in it.
    pub span: Span,
    /// Where it starts with the parentheses around it, if any: what a
    /// binary expression it is the left side of starts with.
    pub outer: Span,
}

impl Expr {
    fn new(kind: ExprKind, span: Span) -> Expr {
        Expr { kind, span, outer: span }
    }
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Int(u128, Option<IntTy>),
    Bool(bool),
    Var(String),
    Call(String, Vec<Expr>),
    IntConst(IntTy, IntConst),
    BlackBox(Box<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Assign(String, Box<Expr>),
    CompoundAssign(BinOp, String, Box<Expr>),
    Cast(Box<Expr>, IntTy),
    If(Box<Expr>, Block, Option<Box<Expr>>),
    While(Box<Expr>, Block),
    Loop(Block),
    For { var: String, start: Box<Expr>, end: Box<Expr>, inclusive: bool, body: Block },
    Block(Block),
    Break,
    Continue,
    Return(Option<Box<Expr>>),
    Print(Print),
}

/// `print!`, `println!`, `eprint!`, `eprintln!`.
#[derive(Clone, Debug)]
pub struct Print {
    pub stderr: bool,
    pub pieces: Vec<Piece>,
    pub args: Vec<Expr>,
}

#[derive(Clone, Debug)]
pub enum Piece {
    Text(String),
    /// An argument, by index into `args`; `{:?}` and `{}` print integers and
    /// `bool`s alike.
    Arg(usize),
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub tail: Option<Box<Expr>>,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Let {
        name: String,
        mutable: bool,
        ty: Option<TyName>,
        init: Expr,
    },
    /// An expression statement; `semi` is whether a `;` followed it.
    Expr {
        expr: Expr,
        semi: bool,
    },
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: String,
    pub mutable: bool,
    pub ty: TyName,
}

#[derive(Clone, Debug)]
pub struct Func {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: TyName,
    pub body: Block,
}

// ---- lexing ----

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Int(u128, Option<IntTy>),
    Str(String),
    Punct(&'static str),
    Eof,
}

struct Token {
    tok: Tok,
    span: Span,
}

const PUNCTS: &[&str] = &[
    "<<=", ">>=", "..=", "::", "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "+=", "-=", "*=", "/=", "%=", "&=",
    "|=", "^=", "<<", ">>", "..", "+", "-", "*", "/", "%", "&", "|", "^", "!", "<", ">", "=", "(", ")", "{", "}", "[",
    "]", ",", ";", ":", ".", "#", "?", "@", "$", "'",
];

fn lex(src: &str) -> Result<Vec<Token>, Fallback> {
    let chars: Vec<char> = src.chars().collect();
    let (mut i, mut line, mut col) = (0usize, 1u32, 1u32);
    let mut out = Vec::new();
    let advance = |i: &mut usize, line: &mut u32, col: &mut u32, n: usize| {
        for _ in 0..n {
            if chars[*i] == '\n' {
                *line += 1;
                *col = 1;
            } else {
                *col += 1;
            }
            *i += 1;
        }
    };
    while i < chars.len() {
        let c = chars[i];
        let span = Span { line, col };
        if c.is_whitespace() {
            advance(&mut i, &mut line, &mut col, 1);
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            // Doc comments are attributes; leave those to rustc.
            if matches!(chars.get(i + 2), Some('/') | Some('!')) && chars.get(i + 3) != Some(&'/') {
                return Err(fallback("doc comments"));
            }
            while i < chars.len() && chars[i] != '\n' {
                advance(&mut i, &mut line, &mut col, 1);
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut depth = 0;
            loop {
                if i >= chars.len() {
                    return Err(fallback("an unterminated comment"));
                }
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    advance(&mut i, &mut line, &mut col, 2);
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    advance(&mut i, &mut line, &mut col, 2);
                    if depth == 0 {
                        break;
                    }
                } else {
                    advance(&mut i, &mut line, &mut col, 1);
                }
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                advance(&mut i, &mut line, &mut col, 1);
            }
            let word: String = chars[start..i].iter().collect();
            // Raw identifiers, raw strings, byte strings: rustc's.
            if matches!(chars.get(i), Some('#') | Some('"') | Some('\''))
                && matches!(word.as_str(), "r" | "b" | "br" | "c" | "cr")
            {
                return Err(fallback("raw identifiers and byte or raw strings"));
            }
            out.push(Token { tok: Tok::Ident(word), span });
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                advance(&mut i, &mut line, &mut col, 1);
            }
            // A float: a digit run followed by `.` and a digit, or an exponent.
            if chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
                return Err(fallback("floating-point literals"));
            }
            let text: String = chars[start..i].iter().filter(|&&c| c != '_').collect();
            out.push(Token { tok: int_literal(&text)?, span });
            continue;
        }
        if c == '"' {
            advance(&mut i, &mut line, &mut col, 1);
            let mut s = String::new();
            loop {
                let Some(&c) = chars.get(i) else { return Err(fallback("an unterminated string")) };
                advance(&mut i, &mut line, &mut col, 1);
                match c {
                    '"' => break,
                    '\\' => {
                        let Some(&e) = chars.get(i) else { return Err(fallback("an unterminated string")) };
                        advance(&mut i, &mut line, &mut col, 1);
                        match e {
                            'n' => s.push('\n'),
                            't' => s.push('\t'),
                            'r' => s.push('\r'),
                            '0' => s.push('\0'),
                            '\\' => s.push('\\'),
                            '"' => s.push('"'),
                            '\'' => s.push('\''),
                            _ => return Err(fallback("this string escape")),
                        }
                    }
                    // A line break in a string, and what rustc does with
                    // `\` before one, are rustc's to handle.
                    '\n' | '\r' => return Err(fallback("multi-line strings")),
                    c => s.push(c),
                }
            }
            out.push(Token { tok: Tok::Str(s), span });
            continue;
        }
        let rest: String = chars[i..chars.len().min(i + 3)].iter().collect();
        let Some(p) = PUNCTS.iter().find(|p| rest.starts_with(**p)) else {
            return Err(fallback(format!("the character `{c}`")));
        };
        advance(&mut i, &mut line, &mut col, p.len());
        out.push(Token { tok: Tok::Punct(p), span });
    }
    out.push(Token { tok: Tok::Eof, span: Span { line, col } });
    Ok(out)
}

fn int_literal(text: &str) -> Result<Tok, Fallback> {
    let (radix, digits) = match text.get(..2) {
        Some("0x") => (16, &text[2..]),
        Some("0o") => (8, &text[2..]),
        Some("0b") => (2, &text[2..]),
        _ => (10, text),
    };
    // A suffix starts at the first `i` or `u` (no digit is either).
    let (digits, suffix) = match digits.find(['i', 'u']) {
        Some(at) => (&digits[..at], Some(&digits[at..])),
        None => (digits, None),
    };
    let suffix = match suffix {
        Some(s) => Some(IntTy::from_name(s).ok_or_else(|| fallback(format!("the literal suffix `{s}`")))?),
        None => None,
    };
    if digits.is_empty() {
        return Err(fallback("this literal"));
    }
    let value = u128::from_str_radix(digits, radix).map_err(|_| fallback("this literal"))?;
    Ok(Tok::Int(value, suffix))
}

// ---- parsing ----

pub fn parse(src: &str) -> Result<Vec<Func>, Fallback> {
    let toks = lex(src)?;
    let mut p = Parser { toks, at: 0 };
    let mut funcs = Vec::new();
    while p.peek() != &Tok::Eof {
        funcs.push(p.func()?);
    }
    Ok(funcs)
}

struct Parser {
    toks: Vec<Token>,
    at: usize,
}

const KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in",
    "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
    "trait", "true", "type", "unsafe", "use", "where", "while", "async", "await", "dyn", "gen", "abstract", "become",
    "box", "do", "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "union",
];

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.at].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.at + n).min(self.toks.len() - 1)].tok
    }

    fn span(&self) -> Span {
        self.toks[self.at].span
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.at].tok.clone();
        if self.at + 1 < self.toks.len() {
            self.at += 1;
        }
        t
    }

    fn is(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::Punct(q) if *q == p)
    }

    fn is_kw(&self, k: &str) -> bool {
        matches!(self.peek(), Tok::Ident(w) if w == k)
    }

    fn eat(&mut self, p: &str) -> bool {
        if self.is(p) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, k: &str) -> bool {
        if self.is_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, p: &str) -> Result<(), Fallback> {
        if self.eat(p) { Ok(()) } else { Err(fallback(format!("syntax near `{:?}` (expected `{p}`)", self.peek()))) }
    }

    fn ident(&mut self) -> Result<String, Fallback> {
        match self.bump() {
            Tok::Ident(w) if !KEYWORDS.contains(&w.as_str()) => Ok(w),
            t => Err(fallback(format!("syntax near `{t:?}` (expected a name)"))),
        }
    }

    fn func(&mut self) -> Result<Func, Fallback> {
        if !self.eat_kw("fn") {
            return Err(fallback(format!("the item starting `{:?}`", self.peek())));
        }
        let name = self.ident()?;
        self.expect("(")?;
        let mut params = Vec::new();
        while !self.eat(")") {
            let mutable = self.eat_kw("mut");
            let name = self.ident()?;
            self.expect(":")?;
            let ty = self.ty()?;
            params.push(Param { name, mutable, ty });
            if !self.eat(",") {
                self.expect(")")?;
                break;
            }
        }
        let ret = if self.eat("->") { self.ty()? } else { TyName::Unit };
        let body = self.block()?;
        Ok(Func { name, params, ret, body })
    }

    fn ty(&mut self) -> Result<TyName, Fallback> {
        if self.eat("(") {
            self.expect(")")?;
            return Ok(TyName::Unit);
        }
        match self.bump() {
            Tok::Ident(w) if w == "bool" => Ok(TyName::Bool),
            Tok::Ident(w) => IntTy::from_name(&w).map(TyName::Int).ok_or_else(|| fallback(format!("the type `{w}`"))),
            t => Err(fallback(format!("the type starting `{t:?}`"))),
        }
    }

    fn block(&mut self) -> Result<Block, Fallback> {
        self.expect("{")?;
        let mut stmts = Vec::new();
        let mut tail = None;
        loop {
            if self.eat("}") {
                break;
            }
            if self.eat(";") {
                continue;
            }
            if self.eat_kw("let") {
                let mutable = self.eat_kw("mut");
                let name = self.ident()?;
                let ty = if self.eat(":") { Some(self.ty()?) } else { None };
                if !self.eat("=") {
                    return Err(fallback("`let` without an initializer"));
                }
                let init = self.expr()?;
                self.expect(";")?;
                stmts.push(Stmt::Let { name, mutable, ty, init });
                continue;
            }
            let block_like =
                self.is_kw("if") || self.is_kw("while") || self.is_kw("loop") || self.is_kw("for") || self.is("{");
            // A block-like expression at the start of a statement ends there:
            // `if c { .. } - 1` is two statements, as rustc reads it.
            let expr = if block_like { self.primary()? } else { self.expr()? };
            if self.eat(";") {
                stmts.push(Stmt::Expr { expr, semi: true });
            } else if self.eat("}") {
                tail = Some(Box::new(expr));
                break;
            } else if block_like {
                // `if c { .. } x` is two statements; rustc requires the first
                // to be `()`.
                stmts.push(Stmt::Expr { expr, semi: false });
            } else {
                return Err(fallback(format!("syntax near `{:?}`", self.peek())));
            }
        }
        Ok(Block { stmts, tail })
    }

    fn expr(&mut self) -> Result<Expr, Fallback> {
        let span = self.span();
        // Assignment: `name = e`, `name op= e`.
        if let (Tok::Ident(name), Tok::Punct(op)) = (self.peek().clone(), self.peek_at(1).clone())
            && !KEYWORDS.contains(&name.as_str())
        {
            let compound = match op {
                "+=" => Some(BinOp::Add),
                "-=" => Some(BinOp::Sub),
                "*=" => Some(BinOp::Mul),
                "/=" => Some(BinOp::Div),
                "%=" => Some(BinOp::Rem),
                "&=" => Some(BinOp::BitAnd),
                "|=" => Some(BinOp::BitOr),
                "^=" => Some(BinOp::BitXor),
                "<<=" => Some(BinOp::Shl),
                ">>=" => Some(BinOp::Shr),
                _ => None,
            };
            if op == "=" || compound.is_some() {
                self.bump();
                self.bump();
                let rhs = Box::new(self.expr()?);
                let kind = match compound {
                    Some(op) => ExprKind::CompoundAssign(op, name, rhs),
                    None => ExprKind::Assign(name, rhs),
                };
                return Ok(Expr::new(kind, span));
            }
        }
        self.binary(0)
    }

    fn binary(&mut self, min_prec: u8) -> Result<Expr, Fallback> {
        let mut lhs = self.unary()?;
        while let Tok::Punct(p) = self.peek().clone() {
            let (op, prec) = match p {
                "||" => (BinOp::Or, 1),
                "&&" => (BinOp::And, 2),
                "==" => (BinOp::Eq, 3),
                "!=" => (BinOp::Ne, 3),
                "<" => (BinOp::Lt, 3),
                "<=" => (BinOp::Le, 3),
                ">" => (BinOp::Gt, 3),
                ">=" => (BinOp::Ge, 3),
                "|" => (BinOp::BitOr, 4),
                "^" => (BinOp::BitXor, 5),
                "&" => (BinOp::BitAnd, 6),
                "<<" => (BinOp::Shl, 7),
                ">>" => (BinOp::Shr, 7),
                "+" => (BinOp::Add, 8),
                "-" => (BinOp::Sub, 8),
                "*" => (BinOp::Mul, 9),
                "/" => (BinOp::Div, 9),
                "%" => (BinOp::Rem, 9),
                _ => break,
            };
            if prec < min_prec {
                break;
            }
            self.bump();
            // Comparisons do not chain.
            let rhs = self.binary(prec + 1)?;
            if prec == 3 && matches!(self.peek(), Tok::Punct("==" | "!=" | "<" | "<=" | ">" | ">=")) {
                return Err(fallback("chained comparisons"));
            }
            let span = lhs.outer;
            lhs = Expr::new(ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), span);
        }
        Ok(lhs)
    }

    /// A prefix expression, then any `as` casts: `-x as u8` is `(-x) as u8`.
    fn unary(&mut self) -> Result<Expr, Fallback> {
        let mut e = self.prefix()?;
        while self.eat_kw("as") {
            let span = e.outer;
            let ty = match self.ty()? {
                TyName::Int(t) => t,
                other => return Err(fallback(format!("casts to `{other:?}`"))),
            };
            e = Expr::new(ExprKind::Cast(Box::new(e), ty), span);
        }
        Ok(e)
    }

    fn prefix(&mut self) -> Result<Expr, Fallback> {
        let span = self.span();
        if self.eat("-") {
            return Ok(Expr::new(ExprKind::Unary(UnOp::Neg, Box::new(self.prefix()?)), span));
        }
        if self.eat("!") {
            return Ok(Expr::new(ExprKind::Unary(UnOp::Not, Box::new(self.prefix()?)), span));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, Fallback> {
        let span = self.span();
        let kind = match self.peek().clone() {
            Tok::Int(v, s) => {
                self.bump();
                ExprKind::Int(v, s)
            }
            Tok::Punct("(") => {
                self.bump();
                if self.eat(")") {
                    return Err(fallback("the `()` value"));
                }
                let mut e = self.expr()?;
                self.expect(")")?;
                e.outer = span;
                return self.postfix_check(e);
            }
            Tok::Punct("{") => ExprKind::Block(self.block()?),
            Tok::Ident(w) => match w.as_str() {
                "true" | "false" => {
                    self.bump();
                    ExprKind::Bool(w == "true")
                }
                "if" => return self.if_expr(),
                "while" => {
                    self.bump();
                    let cond = self.cond()?;
                    ExprKind::While(Box::new(cond), self.block()?)
                }
                "loop" => {
                    self.bump();
                    ExprKind::Loop(self.block()?)
                }
                "for" => {
                    self.bump();
                    let var = self.ident()?;
                    if !self.eat_kw("in") {
                        return Err(fallback("this `for`"));
                    }
                    let start = self.no_struct_binary()?;
                    let inclusive = if self.eat("..=") {
                        true
                    } else if self.eat("..") {
                        false
                    } else {
                        return Err(fallback("`for` over something other than a range"));
                    };
                    let end = self.no_struct_binary()?;
                    let body = self.block()?;
                    ExprKind::For { var, start: Box::new(start), end: Box::new(end), inclusive, body }
                }
                "break" => {
                    self.bump();
                    if !self.is(";") && !self.is("}") {
                        return Err(fallback("`break` with a value or a label"));
                    }
                    ExprKind::Break
                }
                "continue" => {
                    self.bump();
                    ExprKind::Continue
                }
                "return" => {
                    self.bump();
                    if self.is(";") || self.is("}") {
                        ExprKind::Return(None)
                    } else {
                        ExprKind::Return(Some(Box::new(self.expr()?)))
                    }
                }
                _ => self.path_expr()?,
            },
            t => return Err(fallback(format!("the expression starting `{t:?}`"))),
        };
        self.postfix_check(Expr::new(kind, span))
    }

    /// Method calls, field access, indexing, `?`: not in the subset.
    fn postfix_check(&mut self, e: Expr) -> Result<Expr, Fallback> {
        if self.is(".") || self.is("[") || self.is("?") || self.is("(") {
            return Err(fallback(format!("`{:?}` after an expression", self.peek())));
        }
        Ok(e)
    }

    /// Conditions and range bounds: Rust parses them without struct
    /// literals, which the subset has none of anyway.
    fn cond(&mut self) -> Result<Expr, Fallback> {
        self.expr()
    }

    fn no_struct_binary(&mut self) -> Result<Expr, Fallback> {
        self.binary(0)
    }

    fn if_expr(&mut self) -> Result<Expr, Fallback> {
        let span = self.span();
        self.bump();
        if self.is_kw("let") {
            return Err(fallback("`if let`"));
        }
        let cond = self.cond()?;
        let then = self.block()?;
        let els = if self.eat_kw("else") {
            if self.is_kw("if") {
                Some(Box::new(self.if_expr()?))
            } else {
                let s = self.span();
                Some(Box::new(Expr::new(ExprKind::Block(self.block()?), s)))
            }
        } else {
            None
        };
        Ok(Expr::new(ExprKind::If(Box::new(cond), then, els), span))
    }

    fn path_expr(&mut self) -> Result<ExprKind, Fallback> {
        let mut path = vec![self.ident_or_kw()?];
        while self.eat("::") {
            path.push(self.ident_or_kw()?);
        }
        if self.eat("!") {
            return self.macro_call(&path);
        }
        let p: Vec<&str> = path.iter().map(String::as_str).collect();
        if self.eat("(") {
            let mut args = Vec::new();
            while !self.eat(")") {
                args.push(self.expr()?);
                if !self.eat(",") {
                    self.expect(")")?;
                    break;
                }
            }
            return match p.as_slice() {
                [name] => Ok(ExprKind::Call(name.to_string(), args)),
                ["std" | "core", "hint", "black_box"] if args.len() == 1 => {
                    Ok(ExprKind::BlackBox(Box::new(args.pop().expect("one argument"))))
                }
                _ => Err(fallback(format!("calling `{}`", path.join("::")))),
            };
        }
        match p.as_slice() {
            [name] => Ok(ExprKind::Var(name.to_string())),
            [ty, c] if IntTy::from_name(ty).is_some() => {
                let t = IntTy::from_name(ty).expect("checked");
                let c = match *c {
                    "MAX" => IntConst::Max,
                    "MIN" => IntConst::Min,
                    "BITS" => IntConst::Bits,
                    _ => return Err(fallback(format!("`{ty}::{c}`"))),
                };
                Ok(ExprKind::IntConst(t, c))
            }
            _ => Err(fallback(format!("the path `{}`", path.join("::")))),
        }
    }

    fn ident_or_kw(&mut self) -> Result<String, Fallback> {
        match self.bump() {
            Tok::Ident(w) => Ok(w),
            t => Err(fallback(format!("syntax near `{t:?}`"))),
        }
    }

    fn macro_call(&mut self, path: &[String]) -> Result<ExprKind, Fallback> {
        let (stderr, newline) = match path {
            [m] if m == "print" => (false, false),
            [m] if m == "println" => (false, true),
            [m] if m == "eprint" => (true, false),
            [m] if m == "eprintln" => (true, true),
            _ => return Err(fallback(format!("the macro `{}!`", path.join("::")))),
        };
        self.expect("(")?;
        let mut format = None;
        if let Tok::Str(s) = self.peek().clone() {
            self.bump();
            format = Some(s);
        }
        let mut args = Vec::new();
        if format.is_some() {
            while self.eat(",") {
                if self.is(")") {
                    break;
                }
                // Named arguments (`x = e`) are rustc's.
                if matches!(self.peek_at(1), Tok::Punct("=")) {
                    return Err(fallback("named format arguments"));
                }
                args.push(self.expr()?);
            }
        }
        self.expect(")")?;
        let format = match format {
            Some(f) => f,
            None if newline => String::new(),
            None => return Err(fallback("`print!` without a format string")),
        };
        let pieces = format_pieces(&format, &mut args, newline, self.span())?;
        Ok(ExprKind::Print(Print { stderr, pieces, args }))
    }
}

/// Split a format string into text and arguments. Implicit captures
/// (`{name}`) become arguments at the end, as `format_args!` makes them.
fn format_pieces(f: &str, args: &mut Vec<Expr>, newline: bool, span: Span) -> Result<Vec<Piece>, Fallback> {
    let explicit = args.len();
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut next = 0usize;
    let mut used = vec![false; explicit];
    let mut captures: Vec<String> = Vec::new();
    let mut chars = f.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                text.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                text.push('}');
            }
            '}' => return Err(fallback("an unmatched `}` in a format string")),
            '{' => {
                let mut spec = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => spec.push(c),
                        None => return Err(fallback("an unterminated `{` in a format string")),
                    }
                }
                let (name, fmt) = spec.split_once(':').unwrap_or((spec.as_str(), ""));
                if !fmt.is_empty() && fmt != "?" {
                    return Err(fallback(format!("the format spec `{{:{fmt}}}`")));
                }
                let index = if name.is_empty() {
                    next += 1;
                    next - 1
                } else if let Ok(i) = name.parse::<usize>() {
                    i
                } else if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !name.starts_with(|c: char| c.is_ascii_digit())
                {
                    match captures.iter().position(|c| c == name) {
                        Some(k) => explicit + k,
                        None => {
                            captures.push(name.to_owned());
                            explicit + captures.len() - 1
                        }
                    }
                } else {
                    return Err(fallback(format!("the format argument `{{{spec}}}`")));
                };
                if index < explicit {
                    used[index] = true;
                } else if index >= explicit + captures.len() {
                    return Err(fallback("a format argument out of range"));
                }
                if !text.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut text)));
                }
                pieces.push(Piece::Arg(index));
            }
            c => text.push(c),
        }
    }
    // Every explicit argument must be used, or rustc rejects the call.
    if used.iter().any(|u| !u) {
        return Err(fallback("an unused format argument"));
    }
    if newline {
        text.push('\n');
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    for name in captures {
        args.push(Expr::new(ExprKind::Var(name), span));
    }
    Ok(pieces)
}
