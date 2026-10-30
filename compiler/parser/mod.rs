pub mod ast;

pub use ast::*;

use crate::lexer::{InterpPart, LexError, Lexer, Span, Token, TokenKind};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ParseError at {}:{}: {}",
            self.span.line, self.span.col, self.message
        )
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        Self {
            message: e.message,
            span: e.span,
        }
    }
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    pub fn parse_source(src: &str) -> Result<Program, ParseError> {
        let tokens = Lexer::tokenize(src)?;
        let mut parser = Self::new(tokens);
        parser.parse_program()
    }

    fn cur(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .unwrap_or_else(|| self.tokens.last().expect("tokens must end with Eof"))
    }

    fn cur_kind(&self) -> &TokenKind {
        &self.cur().kind
    }

    fn cur_span(&self) -> Span {
        self.cur().span
    }

    fn prev_span(&self) -> Span {
        if self.pos > 0 {
            self.tokens[self.pos - 1].span
        } else {
            self.cur_span()
        }
    }

    fn at_eof(&self) -> bool {
        matches!(self.cur_kind(), TokenKind::Eof)
    }

    fn advance(&mut self) -> Token {
        let tok = self.cur().clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError {
            message: msg.into(),
            span: self.cur_span(),
        })
    }

    fn expect(&mut self, expected: &TokenKind) -> Result<Token, ParseError> {
        if self.cur_kind() == expected {
            Ok(self.advance())
        } else {
            self.err(format!(
                "expected `{:?}`, found `{:?}`",
                expected,
                self.cur_kind()
            ))
        }
    }

    fn expect_ident(&mut self) -> Result<(String, Span), ParseError> {
        match self.cur_kind().clone() {
            TokenKind::Ident(s) => {
                let sp = self.advance().span;
                Ok((s, sp))
            }
            other => self.err(format!("expected identifier, found `{:?}`", other)),
        }
    }

    fn expect_type_ident(&mut self) -> Result<(String, Span), ParseError> {
        match self.cur_kind().clone() {
            TokenKind::TypeIdent(s) => {
                let sp = self.advance().span;
                Ok((s, sp))
            }
            other => self.err(format!("expected type identifier, found `{:?}`", other)),
        }
    }

    fn skip_seps(&mut self) {
        while matches!(
            self.cur_kind(),
            TokenKind::Newline | TokenKind::Comma | TokenKind::Semicolon
        ) {
            self.advance();
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.cur_kind(), TokenKind::Newline) {
            self.advance();
        }
    }

    fn is_stmt_terminator(&self) -> bool {
        matches!(
            self.cur_kind(),
            TokenKind::Newline
                | TokenKind::Comma
                | TokenKind::Semicolon
                | TokenKind::RBrace
                | TokenKind::ComptimeClose
                | TokenKind::Eof
        )
    }

    pub fn parse_program(&mut self) -> Result<Program, ParseError> {
        let mut items = Vec::new();
        self.skip_seps();
        while !self.at_eof() {
            items.push(self.parse_stmt(true)?);
            self.skip_seps();
        }
        Ok(Program { items })
    }

    fn parse_block_until(&mut self, close_kind: TokenKind, open_span: Span) -> Result<Block, ParseError> {
        let mut stmts = Vec::new();
        self.skip_seps();
        while self.cur_kind() != &close_kind && !self.at_eof() {
            stmts.push(self.parse_stmt(false)?);
            self.skip_seps();
        }
        let close_tok = self.expect(&close_kind)?;
        Ok(Block {
            stmts,
            span: open_span.merge(close_tok.span),
        })
    }

    fn parse_brace_block(&mut self) -> Result<Block, ParseError> {
        let open = self.expect(&TokenKind::LBrace)?;
        self.parse_block_until(TokenKind::RBrace, open.span)
    }

    fn parse_stmt(&mut self, allow_pub: bool) -> Result<Stmt, ParseError> {
        let start_span = self.cur_span();

        // Defer / ErrDefer statements: ~{ ... } and ?!{ ... }
        if matches!(self.cur_kind(), TokenKind::DeferOpen) {
            let open = self.advance();
            let body = self.parse_block_until(TokenKind::RBrace, open.span)?;
            let span = start_span.merge(body.span);
            return Ok(Stmt {
                kind: StmtKind::Defer {
                    is_errdefer: false,
                    body,
                },
                span,
            });
        }
        if matches!(self.cur_kind(), TokenKind::ErrDeferOpen) {
            let open = self.advance();
            let body = self.parse_block_until(TokenKind::RBrace, open.span)?;
            let span = start_span.merge(body.span);
            return Ok(Stmt {
                kind: StmtKind::Defer {
                    is_errdefer: true,
                    body,
                },
                span,
            });
        }

        // Control-flow statements: ^, ^@, ^~, ^#, !^, !@, !#, ~@, ~#, or standalone `!` break
        if let Some(flow) = self.try_parse_flow_stmt()? {
            let span = start_span.merge(self.prev_span());
            return Ok(Stmt {
                kind: StmtKind::Flow(flow),
                span,
            });
        }

        // Check optional `+` export prefix
        let saved = self.pos;
        let is_pub = if matches!(self.cur_kind(), TokenKind::Plus) {
            if !allow_pub {
                return self.err("export `+` modifier is only allowed at module top level");
            }
            self.advance();
            true
        } else {
            false
        };

        // 1. MethodDef (`[+] TypeIdent.method[!] = ...`) or TypeDef (`[+] TypeIdent = ...`)
        if let TokenKind::TypeIdent(tname) = self.cur_kind().clone() {
            // Only user TypeIdents (uppercase) or primitive method extensions
            if self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|t| t.kind == TokenKind::Dot)
                && self
                    .tokens
                    .get(self.pos + 2)
                    .is_some_and(|t| matches!(t.kind, TokenKind::Ident(_)))
            {
                let saved_method = self.pos;
                self.advance(); // TypeIdent
                self.advance(); // .
                let (method_name, _) = self.expect_ident()?;
                let is_mutating = if matches!(self.cur_kind(), TokenKind::Bang) {
                    self.advance();
                    true
                } else {
                    false
                };
                if matches!(self.cur_kind(), TokenKind::Eq) {
                    self.advance();
                    let generics = self.try_parse_generic_params()?;
                    let mut func = self.parse_func_def()?;
                    if func.generics.is_empty() {
                        func.generics = generics.clone();
                    }
                    let span = start_span.merge(func.span);
                    return Ok(Stmt {
                        kind: StmtKind::MethodDef {
                            is_pub,
                            type_name: tname,
                            method_name,
                            is_mutating,
                            generics,
                            func,
                        },
                        span,
                    });
                } else {
                    self.pos = saved_method;
                }
            }

            if self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|t| t.kind == TokenKind::Eq)
            {
                self.advance(); // TypeIdent
                self.advance(); // =
                let generics = self.try_parse_generic_params()?;
                let def = self.parse_type_def_body()?;
                let span = start_span.merge(self.prev_span());
                return Ok(Stmt {
                    kind: StmtKind::TypeDef {
                        is_pub,
                        name: tname,
                        generics,
                        def,
                    },
                    span,
                });
            }
        }

        // 2. Single variable BindStmt: `[+] ident [TypeTag]? ("=" | "~=") Expr`
        if matches!(self.cur_kind(), TokenKind::Ident(_)) {
            let bind_saved = self.pos;
            let (name, _) = self.expect_ident()?;
            let ty = if matches!(self.cur_kind(), TokenKind::LBracket) {
                self.try_parse_type_tag()
            } else {
                None
            };
            if matches!(self.cur_kind(), TokenKind::Eq | TokenKind::TildeEq) {
                let is_mut = matches!(self.advance().kind, TokenKind::TildeEq);
                if is_pub && is_mut {
                    return self.err("exported module binding `+` cannot be mutable `~=`");
                }
                self.skip_newlines();
                let value = self.parse_expr()?;
                let span = start_span.merge(value.span);
                return Ok(Stmt {
                    kind: StmtKind::Bind {
                        is_pub,
                        is_mut,
                        target: BindTarget::Single { name, ty },
                        value,
                    },
                    span,
                });
            }
            self.pos = bind_saved;
        }

        // 3. Destructuring BindStmt or MutateStmt: `[a, b] = expr` / `[a, b] ~= expr` / `[a, b] <- expr`
        if !is_pub && matches!(self.cur_kind(), TokenKind::LBracket) {
            if let Some(names) = self.try_parse_destruct_pat() {
                if matches!(self.cur_kind(), TokenKind::Eq | TokenKind::TildeEq) {
                    let is_mut = matches!(self.advance().kind, TokenKind::TildeEq);
                    self.skip_newlines();
                    let value = self.parse_expr()?;
                    let span = start_span.merge(value.span);
                    return Ok(Stmt {
                        kind: StmtKind::Bind {
                            is_pub: false,
                            is_mut,
                            target: BindTarget::Destruct(names),
                            value,
                        },
                        span,
                    });
                } else if matches!(self.cur_kind(), TokenKind::LeftArrow) {
                    self.advance();
                    self.skip_newlines();
                    let value = self.parse_expr()?;
                    let span = start_span.merge(value.span);
                    return Ok(Stmt {
                        kind: StmtKind::Mutate {
                            target: MutTarget::Destruct(names),
                            op: MutOp::Assign,
                            value,
                        },
                        span,
                    });
                }
                self.pos = saved;
            }
        }

        if is_pub {
            return self.err("expected identifier or type definition after `+`");
        }

        // 4. Expression or LValue Mutation (`lvalue <- expr`, `+<-`, `-<-`, `*<-`, `/<-`)
        let expr = self.parse_expr()?;
        let mut_op = match self.cur_kind() {
            TokenKind::LeftArrow => Some(MutOp::Assign),
            TokenKind::PlusAssign => Some(MutOp::Add),
            TokenKind::MinusAssign => Some(MutOp::Sub),
            TokenKind::StarAssign => Some(MutOp::Mul),
            TokenKind::SlashAssign => Some(MutOp::Div),
            _ => None,
        };
        if let Some(op) = mut_op {
            self.advance();
            self.skip_newlines();
            let value = self.parse_expr()?;
            let span = start_span.merge(value.span);
            return Ok(Stmt {
                kind: StmtKind::Mutate {
                    target: MutTarget::LValue(expr),
                    op,
                    value,
                },
                span,
            });
        }

        let span = expr.span;
        Ok(Stmt {
            kind: StmtKind::Expr(expr),
            span,
        })
    }

    fn try_parse_destruct_pat(&mut self) -> Option<Vec<String>> {
        let saved = self.pos;
        if !matches!(self.cur_kind(), TokenKind::LBracket) {
            return None;
        }
        self.advance();
        self.skip_seps();
        let mut names = Vec::new();
        while !matches!(self.cur_kind(), TokenKind::RBracket | TokenKind::Eof) {
            match self.cur_kind().clone() {
                TokenKind::Ident(s) => {
                    names.push(s);
                    self.advance();
                }
                TokenKind::Underscore => {
                    names.push("_".to_string());
                    self.advance();
                }
                _ => {
                    self.pos = saved;
                    return None;
                }
            }
            if matches!(
                self.cur_kind(),
                TokenKind::Comma | TokenKind::Newline | TokenKind::Semicolon
            ) {
                self.skip_seps();
            } else if !matches!(self.cur_kind(), TokenKind::RBracket) {
                self.pos = saved;
                return None;
            }
        }
        if names.is_empty() || !matches!(self.cur_kind(), TokenKind::RBracket) {
            self.pos = saved;
            return None;
        }
        self.advance(); // ]
        if matches!(
            self.cur_kind(),
            TokenKind::Eq | TokenKind::TildeEq | TokenKind::LeftArrow
        ) {
            Some(names)
        } else {
            self.pos = saved;
            None
        }
    }

    fn try_parse_flow_stmt(&mut self) -> Result<Option<FlowKind>, ParseError> {
        match self.cur_kind().clone() {
            TokenKind::Caret => {
                self.advance();
                let expr = if self.is_stmt_terminator() {
                    None
                } else {
                    Some(Box::new(self.parse_expr()?))
                };
                Ok(Some(FlowKind::Return(expr)))
            }
            TokenKind::CaretAt => {
                self.advance();
                let expr = if self.is_stmt_terminator() {
                    None
                } else {
                    Some(Box::new(self.parse_expr()?))
                };
                Ok(Some(FlowKind::LocalReturn(expr)))
            }
            TokenKind::CaretHash => {
                self.advance();
                let (lbl, _) = self.expect_ident()?;
                let expr = if self.is_stmt_terminator() {
                    None
                } else {
                    Some(Box::new(self.parse_expr()?))
                };
                Ok(Some(FlowKind::LabeledReturn(lbl, expr)))
            }
            TokenKind::CaretTilde => {
                self.advance();
                let expr = self.parse_expr()?;
                Ok(Some(FlowKind::Yield(Box::new(expr))))
            }
            TokenKind::BangCaret => {
                self.advance();
                let expr = self.parse_expr()?;
                Ok(Some(FlowKind::Throw(Box::new(expr))))
            }
            TokenKind::BangAt => {
                self.advance();
                let expr = if self.is_stmt_terminator() {
                    None
                } else {
                    Some(Box::new(self.parse_expr()?))
                };
                Ok(Some(FlowKind::Break(expr)))
            }
            TokenKind::BangHash => {
                self.advance();
                let (lbl, _) = self.expect_ident()?;
                let expr = if self.is_stmt_terminator() {
                    None
                } else {
                    Some(Box::new(self.parse_expr()?))
                };
                Ok(Some(FlowKind::LabeledBreak(lbl, expr)))
            }
            TokenKind::TildeAt => {
                self.advance();
                Ok(Some(FlowKind::Continue))
            }
            TokenKind::TildeHash => {
                self.advance();
                let (lbl, _) = self.expect_ident()?;
                Ok(Some(FlowKind::LabeledContinue(lbl)))
            }
            // Part 1 shorthand `!` alone before terminator -> Break(None)
            TokenKind::Bang => {
                if self.tokens.get(self.pos + 1).is_some_and(|t| {
                    matches!(
                        t.kind,
                        TokenKind::Newline
                            | TokenKind::Comma
                            | TokenKind::Semicolon
                            | TokenKind::RBrace
                            | TokenKind::ComptimeClose
                            | TokenKind::Eof
                    )
                }) {
                    self.advance();
                    Ok(Some(FlowKind::Break(None)))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }

    // =========================================================================
    // Type Specs, Generics & Type Definitions
    // =========================================================================

    fn try_parse_generic_params(&mut self) -> Result<Vec<GenericParam>, ParseError> {
        let saved = self.pos;
        if !matches!(self.cur_kind(), TokenKind::Lt) {
            return Ok(Vec::new());
        }
        self.advance(); // <
        self.skip_seps();
        if !matches!(self.cur_kind(), TokenKind::TypeIdent(_)) {
            self.pos = saved;
            return Ok(Vec::new());
        }
        let mut params = Vec::new();
        while !matches!(self.cur_kind(), TokenKind::Gt | TokenKind::Eof) {
            let (name, sp) = match self.cur_kind().clone() {
                TokenKind::TypeIdent(s) if s.chars().next().unwrap().is_ascii_uppercase() => {
                    let s_sp = self.advance().span;
                    (s, s_sp)
                }
                _ => {
                    self.pos = saved;
                    return Ok(Vec::new());
                }
            };
            let bound = if matches!(self.cur_kind(), TokenKind::LBracket) {
                self.try_parse_type_tag()
            } else {
                None
            };
            let end_sp = self.prev_span();
            params.push(GenericParam {
                name,
                bound,
                span: sp.merge(end_sp),
            });
            self.skip_seps();
        }
        if !matches!(self.cur_kind(), TokenKind::Gt) {
            self.pos = saved;
            return Ok(Vec::new());
        }
        self.advance(); // >
        Ok(params)
    }

    fn try_parse_type_tag(&mut self) -> Option<TypeExpr> {
        let saved = self.pos;
        if !matches!(self.cur_kind(), TokenKind::LBracket) {
            return None;
        }
        self.advance(); // [
        self.skip_seps();
        if matches!(self.cur_kind(), TokenKind::RBracket) {
            self.advance();
            return Some(TypeExpr::Unit);
        }
        let ty = match self.parse_type_spec() {
            Ok(first) => {
                // Support tuple type tag `[i64, str]` if comma-separated TypeSpecs
                if matches!(self.cur_kind(), TokenKind::Comma) {
                    let mut elems = vec![first];
                    while matches!(self.cur_kind(), TokenKind::Comma) {
                        self.advance();
                        self.skip_seps();
                        if matches!(self.cur_kind(), TokenKind::RBracket) {
                            break;
                        }
                        match self.parse_type_spec() {
                            Ok(next) => elems.push(next),
                            Err(_) => {
                                self.pos = saved;
                                return None;
                            }
                        }
                    }
                    TypeExpr::Tuple(elems)
                } else {
                    first
                }
            }
            Err(_) => {
                self.pos = saved;
                return None;
            }
        };
        self.skip_seps();
        if matches!(self.cur_kind(), TokenKind::RBracket) {
            self.advance();
            Some(ty)
        } else {
            self.pos = saved;
            None
        }
    }

    fn parse_type_spec(&mut self) -> Result<TypeExpr, ParseError> {
        let lhs = self.parse_single_type_spec()?;
        if matches!(self.cur_kind(), TokenKind::FatArrow) {
            self.advance();
            let rhs = self.parse_type_spec()?;
            Ok(TypeExpr::Map(Box::new(lhs), Box::new(rhs)))
        } else {
            Ok(lhs)
        }
    }

    fn parse_single_type_spec(&mut self) -> Result<TypeExpr, ParseError> {
        // Empty unit type `[]` if nested
        if matches!(self.cur_kind(), TokenKind::LBracket) {
            if let Some(inner) = self.try_parse_type_tag() {
                return Ok(inner);
            }
        }

        // Function type `(T1, T2) [Ret]`
        if matches!(self.cur_kind(), TokenKind::LParen) {
            self.advance();
            self.skip_seps();
            let mut params = Vec::new();
            while !matches!(self.cur_kind(), TokenKind::RParen | TokenKind::Eof) {
                // Allow optional param name before TypeTag: `(x [i64])` or `(i64)`
                if matches!(self.cur_kind(), TokenKind::Ident(_))
                    && self
                        .tokens
                        .get(self.pos + 1)
                        .is_some_and(|t| t.kind == TokenKind::LBracket)
                {
                    self.advance();
                    let pty = self
                        .try_parse_type_tag()
                        .ok_or_else(|| ParseError {
                            message: "expected parameter type tag".into(),
                            span: self.cur_span(),
                        })?;
                    params.push(pty);
                } else {
                    params.push(self.parse_type_spec()?);
                }
                self.skip_seps();
            }
            self.expect(&TokenKind::RParen)?;
            let ret = self.try_parse_type_tag().ok_or_else(|| ParseError {
                message: "expected return `[Type]` in function type".into(),
                span: self.cur_span(),
            })?;
            return Ok(TypeExpr::Func {
                params,
                ret: Box::new(ret),
            });
        }

        // Prefix `~` (Channel/Task) or `%` (Set)
        let prefix = if matches!(self.cur_kind(), TokenKind::Tilde) {
            self.advance();
            Some('~')
        } else if matches!(self.cur_kind(), TokenKind::Percent) {
            self.advance();
            Some('%')
        } else {
            None
        };

        let (name, _) = self.expect_type_ident()?;
        let mut generics = Vec::new();
        if matches!(self.cur_kind(), TokenKind::Lt) {
            self.advance();
            self.skip_seps();
            while !matches!(self.cur_kind(), TokenKind::Gt | TokenKind::Eof) {
                generics.push(self.parse_type_spec()?);
                self.skip_seps();
            }
            self.expect(&TokenKind::Gt)?;
        }

        let mut ty = TypeExpr::Named { name, generics };
        match prefix {
            Some('~') => ty = TypeExpr::Channel(Box::new(ty)),
            Some('%') => ty = TypeExpr::Set(Box::new(ty)),
            _ => {}
        }

        // Compositional suffixes: `?` (Nullable) and `..` (Dynamic Array / Slice) in any order (e.g. `Order?..`, `i64..?`)
        loop {
            if matches!(self.cur_kind(), TokenKind::Question) {
                self.advance();
                ty = TypeExpr::Nullable(Box::new(ty));
            } else if matches!(self.cur_kind(), TokenKind::DotDot) {
                self.advance();
                ty = TypeExpr::Array(Box::new(ty));
            } else {
                break;
            }
        }

        Ok(ty)
    }

    fn parse_type_def_body(&mut self) -> Result<TypeDefKind, ParseError> {
        self.skip_newlines();
        self.expect(&TokenKind::LBracket)?;
        self.skip_seps();

        // Empty struct `DefaultPayload = []`
        if matches!(self.cur_kind(), TokenKind::RBracket) {
            self.advance();
            return Ok(TypeDefKind::Struct(Vec::new()));
        }

        // Check Enum VariantList: starts with uppercase `TypeIdent`
        if let TokenKind::TypeIdent(first_name) = self.cur_kind().clone() {
            if first_name.chars().next().unwrap().is_ascii_uppercase() {
                let mut variants = Vec::new();
                while !matches!(self.cur_kind(), TokenKind::RBracket | TokenKind::Eof) {
                    let (vname, vsp) = self.expect_type_ident()?;
                    let payload = if matches!(self.cur_kind(), TokenKind::LParen) {
                        self.advance();
                        self.skip_seps();
                        let mut tys = Vec::new();
                        while !matches!(self.cur_kind(), TokenKind::RParen | TokenKind::Eof) {
                            tys.push(self.parse_type_spec()?);
                            self.skip_seps();
                        }
                        self.expect(&TokenKind::RParen)?;
                        VariantPayload::Tuple(tys)
                    } else if matches!(self.cur_kind(), TokenKind::LBracket) {
                        self.advance();
                        let fs = self.parse_struct_field_defs()?;
                        self.expect(&TokenKind::RBracket)?;
                        VariantPayload::Struct(fs)
                    } else {
                        VariantPayload::Unit
                    };
                    let end_sp = self.prev_span();
                    variants.push(VariantDef {
                        name: vname,
                        payload,
                        span: vsp.merge(end_sp),
                    });
                    if matches!(self.cur_kind(), TokenKind::Bar) {
                        self.advance();
                    }
                    self.skip_seps();
                }
                self.expect(&TokenKind::RBracket)?;
                return Ok(TypeDefKind::Enum(variants));
            } else {
                // Primitive type alias e.g. `UserId = [i64]`
                let ty = self.parse_type_spec()?;
                self.skip_seps();
                self.expect(&TokenKind::RBracket)?;
                return Ok(TypeDefKind::Alias(ty));
            }
        }

        // Check Interface (`to_bytes = () [u8..]`) vs Struct Schema
        let saved_inside = self.pos;
        if let Some(methods) = self.try_parse_interface_methods() {
            self.expect(&TokenKind::RBracket)?;
            return Ok(TypeDefKind::Interface(methods));
        }
        self.pos = saved_inside;

        let fields = self.parse_struct_field_defs()?;
        self.expect(&TokenKind::RBracket)?;
        Ok(TypeDefKind::Struct(fields))
    }

    fn try_parse_interface_methods(&mut self) -> Option<Vec<(String, TypeExpr)>> {
        let mut methods = Vec::new();
        while !matches!(self.cur_kind(), TokenKind::RBracket | TokenKind::Eof) {
            let (mname, _) = self.expect_ident().ok()?;
            if !matches!(self.cur_kind(), TokenKind::Eq) {
                return None;
            }
            self.advance();
            if !matches!(self.cur_kind(), TokenKind::LParen) {
                return None;
            }
            let fty = self.parse_single_type_spec().ok()?;
            // Must NOT be followed by `{` (which would be a default function value in a struct)
            if matches!(self.cur_kind(), TokenKind::LBrace) {
                return None;
            }
            methods.push((mname, fty));
            self.skip_seps();
        }
        if methods.is_empty() {
            None
        } else {
            Some(methods)
        }
    }

    fn parse_struct_field_defs(&mut self) -> Result<Vec<StructFieldDef>, ParseError> {
        let mut fields = Vec::new();
        self.skip_seps();
        while !matches!(self.cur_kind(), TokenKind::RBracket | TokenKind::Eof) {
            let (name, fsp) = self.expect_ident()?;
            let mut ty = if matches!(self.cur_kind(), TokenKind::LBracket) {
                self.try_parse_type_tag()
            } else {
                None
            };
            let mut default = None;
            if matches!(self.cur_kind(), TokenKind::Eq) {
                self.advance();
                // Support `Attack[target = i64, dmg = i64]` where RHS of `=` is a TypeSpec
                let saved_rhs = self.pos;
                if ty.is_none() && matches!(self.cur_kind(), TokenKind::TypeIdent(_)) {
                    if let Ok(ts) = self.parse_type_spec() {
                        if matches!(
                            self.cur_kind(),
                            TokenKind::Comma
                                | TokenKind::Newline
                                | TokenKind::Semicolon
                                | TokenKind::RBracket
                        ) {
                            ty = Some(ts);
                        } else {
                            self.pos = saved_rhs;
                            default = Some(self.parse_expr()?);
                        }
                    } else {
                        self.pos = saved_rhs;
                        default = Some(self.parse_expr()?);
                    }
                } else {
                    default = Some(self.parse_expr()?);
                }
            }
            let end_sp = self.prev_span();
            fields.push(StructFieldDef {
                name,
                ty,
                default,
                span: fsp.merge(end_sp),
            });
            self.skip_seps();
        }
        Ok(fields)
    }

    // =========================================================================
    // Functions `(params) [Ret]? { ... }` and `{ ... }`
    // =========================================================================

    fn parse_func_def(&mut self) -> Result<FuncDef, ParseError> {
        self.skip_newlines();
        let start = self.cur_span();
        let generics = self.try_parse_generic_params()?;
        let params = if matches!(self.cur_kind(), TokenKind::LParen) {
            self.parse_param_list()?
        } else {
            Vec::new()
        };
        let ret_ty = if matches!(self.cur_kind(), TokenKind::LBracket) {
            self.try_parse_type_tag()
        } else {
            None
        };
        self.skip_newlines();
        let body = self.parse_brace_block()?;
        let span = start.merge(body.span);
        Ok(FuncDef {
            generics,
            params,
            ret_ty,
            body,
            span,
        })
    }

    fn parse_param_list(&mut self) -> Result<Vec<Param>, ParseError> {
        self.expect(&TokenKind::LParen)?;
        self.skip_seps();
        let mut params = Vec::new();
        while !matches!(self.cur_kind(), TokenKind::RParen | TokenKind::Eof) {
            let (name, sp) = self.expect_ident()?;
            let ty = if matches!(self.cur_kind(), TokenKind::LBracket) {
                self.try_parse_type_tag()
            } else {
                None
            };
            let default = if matches!(self.cur_kind(), TokenKind::Eq) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            let end_sp = self.prev_span();
            params.push(Param {
                name,
                ty,
                default,
                span: sp.merge(end_sp),
            });
            self.skip_seps();
        }
        self.expect(&TokenKind::RParen)?;
        Ok(params)
    }

    fn try_parse_func_expr(&mut self) -> Option<FuncDef> {
        let saved = self.pos;
        let generics = self.try_parse_generic_params().ok()?;
        if !generics.is_empty() {
            if let Ok(mut f) = self.parse_func_def() {
                f.generics = generics;
                return Some(f);
            }
            self.pos = saved;
            return None;
        }
        if matches!(self.cur_kind(), TokenKind::LBrace) {
            if let Ok(f) = self.parse_func_def() {
                return Some(f);
            }
            self.pos = saved;
            return None;
        }
        if matches!(self.cur_kind(), TokenKind::LParen) {
            if let Ok(params) = self.parse_param_list() {
                let ret_ty = if matches!(self.cur_kind(), TokenKind::LBracket) {
                    self.try_parse_type_tag()
                } else {
                    None
                };
                if matches!(self.cur_kind(), TokenKind::LBrace) {
                    if let Ok(body) = self.parse_brace_block() {
                        let span = self.tokens[saved].span.merge(body.span);
                        return Some(FuncDef {
                            generics: Vec::new(),
                            params,
                            ret_ty,
                            body,
                            span,
                        });
                    }
                }
            }
            self.pos = saved;
        }
        None
    }

    // =========================================================================
    // Stratified Pratt / Recursive Descent Expression Parser
    // =========================================================================

    pub fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_guard_expr()
    }

    fn parse_guard_expr(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_chan_send_expr()?;
        if matches!(self.cur_kind(), TokenKind::QuestCaret) {
            self.advance();
            let ret = if let Some(flow) = self.try_parse_flow_stmt()? {
                ElvisRhs::Flow(Box::new(flow))
            } else {
                ElvisRhs::Expr(Box::new(self.parse_expr()?))
            };
            let span = lhs.span.merge(self.prev_span());
            return Ok(Expr {
                kind: ExprKind::GuardReturn {
                    cond: Box::new(lhs),
                    ret,
                },
                span,
            });
        }
        Ok(lhs)
    }

    fn parse_chan_send_expr(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_pipe_expr()?;
        if matches!(self.cur_kind(), TokenKind::LeftPipe) {
            self.advance();
            let rhs = self.parse_pipe_expr()?;
            let span = lhs.span.merge(rhs.span);
            return Ok(Expr {
                kind: ExprKind::Binary {
                    op: BinOp::ChanSend,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            });
        }
        Ok(lhs)
    }

    fn peek_non_newline_kind(&self) -> (&TokenKind, usize) {
        let mut idx = self.pos;
        while idx < self.tokens.len() && matches!(self.tokens[idx].kind, TokenKind::Newline) {
            idx += 1;
        }
        let tok = self
            .tokens
            .get(idx)
            .unwrap_or_else(|| self.tokens.last().unwrap());
        (&tok.kind, idx)
    }

    fn parse_pipe_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_elvis_expr()?;
        loop {
            let (next_kind, idx) = self.peek_non_newline_kind();
            let pipe_kind = match next_kind {
                TokenKind::Pipe => Some(PipeKind::Seq),
                TokenKind::ParPipe => Some(PipeKind::Par),
                TokenKind::SafePipe => Some(PipeKind::Safe),
                _ => None,
            };
            let Some(kind) = pipe_kind else { break };
            self.pos = idx + 1;
            let stage = self.parse_pipe_stage()?;
            let span = lhs.span.merge(self.prev_span());
            lhs = Expr {
                kind: ExprKind::Pipe {
                    lhs: Box::new(lhs),
                    kind,
                    stage,
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_pipe_stage(&mut self) -> Result<PipeStage, ParseError> {
        self.skip_newlines();

        // Filter stage: `? { ... }`
        if matches!(self.cur_kind(), TokenKind::Question)
            && self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|t| t.kind == TokenKind::LBrace)
        {
            self.advance(); // ?
            let body = self.parse_brace_block()?;
            return Ok(PipeStage::Filter { body });
        }

        // Fold `(acc = init) { ... }` or named-param map `(row) { ... }`
        if matches!(self.cur_kind(), TokenKind::LParen) {
            let saved = self.pos;
            self.advance(); // (
            self.skip_seps();
            if let TokenKind::Ident(pname) = self.cur_kind().clone() {
                self.advance();
                if matches!(self.cur_kind(), TokenKind::Eq) {
                    self.advance();
                    let init = self.parse_expr()?;
                    self.skip_seps();
                    if matches!(self.cur_kind(), TokenKind::RParen) {
                        self.advance();
                        self.skip_newlines();
                        if matches!(self.cur_kind(), TokenKind::LBrace) {
                            let body = self.parse_brace_block()?;
                            return Ok(PipeStage::Fold {
                                acc_name: pname,
                                init: Box::new(init),
                                body,
                            });
                        }
                    }
                } else if matches!(self.cur_kind(), TokenKind::RParen) {
                    self.advance();
                    self.skip_newlines();
                    if matches!(self.cur_kind(), TokenKind::LBrace) {
                        let body = self.parse_brace_block()?;
                        return Ok(PipeStage::Map {
                            param: Some(pname),
                            body,
                        });
                    }
                }
            }
            self.pos = saved;
        }

        // Standard implicit-`@` block stage `{ ... }`
        if matches!(self.cur_kind(), TokenKind::LBrace) {
            let body = self.parse_brace_block()?;
            return Ok(PipeStage::Map { param: None, body });
        }

        let expr = self.parse_elvis_expr()?;
        Ok(PipeStage::Expr(Box::new(expr)))
    }

    fn parse_elvis_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_or_expr()?;
        loop {
            let (next_kind, idx) = self.peek_non_newline_kind();
            if matches!(next_kind, TokenKind::Elvis) {
                self.pos = idx + 1;
                self.skip_newlines();
                let rhs = if let Some(flow) = self.try_parse_flow_stmt()? {
                    ElvisRhs::Flow(Box::new(flow))
                } else {
                    ElvisRhs::Expr(Box::new(self.parse_or_expr()?))
                };
                let span = lhs.span.merge(self.prev_span());
                lhs = Expr {
                    kind: ExprKind::Elvis {
                        lhs: Box::new(lhs),
                        rhs,
                    },
                    span,
                };
            } else if matches!(next_kind, TokenKind::QuestBang)
                && !matches!(lhs.kind, ExprKind::Try { .. })
            {
                // Inline error fallback `expr ?! { ... }` (Part 1 syntax)
                let saved = self.pos;
                self.pos = idx + 1;
                if matches!(self.cur_kind(), TokenKind::LBrace) {
                    let body = self.parse_brace_block()?;
                    let span = lhs.span.merge(body.span);
                    lhs = Expr {
                        kind: ExprKind::ErrorFallback {
                            lhs: Box::new(lhs),
                            body,
                        },
                        span,
                    };
                } else {
                    self.pos = saved;
                    break;
                }
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_or_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and_expr()?;
        while matches!(self.cur_kind(), TokenKind::Bar) {
            self.advance();
            self.skip_newlines();
            let rhs = self.parse_and_expr()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op: BinOp::Or,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_and_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_bitor_expr()?;
        while matches!(self.cur_kind(), TokenKind::Amp) {
            self.advance();
            self.skip_newlines();
            let rhs = self.parse_bitor_expr()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op: BinOp::And,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_bitor_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_bitxor_expr()?;
        while matches!(self.cur_kind(), TokenKind::BitOr) {
            self.advance();
            let rhs = self.parse_bitxor_expr()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op: BinOp::BitOr,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_bitxor_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_bitand_expr()?;
        while matches!(self.cur_kind(), TokenKind::BitXor) {
            self.advance();
            let rhs = self.parse_bitand_expr()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op: BinOp::BitXor,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_bitand_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_comp_expr()?;
        while matches!(self.cur_kind(), TokenKind::BitAnd) {
            self.advance();
            let rhs = self.parse_comp_expr()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op: BinOp::BitAnd,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_comp_expr(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_range_expr()?;
        let op = match self.cur_kind() {
            TokenKind::EqEq => Some(BinOp::Eq),
            TokenKind::NotEq => Some(BinOp::NotEq),
            TokenKind::Lt => Some(BinOp::Lt),
            TokenKind::Gt => Some(BinOp::Gt),
            TokenKind::LtEq => Some(BinOp::LtEq),
            TokenKind::GtEq => Some(BinOp::GtEq),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let rhs = self.parse_range_expr()?;
            let span = lhs.span.merge(rhs.span);
            return Ok(Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            });
        }
        Ok(lhs)
    }

    fn parse_range_expr(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_shift_expr()?;
        if matches!(self.cur_kind(), TokenKind::DotDot | TokenKind::DotDotEq) {
            let inclusive = matches!(self.advance().kind, TokenKind::DotDotEq);
            let end = if matches!(
                self.cur_kind(),
                TokenKind::RBracket
                    | TokenKind::RParen
                    | TokenKind::RBrace
                    | TokenKind::Comma
                    | TokenKind::Newline
                    | TokenKind::Semicolon
                    | TokenKind::Pipe
                    | TokenKind::ParPipe
                    | TokenKind::SafePipe
                    | TokenKind::Eof
            ) {
                None
            } else {
                Some(Box::new(self.parse_shift_expr()?))
            };
            let span = lhs.span.merge(self.prev_span());
            return Ok(Expr {
                kind: ExprKind::Range {
                    start: Some(Box::new(lhs)),
                    end,
                    inclusive,
                },
                span,
            });
        }
        Ok(lhs)
    }

    fn parse_shift_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_add_expr()?;
        while matches!(self.cur_kind(), TokenKind::BitShl | TokenKind::BitShr) {
            let op = match self.advance().kind {
                TokenKind::BitShl => BinOp::BitShl,
                _ => BinOp::BitShr,
            };
            let rhs = self.parse_add_expr()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_add_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_mul_expr()?;
        while matches!(self.cur_kind(), TokenKind::Plus | TokenKind::Minus) {
            let op = match self.advance().kind {
                TokenKind::Plus => BinOp::Add,
                _ => BinOp::Sub,
            };
            let rhs = self.parse_mul_expr()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_mul_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary_expr()?;
        while matches!(
            self.cur_kind(),
            TokenKind::Star | TokenKind::Slash | TokenKind::Percent
        ) {
            let op = match self.advance().kind {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                _ => BinOp::Rem,
            };
            let rhs = self.parse_unary_expr()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_unary_expr(&mut self) -> Result<Expr, ParseError> {
        let start = self.cur_span();
        let unary_op = match self.cur_kind() {
            TokenKind::Minus => Some(UnaryOp::Neg),
            TokenKind::Bang => Some(UnaryOp::Not),
            TokenKind::BitNot => Some(UnaryOp::BitNot),
            TokenKind::LeftPipe => Some(UnaryOp::Await),
            _ => None,
        };
        if let Some(op) = unary_op {
            self.advance();
            let inner = self.parse_unary_expr()?;
            let span = start.merge(inner.span);
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op,
                    expr: Box::new(inner),
                },
                span,
            });
        }
        self.parse_postfix_expr()
    }

    fn parse_postfix_expr(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_primary_expr()?;

        loop {
            // `.ident` and `?.ident` can cross newlines for fluent method chains
            let (nl_kind, nl_idx) = self.peek_non_newline_kind();
            if matches!(nl_kind, TokenKind::Dot | TokenKind::SafeDot) {
                let safe = matches!(nl_kind, TokenKind::SafeDot);
                self.pos = nl_idx + 1;
                let (mut field, mut fsp) = match self.cur_kind().clone() {
                    TokenKind::Ident(s) | TokenKind::TypeIdent(s) => {
                        let sp = self.advance().span;
                        (s, sp)
                    }
                    other => {
                        return self.err(format!(
                            "expected field or method name after `.`, found `{:?}`",
                            other
                        ))
                    }
                };
                if matches!(self.cur_kind(), TokenKind::Bang) {
                    fsp = self.advance().span;
                    field.push('!');
                }
                let span = expr.span.merge(fsp);
                expr = Expr {
                    kind: ExprKind::Field {
                        expr: Box::new(expr),
                        field,
                        safe,
                    },
                    span,
                };
                continue;
            }

            // Remaining postfix ops stay on the same line (never cross unescaped Newline)
            match self.cur_kind().clone() {
                TokenKind::Lt => {
                    // Speculative `<TypeList>` generic args if followed by `(` or `[`
                    let saved = self.pos;
                    self.advance();
                    let mut gargs = Vec::new();
                    let mut ok = true;
                    while !matches!(self.cur_kind(), TokenKind::Gt | TokenKind::Eof) {
                        match self.parse_type_spec() {
                            Ok(t) => gargs.push(t),
                            Err(_) => {
                                ok = false;
                                break;
                            }
                        }
                        if matches!(self.cur_kind(), TokenKind::Comma) {
                            self.advance();
                        } else if !matches!(self.cur_kind(), TokenKind::Gt) {
                            ok = false;
                            break;
                        }
                    }
                    if ok
                        && !gargs.is_empty()
                        && matches!(self.cur_kind(), TokenKind::Gt)
                        && self.tokens.get(self.pos + 1).is_some_and(|t| {
                            matches!(t.kind, TokenKind::LParen | TokenKind::LBracket)
                        })
                    {
                        self.advance(); // >
                        if let ExprKind::TypeRef { name, .. } = expr.kind {
                            expr = Expr {
                                kind: ExprKind::TypeRef {
                                    name,
                                    generics: gargs,
                                },
                                span: expr.span.merge(self.prev_span()),
                            };
                            continue;
                        } else if matches!(self.cur_kind(), TokenKind::LParen) {
                            let (args, end_sp) = self.parse_arg_list()?;
                            let span = expr.span.merge(end_sp);
                            expr = Expr {
                                kind: ExprKind::Call {
                                    callee: Box::new(expr),
                                    generics: gargs,
                                    args,
                                    safe: false,
                                },
                                span,
                            };
                            continue;
                        }
                    }
                    self.pos = saved;
                    break;
                }
                TokenKind::LParen => {
                    let (args, end_sp) = self.parse_arg_list()?;
                    let span = expr.span.merge(end_sp);
                    expr = Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(expr),
                            generics: Vec::new(),
                            args,
                            safe: false,
                        },
                        span,
                    };
                }
                TokenKind::SafeLParen => {
                    self.advance(); // ?(
                    let (args, end_sp) = self.parse_arg_list_after_open()?;
                    let span = expr.span.merge(end_sp);
                    expr = Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(expr),
                            generics: Vec::new(),
                            args,
                            safe: true,
                        },
                        span,
                    };
                }
                TokenKind::LBracket => {
                    // 1. Struct / Variant construction: `Player[name = "Max", team = 1]` or `DefaultPayload[]`
                    if let ExprKind::TypeRef {
                        ref name,
                        ref generics,
                    } = expr.kind
                    {
                        if name.chars().next().unwrap().is_ascii_uppercase() {
                            let tname = name.clone();
                            let gens = generics.clone();
                            let start_sp = expr.span;
                            expr = self.parse_named_struct_init(tname, gens, start_sp)?;
                            continue;
                        }
                    }

                    // 2. Type cast/annotation: `val[i64]` or `0 [u8]`
                    if let Some(ty) = self.try_parse_type_tag() {
                        let span = expr.span.merge(self.prev_span());
                        expr = Expr {
                            kind: ExprKind::Cast {
                                expr: Box::new(expr),
                                ty,
                                safe: false,
                            },
                            span,
                        };
                        continue;
                    }

                    // 3. Slice `list[..2]`, `list[1..4]`, `list[3..]` or Index `list[i]`
                    self.advance(); // [
                    self.skip_seps();
                    if matches!(self.cur_kind(), TokenKind::DotDot | TokenKind::DotDotEq) {
                        let inclusive = matches!(self.advance().kind, TokenKind::DotDotEq);
                        let end = if matches!(self.cur_kind(), TokenKind::RBracket) {
                            None
                        } else {
                            Some(Box::new(self.parse_shift_expr()?))
                        };
                        let close = self.expect(&TokenKind::RBracket)?;
                        let span = expr.span.merge(close.span);
                        expr = Expr {
                            kind: ExprKind::Slice {
                                expr: Box::new(expr),
                                start: None,
                                end,
                                inclusive,
                            },
                            span,
                        };
                        continue;
                    }

                    let idx_or_range = self.parse_expr()?;
                    self.skip_seps();
                    let close = self.expect(&TokenKind::RBracket)?;
                    let span = expr.span.merge(close.span);
                    if let ExprKind::Range {
                        start,
                        end,
                        inclusive,
                    } = idx_or_range.kind
                    {
                        expr = Expr {
                            kind: ExprKind::Slice {
                                expr: Box::new(expr),
                                start,
                                end,
                                inclusive,
                            },
                            span,
                        };
                    } else {
                        expr = Expr {
                            kind: ExprKind::Index {
                                expr: Box::new(expr),
                                index: Box::new(idx_or_range),
                                safe: false,
                            },
                            span,
                        };
                    }
                }
                TokenKind::SafeLBracket => {
                    self.advance(); // ?[
                    self.skip_seps();
                    // Check safe cast `entity?[Admin]` vs safe index `list?[0]`
                    let saved_cast = self.pos;
                    if let Ok(ty) = self.parse_type_spec() {
                        if matches!(self.cur_kind(), TokenKind::RBracket) {
                            let close = self.advance();
                            let span = expr.span.merge(close.span);
                            expr = Expr {
                                kind: ExprKind::Cast {
                                    expr: Box::new(expr),
                                    ty,
                                    safe: true,
                                },
                                span,
                            };
                            continue;
                        }
                    }
                    self.pos = saved_cast;
                    let idx_expr = self.parse_expr()?;
                    self.skip_seps();
                    let close = self.expect(&TokenKind::RBracket)?;
                    let span = expr.span.merge(close.span);
                    expr = Expr {
                        kind: ExprKind::Index {
                            expr: Box::new(expr),
                            index: Box::new(idx_expr),
                            safe: true,
                        },
                        span,
                    };
                }
                TokenKind::BangBang => {
                    let bang = self.advance();
                    let span = expr.span.merge(bang.span);
                    expr = Expr {
                        kind: ExprKind::NonNullAssert(Box::new(expr)),
                        span,
                    };
                }
                TokenKind::Question => {
                    let q = self.advance();
                    let span = expr.span.merge(q.span);
                    expr = Expr {
                        kind: ExprKind::TryPropagate(Box::new(expr)),
                        span,
                    };
                }
                _ => break,
            }
        }

        Ok(expr)
    }

    fn parse_arg_list(&mut self) -> Result<(Vec<Expr>, Span), ParseError> {
        self.expect(&TokenKind::LParen)?;
        self.parse_arg_list_after_open()
    }

    fn parse_arg_list_after_open(&mut self) -> Result<(Vec<Expr>, Span), ParseError> {
        self.skip_seps();
        let mut args = Vec::new();
        while !matches!(self.cur_kind(), TokenKind::RParen | TokenKind::Eof) {
            args.push(self.parse_expr()?);
            self.skip_seps();
        }
        let close = self.expect(&TokenKind::RParen)?;
        Ok((args, close.span))
    }

    fn parse_named_struct_init(
        &mut self,
        type_name: String,
        generics: Vec<TypeExpr>,
        start_sp: Span,
    ) -> Result<Expr, ParseError> {
        self.expect(&TokenKind::LBracket)?;
        self.skip_seps();
        let mut spread = None;
        let mut fields = Vec::new();
        if matches!(self.cur_kind(), TokenKind::DotDot) {
            self.advance();
            spread = Some(Box::new(self.parse_expr()?));
            self.skip_seps();
        }
        while !matches!(self.cur_kind(), TokenKind::RBracket | TokenKind::Eof) {
            let (fname, _) = self.expect_ident()?;
            self.expect(&TokenKind::Eq)?;
            let val = self.parse_expr()?;
            fields.push((fname, val));
            self.skip_seps();
        }
        let close = self.expect(&TokenKind::RBracket)?;
        Ok(Expr {
            kind: ExprKind::StructLit {
                type_name: Some(type_name),
                generics,
                spread,
                fields,
            },
            span: start_sp.merge(close.span),
        })
    }

    fn parse_primary_expr(&mut self) -> Result<Expr, ParseError> {
        let start = self.cur_span();

        // Direct `{ ... }` in expression position is always a 0-arg FuncExpr; propagate inner errors directly
        if matches!(self.cur_kind(), TokenKind::LBrace) {
            let f = self.parse_func_def()?;
            let span = f.span;
            return Ok(Expr {
                kind: ExprKind::Func(Box::new(f)),
                span,
            });
        }

        // Check FuncExpr (`(params) [Ret]? { ... }`, `<T> (params) { ... }`) BEFORE grouped `(Expr)`
        if matches!(self.cur_kind(), TokenKind::LParen | TokenKind::Lt) {
            if let Some(f) = self.try_parse_func_expr() {
                let span = f.span;
                return Ok(Expr {
                    kind: ExprKind::Func(Box::new(f)),
                    span,
                });
            }
        }

        match self.cur_kind().clone() {
            TokenKind::IntLit(n) => {
                let sp = self.advance().span;
                Ok(Expr {
                    kind: ExprKind::Int(n),
                    span: sp,
                })
            }
            TokenKind::FloatLit(x) => {
                let sp = self.advance().span;
                Ok(Expr {
                    kind: ExprKind::Float(x),
                    span: sp,
                })
            }
            TokenKind::BoolLit(b) => {
                let sp = self.advance().span;
                Ok(Expr {
                    kind: ExprKind::Bool(b),
                    span: sp,
                })
            }
            TokenKind::CharLit(c) => {
                let sp = self.advance().span;
                Ok(Expr {
                    kind: ExprKind::Char(c),
                    span: sp,
                })
            }
            TokenKind::StringLit(s) => {
                let sp = self.advance().span;
                Ok(Expr {
                    kind: ExprKind::Str(s),
                    span: sp,
                })
            }
            TokenKind::InterpString(parts) => {
                let sp = self.advance().span;
                let mut segs = Vec::with_capacity(parts.len());
                for part in parts {
                    match part {
                        InterpPart::Literal(lit) => segs.push(InterpSegment::Literal(lit)),
                        InterpPart::ExprTokens(toks) => {
                            let mut sub_parser = Parser::new(toks);
                            let e = sub_parser.parse_expr()?;
                            segs.push(InterpSegment::Expr(e));
                        }
                    }
                }
                Ok(Expr {
                    kind: ExprKind::InterpString(segs),
                    span: sp,
                })
            }
            TokenKind::Tilde => {
                let sp = self.advance().span;
                Ok(Expr {
                    kind: ExprKind::Null,
                    span: sp,
                })
            }
            TokenKind::At => {
                // Loop `@ (cond_or_iter) { ... }` or `@ { ... }` vs CurrentItem `@`
                if self.tokens.get(self.pos + 1).is_some_and(|t| {
                    matches!(t.kind, TokenKind::LParen | TokenKind::LBrace)
                }) {
                    return self.parse_loop_expr();
                }
                let sp = self.advance().span;
                Ok(Expr {
                    kind: ExprKind::CurrentItem,
                    span: sp,
                })
            }
            TokenKind::Dollar => {
                let sp = self.advance().span;
                Ok(Expr {
                    kind: ExprKind::SelfFn,
                    span: sp,
                })
            }
            TokenKind::Ident(s) => {
                let sp = self.advance().span;
                Ok(Expr {
                    kind: ExprKind::Ident(s),
                    span: sp,
                })
            }
            TokenKind::TypeIdent(s) => {
                let sp = self.advance().span;
                Ok(Expr {
                    kind: ExprKind::TypeRef {
                        name: s,
                        generics: Vec::new(),
                    },
                    span: sp,
                })
            }
            TokenKind::Label(lbl) => {
                let sp = self.advance().span;
                self.skip_newlines();
                let inner = if matches!(self.cur_kind(), TokenKind::At) {
                    self.parse_loop_expr()?
                } else {
                    let blk = self.parse_brace_block()?;
                    let bsp = blk.span;
                    Expr {
                        kind: ExprKind::BlockExpr(blk),
                        span: bsp,
                    }
                };
                let span = sp.merge(inner.span);
                Ok(Expr {
                    kind: ExprKind::Labeled {
                        label: lbl,
                        expr: Box::new(inner),
                    },
                    span,
                })
            }
            TokenKind::Question | TokenKind::SafeLParen | TokenKind::SafeLBracket => {
                self.parse_cond_or_match_expr()
            }
            TokenKind::TryOpen => self.parse_try_expr(),
            TokenKind::AsyncOpen => {
                let open = self.advance();
                let blk = self.parse_block_until(TokenKind::RBrace, open.span)?;
                let span = blk.span;
                Ok(Expr {
                    kind: ExprKind::AsyncSpawn(blk),
                    span,
                })
            }
            TokenKind::ComptimeOpen => {
                let open = self.advance();
                let blk = self.parse_block_until(TokenKind::ComptimeClose, open.span)?;
                let span = blk.span;
                Ok(Expr {
                    kind: ExprKind::Comptime(blk),
                    span,
                })
            }
            TokenKind::Lt => self.parse_meta_bracket(),
            TokenKind::LBracket => self.parse_data_bracket(),
            TokenKind::LParen => {
                let open = self.advance();
                self.skip_seps();
                if matches!(self.cur_kind(), TokenKind::RParen) {
                    let close = self.advance();
                    return Ok(Expr {
                        kind: ExprKind::Unit,
                        span: open.span.merge(close.span),
                    });
                }
                let inner = self.parse_expr()?;
                self.skip_seps();
                let close = self.expect(&TokenKind::RParen)?;
                Ok(Expr {
                    kind: inner.kind,
                    span: open.span.merge(close.span),
                })
            }
            other => self.err(format!(
                "expected expression, found `{:?}` at {}:{}",
                other, start.line, start.col
            )),
        }
    }

    // =========================================================================
    // MetaBracket `<"std/math">` & `<"c" "puts" (str) [i32]>`
    // =========================================================================

    fn parse_meta_bracket(&mut self) -> Result<Expr, ParseError> {
        let open = self.expect(&TokenKind::Lt)?;
        self.skip_seps();
        let first_str = match self.cur_kind().clone() {
            TokenKind::StringLit(s) => {
                self.advance();
                s
            }
            other => {
                return self.err(format!(
                    "expected module path or `\"c\"` inside `< ... >`, found `{:?}`",
                    other
                ))
            }
        };
        self.skip_seps();

        // Import `<"std/math">` or `<"./module.lo">`
        if matches!(self.cur_kind(), TokenKind::Gt) {
            let close = self.advance();
            return Ok(Expr {
                kind: ExprKind::Import(first_str),
                span: open.span.merge(close.span),
            });
        }

        // Extern C FFI `<"c" "puts" (str) [i32]>`
        if first_str != "c" {
            return self.err(format!(
                "expected `\"c\"` ABI tag for FFI declaration, found `{:?}`",
                first_str
            ));
        }
        let symbol = match self.cur_kind().clone() {
            TokenKind::StringLit(s) => {
                self.advance();
                s
            }
            other => {
                return self.err(format!(
                    "expected C symbol string literal, found `{:?}`",
                    other
                ))
            }
        };
        self.skip_seps();
        self.expect(&TokenKind::LParen)?;
        self.skip_seps();
        let mut params = Vec::new();
        while !matches!(self.cur_kind(), TokenKind::RParen | TokenKind::Eof) {
            params.push(self.parse_type_spec()?);
            self.skip_seps();
        }
        self.expect(&TokenKind::RParen)?;
        self.skip_seps();
        let ret = self
            .try_parse_type_tag()
            .unwrap_or(TypeExpr::Unit);
        self.skip_seps();
        let close = self.expect(&TokenKind::Gt)?;
        Ok(Expr {
            kind: ExprKind::ExternC {
                symbol,
                params,
                ret,
            },
            span: open.span.merge(close.span),
        })
    }

    // =========================================================================
    // DataBracket `[ ... ]` (Unit, Set, StructLit, MapLit, RepeatArray, Array, TypeVal)
    // =========================================================================

    fn parse_data_bracket(&mut self) -> Result<Expr, ParseError> {
        let open = self.expect(&TokenKind::LBracket)?;
        self.skip_seps();

        // 1. Empty `[]`
        if matches!(self.cur_kind(), TokenKind::RBracket) {
            let close = self.advance();
            return Ok(Expr {
                kind: ExprKind::Unit,
                span: open.span.merge(close.span),
            });
        }

        // 2. HashSet `[% e1, e2, ...]` (note: `[%Type]` followed immediately by `]` when TypeIdent -> TypeVal)
        if matches!(self.cur_kind(), TokenKind::Percent) {
            let saved_set = self.pos;
            self.advance();
            if matches!(self.cur_kind(), TokenKind::TypeIdent(_)) {
                if let Ok(inner_ty) = self.parse_type_spec() {
                    if matches!(self.cur_kind(), TokenKind::RBracket) {
                        let close = self.advance();
                        return Ok(Expr {
                            kind: ExprKind::TypeVal(TypeExpr::Set(Box::new(inner_ty))),
                            span: open.span.merge(close.span),
                        });
                    }
                }
            }
            self.pos = saved_set + 1;
            self.skip_seps();
            let mut elems = Vec::new();
            while !matches!(self.cur_kind(), TokenKind::RBracket | TokenKind::Eof) {
                elems.push(self.parse_expr()?);
                self.skip_seps();
            }
            let close = self.expect(&TokenKind::RBracket)?;
            return Ok(Expr {
                kind: ExprKind::SetLit(elems),
                span: open.span.merge(close.span),
            });
        }

        // 3. Channel/Type constructor `[~i64]` -> TypeVal(Channel(i64))
        if matches!(self.cur_kind(), TokenKind::Tilde)
            && self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|t| matches!(t.kind, TokenKind::TypeIdent(_)))
        {
            let saved_ty = self.pos;
            if let Ok(ty) = self.parse_type_spec() {
                if matches!(self.cur_kind(), TokenKind::RBracket) {
                    let close = self.advance();
                    return Ok(Expr {
                        kind: ExprKind::TypeVal(ty),
                        span: open.span.merge(close.span),
                    });
                }
            }
            self.pos = saved_ty;
        }

        // 4. Struct update with spread `[..user, hp = user.hp - 25]`
        if matches!(self.cur_kind(), TokenKind::DotDot) {
            self.advance();
            let base = self.parse_expr()?;
            self.skip_seps();
            let mut fields = Vec::new();
            while !matches!(self.cur_kind(), TokenKind::RBracket | TokenKind::Eof) {
                let (fname, _) = self.expect_ident()?;
                self.expect(&TokenKind::Eq)?;
                let val = self.parse_expr()?;
                fields.push((fname, val));
                self.skip_seps();
            }
            let close = self.expect(&TokenKind::RBracket)?;
            return Ok(Expr {
                kind: ExprKind::StructLit {
                    type_name: None,
                    generics: Vec::new(),
                    spread: Some(Box::new(base)),
                    fields,
                },
                span: open.span.merge(close.span),
            });
        }

        // 5. Anonymous struct literal `[x = 12.5, y = -4.0]`
        if matches!(self.cur_kind(), TokenKind::Ident(_))
            && self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|t| t.kind == TokenKind::Eq)
        {
            let mut fields = Vec::new();
            while !matches!(self.cur_kind(), TokenKind::RBracket | TokenKind::Eof) {
                let (fname, _) = self.expect_ident()?;
                self.expect(&TokenKind::Eq)?;
                let val = self.parse_expr()?;
                fields.push((fname, val));
                self.skip_seps();
            }
            let close = self.expect(&TokenKind::RBracket)?;
            return Ok(Expr {
                kind: ExprKind::StructLit {
                    type_name: None,
                    generics: Vec::new(),
                    spread: None,
                    fields,
                },
                span: open.span.merge(close.span),
            });
        }

        // 6. Repeat array `[0 [u8] .. 1024]` or Map `[k => v]` or Array `[e1, e2, ...]`
        let first = self.parse_expr()?;

        // Check Map `[k => v, ...]`
        if matches!(self.cur_kind(), TokenKind::FatArrow) {
            self.advance();
            let first_val = self.parse_expr()?;
            let mut entries = vec![(first, first_val)];
            self.skip_seps();
            while !matches!(self.cur_kind(), TokenKind::RBracket | TokenKind::Eof) {
                let k = self.parse_expr()?;
                self.expect(&TokenKind::FatArrow)?;
                let v = self.parse_expr()?;
                entries.push((k, v));
                self.skip_seps();
            }
            let close = self.expect(&TokenKind::RBracket)?;
            return Ok(Expr {
                kind: ExprKind::MapLit(entries),
                span: open.span.merge(close.span),
            });
        }

        // Check RepeatArray `[0 [u8] .. 1024]` (parsed as Range { start: Some(Cast { expr: val, ty }), end: Some(count), inclusive: false })
        if matches!(self.cur_kind(), TokenKind::RBracket) {
            if let ExprKind::Range {
                start: Some(ref s),
                end: Some(ref c),
                inclusive: false,
            } = first.kind
            {
                if let ExprKind::Cast {
                    ref expr,
                    ref ty,
                    safe: false,
                } = s.kind
                {
                    let close = self.advance();
                    return Ok(Expr {
                        kind: ExprKind::RepeatArray {
                            value: expr.clone(),
                            elem_ty: Some(ty.clone()),
                            count: c.clone(),
                        },
                        span: open.span.merge(close.span),
                    });
                }
            }
        }

        let mut elems = vec![first];
        self.skip_seps();
        while !matches!(self.cur_kind(), TokenKind::RBracket | TokenKind::Eof) {
            elems.push(self.parse_expr()?);
            self.skip_seps();
        }
        let close = self.expect(&TokenKind::RBracket)?;
        Ok(Expr {
            kind: ExprKind::Array(elems),
            span: open.span.merge(close.span),
        })
    }

    // =========================================================================
    // Conditionals `? (...) {}`, Match `? [...] {}`, Loops `@`, Try `!{}`
    // =========================================================================

    fn parse_cond_or_match_expr(&mut self) -> Result<Expr, ParseError> {
        let start = self.cur_span();

        // 1. Match `? [scrutinee] { arms }` or `?[scrutinee] { arms }`
        let is_bracket_match = matches!(self.cur_kind(), TokenKind::SafeLBracket)
            || (matches!(self.cur_kind(), TokenKind::Question)
                && self
                    .tokens
                    .get(self.pos + 1)
                    .is_some_and(|t| t.kind == TokenKind::LBracket));
        if is_bracket_match {
            if matches!(self.cur_kind(), TokenKind::SafeLBracket) {
                self.advance(); // ?[
            } else {
                self.advance(); // ?
                self.advance(); // [
            }
            self.skip_seps();
            let scrutinee = self.parse_expr()?;
            self.skip_seps();
            self.expect(&TokenKind::RBracket)?;
            self.skip_newlines();
            let arms = self.parse_match_arms()?;
            let span = start.merge(self.prev_span());
            return Ok(Expr {
                kind: ExprKind::Match {
                    scrutinee: Box::new(scrutinee),
                    arms,
                },
                span,
            });
        }

        // 2. Multi-branch `? { (c1) { b1 } _ { else_b } }` (Part 1 syntax)
        if matches!(self.cur_kind(), TokenKind::Question)
            && self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|t| t.kind == TokenKind::LBrace)
        {
            self.advance(); // ?
            self.advance(); // {
            self.skip_seps();
            let mut branches = Vec::new();
            let mut else_block = None;
            while !matches!(self.cur_kind(), TokenKind::RBrace | TokenKind::Eof) {
                if matches!(self.cur_kind(), TokenKind::Underscore) {
                    self.advance();
                    else_block = Some(self.parse_brace_block()?);
                } else {
                    self.expect(&TokenKind::LParen)?;
                    let cond = self.parse_expr()?;
                    self.expect(&TokenKind::RParen)?;
                    let blk = self.parse_brace_block()?;
                    branches.push((cond, blk));
                }
                self.skip_seps();
            }
            let close = self.expect(&TokenKind::RBrace)?;
            return Ok(Expr {
                kind: ExprKind::If {
                    branches,
                    else_block,
                },
                span: start.merge(close.span),
            });
        }

        // 3. `? (cond_or_scrutinee) ...`
        if matches!(self.cur_kind(), TokenKind::SafeLParen) {
            self.advance(); // ?(
        } else {
            self.expect(&TokenKind::Question)?;
            self.expect(&TokenKind::LParen)?;
        }
        self.skip_seps();
        let first_cond = self.parse_expr()?;
        self.skip_seps();
        self.expect(&TokenKind::RParen)?;
        self.skip_newlines();

        // Check Part 1 pattern match `? (state) { Idle { ... } Run(s) { ... } }`
        if self.looks_like_legacy_match_block() {
            let arms = self.parse_match_arms()?;
            let span = start.merge(self.prev_span());
            return Ok(Expr {
                kind: ExprKind::Match {
                    scrutinee: Box::new(first_cond),
                    arms,
                },
                span,
            });
        }

        let first_block = self.parse_brace_block()?;
        let mut branches = vec![(first_cond, first_block)];
        let mut else_block = None;

        // Parse chained `(cond2) { block2 }` else-if branches and optional `{ else_block }`
        loop {
            let saved = self.pos;
            self.skip_newlines();
            if matches!(self.cur_kind(), TokenKind::LParen) {
                self.advance();
                self.skip_seps();
                if let Ok(cond) = self.parse_expr() {
                    self.skip_seps();
                    if matches!(self.cur_kind(), TokenKind::RParen) {
                        self.advance();
                        self.skip_newlines();
                        if matches!(self.cur_kind(), TokenKind::LBrace) {
                            let blk = self.parse_brace_block()?;
                            branches.push((cond, blk));
                            continue;
                        }
                    }
                }
                self.pos = saved;
                break;
            } else if matches!(self.cur_kind(), TokenKind::LBrace) {
                // Only attach `{ else }` on the same line or directly after `}`
                else_block = Some(self.parse_brace_block()?);
                break;
            } else {
                self.pos = saved;
                break;
            }
        }

        let span = start.merge(self.prev_span());
        Ok(Expr {
            kind: ExprKind::If {
                branches,
                else_block,
            },
            span,
        })
    }

    fn looks_like_legacy_match_block(&self) -> bool {
        if !matches!(self.cur_kind(), TokenKind::LBrace) {
            return false;
        }
        let mut idx = self.pos + 1;
        while idx < self.tokens.len() && matches!(self.tokens[idx].kind, TokenKind::Newline) {
            idx += 1;
        }
        if let Some(Token {
            kind: TokenKind::TypeIdent(tn),
            ..
        }) = self.tokens.get(idx)
        {
            if tn.chars().next().unwrap().is_ascii_uppercase() {
                // In normal blocks, uppercase TypeIdent is followed by `=`, `.`, `[`, or `<`.
                // If followed directly by `{` or `(ident) {` or `[pat] {`, it's a legacy MatchArm!
                if let Some(next_tok) = self.tokens.get(idx + 1) {
                    if matches!(next_tok.kind, TokenKind::LBrace | TokenKind::LParen) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn parse_match_arms(&mut self) -> Result<Vec<MatchArm>, ParseError> {
        self.expect(&TokenKind::LBrace)?;
        self.skip_seps();
        let mut arms = Vec::new();
        while !matches!(self.cur_kind(), TokenKind::RBrace | TokenKind::Eof) {
            let arm_start = self.cur_span();
            let pattern = self.parse_pattern()?;
            let guard = if matches!(self.cur_kind(), TokenKind::LParen) {
                self.advance();
                let g = self.parse_expr()?;
                self.expect(&TokenKind::RParen)?;
                Some(g)
            } else {
                None
            };
            self.skip_newlines();
            let body = self.parse_brace_block()?;
            let span = arm_start.merge(body.span);
            arms.push(MatchArm {
                pattern,
                guard,
                body,
                span,
            });
            self.skip_seps();
        }
        self.expect(&TokenKind::RBrace)?;
        Ok(arms)
    }

    fn parse_pattern(&mut self) -> Result<Pattern, ParseError> {
        let first = self.parse_single_pattern()?;
        if matches!(self.cur_kind(), TokenKind::Bar) {
            let mut alts = vec![first];
            while matches!(self.cur_kind(), TokenKind::Bar) {
                self.advance();
                alts.push(self.parse_single_pattern()?);
            }
            Ok(Pattern::Or(alts))
        } else {
            Ok(first)
        }
    }

    fn parse_single_pattern(&mut self) -> Result<Pattern, ParseError> {
        match self.cur_kind().clone() {
            TokenKind::Underscore => {
                self.advance();
                Ok(Pattern::Wildcard)
            }
            TokenKind::Tilde => {
                self.advance();
                Ok(Pattern::Null)
            }
            TokenKind::BoolLit(b) => {
                self.advance();
                Ok(Pattern::Bool(b))
            }
            TokenKind::CharLit(c) => {
                let sp = self.advance().span;
                if matches!(self.cur_kind(), TokenKind::DotDot | TokenKind::DotDotEq) {
                    let inclusive = matches!(self.advance().kind, TokenKind::DotDotEq);
                    let end = self.parse_shift_expr()?;
                    return Ok(Pattern::Range {
                        start: Box::new(Expr {
                            kind: ExprKind::Char(c),
                            span: sp,
                        }),
                        end: Box::new(end),
                        inclusive,
                    });
                }
                Ok(Pattern::Char(c))
            }
            TokenKind::StringLit(s) => {
                self.advance();
                Ok(Pattern::Str(s))
            }
            TokenKind::Minus | TokenKind::IntLit(_) | TokenKind::FloatLit(_) => {
                let start_sp = self.cur_span();
                let neg = if matches!(self.cur_kind(), TokenKind::Minus) {
                    self.advance();
                    true
                } else {
                    false
                };
                match self.cur_kind().clone() {
                    TokenKind::IntLit(n) => {
                        let sp = self.advance().span;
                        let val = if neg { -n } else { n };
                        if matches!(self.cur_kind(), TokenKind::DotDot | TokenKind::DotDotEq) {
                            let inclusive = matches!(self.advance().kind, TokenKind::DotDotEq);
                            let end = self.parse_shift_expr()?;
                            return Ok(Pattern::Range {
                                start: Box::new(Expr {
                                    kind: ExprKind::Int(val),
                                    span: start_sp.merge(sp),
                                }),
                                end: Box::new(end),
                                inclusive,
                            });
                        }
                        Ok(Pattern::Int(val))
                    }
                    TokenKind::FloatLit(f) => {
                        self.advance();
                        Ok(Pattern::Float(if neg { -f } else { f }))
                    }
                    other => self.err(format!("expected number in pattern, found `{:?}`", other)),
                }
            }
            TokenKind::Ident(id) => {
                self.advance();
                Ok(Pattern::Ident(id))
            }
            TokenKind::TypeIdent(tn) => {
                self.advance();
                if matches!(self.cur_kind(), TokenKind::LParen) {
                    self.advance();
                    self.skip_seps();
                    let mut subs = Vec::new();
                    while !matches!(self.cur_kind(), TokenKind::RParen | TokenKind::Eof) {
                        subs.push(self.parse_pattern()?);
                        self.skip_seps();
                    }
                    self.expect(&TokenKind::RParen)?;
                    Ok(Pattern::VariantTuple {
                        type_name: tn,
                        subpatterns: subs,
                    })
                } else if matches!(self.cur_kind(), TokenKind::LBracket) {
                    let (fields, has_rest) = self.parse_struct_pattern_fields()?;
                    Ok(Pattern::Struct {
                        type_name: Some(tn),
                        fields,
                        has_rest,
                    })
                } else {
                    Ok(Pattern::VariantTuple {
                        type_name: tn,
                        subpatterns: Vec::new(),
                    })
                }
            }
            TokenKind::LBracket => {
                let (fields, has_rest) = self.parse_struct_pattern_fields()?;
                Ok(Pattern::Struct {
                    type_name: None,
                    fields,
                    has_rest,
                })
            }
            other => self.err(format!("expected pattern, found `{:?}`", other)),
        }
    }

    fn parse_struct_pattern_fields(&mut self) -> Result<(Vec<(String, Pattern)>, bool), ParseError> {
        self.expect(&TokenKind::LBracket)?;
        self.skip_seps();
        let mut fields = Vec::new();
        let mut has_rest = false;
        while !matches!(self.cur_kind(), TokenKind::RBracket | TokenKind::Eof) {
            if matches!(self.cur_kind(), TokenKind::DotDot) {
                self.advance();
                has_rest = true;
                self.skip_seps();
                continue;
            }
            let (fname, _) = self.expect_ident()?;
            let pat = if matches!(self.cur_kind(), TokenKind::Eq) {
                self.advance();
                self.parse_pattern()?
            } else {
                // Shorthand field punning `Click[x, y]` -> binds field `x` to pattern `Ident("x")`
                Pattern::Ident(fname.clone())
            };
            fields.push((fname, pat));
            self.skip_seps();
        }
        self.expect(&TokenKind::RBracket)?;
        Ok((fields, has_rest))
    }

    fn parse_loop_expr(&mut self) -> Result<Expr, ParseError> {
        let start = self.expect(&TokenKind::At)?.span;
        let head = if matches!(self.cur_kind(), TokenKind::LParen) {
            self.advance();
            self.skip_seps();
            // Check `ident <- expr` (`@ (item <- list)` or `@ (msg <- <~ ch)`)
            if let TokenKind::Ident(vname) = self.cur_kind().clone() {
                if self
                    .tokens
                    .get(self.pos + 1)
                    .is_some_and(|t| t.kind == TokenKind::LeftArrow)
                {
                    self.advance(); // ident
                    self.advance(); // <-
                    let iter = self.parse_expr()?;
                    self.skip_seps();
                    self.expect(&TokenKind::RParen)?;
                    LoopHead::ForEach {
                        var: vname,
                        iter: Box::new(iter),
                    }
                } else {
                    let cond = self.parse_expr()?;
                    self.skip_seps();
                    self.expect(&TokenKind::RParen)?;
                    LoopHead::While(Box::new(cond))
                }
            } else {
                let cond = self.parse_expr()?;
                self.skip_seps();
                self.expect(&TokenKind::RParen)?;
                LoopHead::While(Box::new(cond))
            }
        } else {
            LoopHead::Infinite
        };
        self.skip_newlines();
        let body = self.parse_brace_block()?;
        let span = start.merge(body.span);
        Ok(Expr {
            kind: ExprKind::Loop { head, body },
            span,
        })
    }

    fn parse_try_expr(&mut self) -> Result<Expr, ParseError> {
        let open = self.expect(&TokenKind::TryOpen)?;
        let body = self.parse_block_until(TokenKind::RBrace, open.span)?;
        let mut catches = Vec::new();
        let mut finally_block = None;

        loop {
            let (next_kind, idx) = self.peek_non_newline_kind();
            if matches!(next_kind, TokenKind::QuestBang) {
                let cstart = self.tokens[idx].span;
                self.pos = idx + 1;
                self.skip_newlines();
                if matches!(self.cur_kind(), TokenKind::LBracket) {
                    self.advance();
                    self.skip_seps();
                    let pat = self.parse_pattern()?;
                    self.skip_seps();
                    self.expect(&TokenKind::RBracket)?;
                    self.skip_newlines();
                    let cbody = self.parse_brace_block()?;
                    let sp = cstart.merge(cbody.span);
                    catches.push(CatchClause::Pattern {
                        pattern: pat,
                        body: cbody,
                        span: sp,
                    });
                } else if matches!(self.cur_kind(), TokenKind::LParen) {
                    self.advance();
                    let (name, _) = self.expect_ident()?;
                    self.expect(&TokenKind::RParen)?;
                    self.skip_newlines();
                    let cbody = self.parse_brace_block()?;
                    let sp = cstart.merge(cbody.span);
                    catches.push(CatchClause::Bind {
                        name,
                        body: cbody,
                        span: sp,
                    });
                } else {
                    let cbody = self.parse_brace_block()?;
                    let sp = cstart.merge(cbody.span);
                    catches.push(CatchClause::Bind {
                        name: "@".to_string(),
                        body: cbody,
                        span: sp,
                    });
                }
            } else {
                break;
            }
        }

        let (next_kind, idx) = self.peek_non_newline_kind();
        if matches!(next_kind, TokenKind::QuestGt) {
            self.pos = idx + 1;
            self.skip_newlines();
            finally_block = Some(self.parse_brace_block()?);
        }

        let span = open.span.merge(self.prev_span());
        Ok(Expr {
            kind: ExprKind::Try {
                body,
                catches,
                finally_block,
            },
            span,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_particle_simulation_from_lo_md() {
        let src = r#"
math = <"std/math">
c_puts = <"c" "puts" (str) [i32]>

Particle = [id = 0, x = 0.0, y = 0.0, active = 1b]

step = (p) {
  ? (p.active) {
    [..p, x = p.x + 1.5, y = math.sin(p.x)]
  } {
    p
  }
}

+main = {
  world = 1..=1000 ~> {
    Particle[id = @, x = @ * 0.1, y = 0.0, active = (@ % 2 == 0)]
  }

  total_x = world
    ~>> { step(@) }
    ~> ? { @.active }
    ~> (acc = 0.0) { acc + @.x }

  c_puts("Сумма координат активных частиц = {total_x}")
}
"#;
        let prog = Parser::parse_source(src).expect("particle simulation should parse");
        assert_eq!(prog.items.len(), 5);
        let formatted = format_program(&prog);
        assert!(formatted.contains("TypeDef Particle (Struct)"));
        assert!(formatted.contains("Pipe(Par)"));
        assert!(formatted.contains("StageFold(acc)"));
    }

    #[test]
    fn test_parse_order_service_v1_from_lo_md() {
        let src = r#"
http = <"std/http">
log  = <"std/log">

DbErr   = [code = 500, msg = ""]
Order   = [id = 0, item = "", qty = 1, coupon [str?] = ~]
Receipt = [order_id = 0, total = 0.0, status = ""]

base_prices = <{
  ["laptop" => 1200.0, "mouse" => 25.0, "keyboard" => 80.0]
}>

Order.calc_discount = {
  code = @.coupon?.trim()?.upper() ?? ""

  ? (code == "VIP50") {
    0.50
  } (code == "SALE20" & @.qty >= 2) {
    0.20
  } (@.qty >= 10) {
    0.10
  } {
    0.0
  }
}

process_order = (raw_order [Order?]) [Receipt?] {
  order = raw_order ?? ^ ~
  unit_price = base_prices[order.item] ?? ^ ~

  tx = db.begin_transaction()
  ~{ tx.close() }
  ?!{ tx.rollback() }

  !{
    (order.qty <= 0) ?^ !^ DbErr[code = 400, msg = "Некорректное количество"]

    discount = order.calc_discount()
    final_sum = (unit_price * order.qty) * (1.0 - discount)

    tx.save(order.id, final_sum)?
    tx.commit()?

    Receipt[order_id = order.id, total = final_sum, status = "OK"]
  } ?! [DbErr[code = 400, msg = m]] {
    log.warn("Ошибка валидации заказа #{order.id}: {m}")
    Receipt[order_id = order.id, total = 0.0, status = "INVALID"]
  } ?! (err) {
    log.error("Критический сбой БД: {err}")
    ~
  } ?> {
    log.info("Завершена обработка заказа #{order.id}")
  }
}

+main = {
  incoming [Order?..] = [
    Order[id = 1, item = "laptop", qty = 1, coupon = "vip50"]
    ~
    Order[id = 2, item = "mouse", qty = 3, coupon = ~]
    Order[id = 3, item = "unknown", qty = 1]
  ]

  incoming
    ~>> { process_order(@) }
    ?~> { log.print("Чек #{@.order_id}: сумма {@.total} ({@.status})") }
}
"#;
        let prog = Parser::parse_source(src).expect("order service v1.0 should parse");
        assert_eq!(prog.items.len(), 9);
        let formatted = format_program(&prog);
        assert!(formatted.contains("MethodDef Order.calc_discount"));
        assert!(formatted.contains("Comptime"));
        assert!(formatted.contains("Defer"));
        assert!(formatted.contains("ErrDefer"));
        assert!(formatted.contains("Try"));
        assert!(formatted.contains("Finally"));
        assert!(formatted.contains("Pipe(Safe)"));
    }

    #[test]
    fn test_parse_generics_interfaces_channels_patterns_and_generators() {
        let src = r#"
State = [
  Idle
  Run(f64)
  Attack[target = i64, dmg = i64]
]

Serializable = [
  to_bytes = () [u8..]
]

Cache = <K, V> [
  store [K => V]
  max_size = 100
]

Vec2 = [x = 0.0, y = 0.0]
Vec2.scale! = (factor [f64]) {
  @.x *<- factor
  @.y *<- factor
}

save_all = <T [Serializable]> (items [T..]) {
  items ~> { write_disk(@.to_bytes()) }
}

fibonacci = (limit) {
  [a, b] ~= [0, 1]
  @ (a < limit) {
    ^~ a
    [a, b] <- [b, a + b]
  }
}

demo_async = {
  buffer = [0 [u8] .. 1024]
  tags = [% "rust", "lua", "lo"]
  ch = [~i64]()
  task = ~>{
    1..=5 ~> { ch <~ (@ * 10) }
    ch.close()
  }
  @ (msg <- <~ ch) {
    ? [msg] {
      10 | 20 { ~@ }
      30..=50 (msg > 30) { !@ }
      _ { print(msg) }
    }
  }
  res = #search {
    [[1, 2], [3, -1]] ~> (row) {
      row ~> (cell) {
        ? (cell == -1) { ^#search 0 }
      }
    }
    100
  }
  <~ task
}
"#;
        let prog = Parser::parse_source(src).expect("generics, channels, match, generator should parse");
        let formatted = format_program(&prog);
        assert!(formatted.contains("TypeDef State (Enum)"));
        assert!(formatted.contains("TypeDef Serializable (Interface)"));
        assert!(formatted.contains("TypeDef Cache<K, V> (Struct)"));
        assert!(formatted.contains("MethodDef Vec2.scale!"));
        assert!(formatted.contains("RepeatArray [u8]"));
        assert!(formatted.contains("SetLit"));
        assert!(formatted.contains("AsyncSpawn"));
        assert!(formatted.contains("Yield"));
        assert!(formatted.contains("Labeled(#search)"));
    }
}
