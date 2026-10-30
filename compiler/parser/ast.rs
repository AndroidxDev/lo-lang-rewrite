use crate::lexer::Span;
use std::fmt::Write;

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub items: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeExpr {
    Unit,
    Named {
        name: String,
        generics: Vec<TypeExpr>,
    },
    Nullable(Box<TypeExpr>),
    Array(Box<TypeExpr>),
    Channel(Box<TypeExpr>),
    Set(Box<TypeExpr>),
    Map(Box<TypeExpr>, Box<TypeExpr>),
    Func {
        params: Vec<TypeExpr>,
        ret: Box<TypeExpr>,
    },
    Tuple(Vec<TypeExpr>),
}

impl TypeExpr {
    pub fn format_short(&self) -> String {
        match self {
            TypeExpr::Unit => "[]".to_string(),
            TypeExpr::Named { name, generics } => {
                if generics.is_empty() {
                    name.clone()
                } else {
                    let args: Vec<String> = generics.iter().map(|g| g.format_short()).collect();
                    format!("{}<{}>", name, args.join(", "))
                }
            }
            TypeExpr::Nullable(inner) => format!("{}?", inner.format_short()),
            TypeExpr::Array(inner) => format!("{}..", inner.format_short()),
            TypeExpr::Channel(inner) => format!("~{}", inner.format_short()),
            TypeExpr::Set(inner) => format!("%{}", inner.format_short()),
            TypeExpr::Map(k, v) => format!("{} => {}", k.format_short(), v.format_short()),
            TypeExpr::Func { params, ret } => {
                let ps: Vec<String> = params.iter().map(|p| p.format_short()).collect();
                format!("({}) [{}]", ps.join(", "), ret.format_short())
            }
            TypeExpr::Tuple(elems) => {
                let es: Vec<String> = elems.iter().map(|e| e.format_short()).collect();
                format!("[{}]", es.join(", "))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenericParam {
    pub name: String,
    pub bound: Option<TypeExpr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StructFieldDef {
    pub name: String,
    pub ty: Option<TypeExpr>,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VariantPayload {
    Unit,
    Tuple(Vec<TypeExpr>),
    Struct(Vec<StructFieldDef>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct VariantDef {
    pub name: String,
    pub payload: VariantPayload,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeDefKind {
    Struct(Vec<StructFieldDef>),
    Enum(Vec<VariantDef>),
    Interface(Vec<(String, TypeExpr)>),
    Alias(TypeExpr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutOp {
    Assign, // <-
    Add,    // +<-
    Sub,    // -<-
    Mul,    // *<-
    Div,    // /<-
}

#[derive(Debug, Clone, PartialEq)]
pub enum BindTarget {
    Single {
        name: String,
        ty: Option<TypeExpr>,
    },
    Destruct(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum MutTarget {
    LValue(Expr),
    Destruct(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FlowKind {
    Return(Option<Box<Expr>>),               // ^ [expr]
    LocalReturn(Option<Box<Expr>>),          // ^@ [expr]
    LabeledReturn(String, Option<Box<Expr>>), // ^#label [expr]
    Yield(Box<Expr>),                        // ^~ expr
    Throw(Box<Expr>),                        // !^ expr
    Break(Option<Box<Expr>>),                // !@ [expr] or legacy !
    LabeledBreak(String, Option<Box<Expr>>), // !#label [expr]
    Continue,                                // ~@
    LabeledContinue(String),                 // ~#label
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    TypeDef {
        is_pub: bool,
        name: String,
        generics: Vec<GenericParam>,
        def: TypeDefKind,
    },
    MethodDef {
        is_pub: bool,
        type_name: String,
        method_name: String,
        is_mutating: bool,
        generics: Vec<GenericParam>,
        func: FuncDef,
    },
    Bind {
        is_pub: bool,
        is_mut: bool,
        target: BindTarget,
        value: Expr,
    },
    Mutate {
        target: MutTarget,
        op: MutOp,
        value: Expr,
    },
    Defer {
        is_errdefer: bool,
        body: Block,
    },
    Flow(FlowKind),
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: Option<TypeExpr>,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuncDef {
    pub generics: Vec<GenericParam>,
    pub params: Vec<Param>,
    pub ret_ty: Option<TypeExpr>,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InterpSegment {
    Literal(String),
    Expr(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,    // -
    Not,    // !
    BitNot, // .~
    Await,  // <~
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    BitShl,
    BitShr,
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    And,
    Or,
    ChanSend, // <~
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeKind {
    Seq,  // ~>
    Par,  // ~>>
    Safe, // ?~>
}

#[derive(Debug, Clone, PartialEq)]
pub enum PipeStage {
    Map {
        param: Option<String>,
        body: Block,
    },
    Filter {
        body: Block,
    },
    Fold {
        acc_name: String,
        init: Box<Expr>,
        body: Block,
    },
    Expr(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    Wildcard, // _
    Null,     // ~
    Bool(bool),
    Int(i64),
    Float(f64),
    Char(char),
    Str(String),
    Ident(String),
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
        inclusive: bool,
    },
    VariantTuple {
        type_name: String,
        subpatterns: Vec<Pattern>,
    },
    Struct {
        type_name: Option<String>,
        fields: Vec<(String, Pattern)>,
        has_rest: bool,
    },
    Or(Vec<Pattern>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CatchClause {
    Pattern {
        pattern: Pattern,
        body: Block,
        span: Span,
    },
    Bind {
        name: String,
        body: Block,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoopHead {
    Infinite,
    While(Box<Expr>),
    ForEach {
        var: String,
        iter: Box<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElvisRhs {
    Expr(Box<Expr>),
    Flow(Box<FlowKind>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Unit,
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Char(char),
    Str(String),
    InterpString(Vec<InterpSegment>),

    Ident(String),
    TypeRef {
        name: String,
        generics: Vec<TypeExpr>,
    },
    TypeVal(TypeExpr), // e.g., [~i64] before ()
    CurrentItem,       // @
    SelfFn,            // $

    Array(Vec<Expr>),
    RepeatArray {
        value: Box<Expr>,
        elem_ty: Option<TypeExpr>,
        count: Box<Expr>,
    },
    StructLit {
        type_name: Option<String>,
        generics: Vec<TypeExpr>,
        spread: Option<Box<Expr>>,
        fields: Vec<(String, Expr)>,
    },
    MapLit(Vec<(Expr, Expr)>),
    SetLit(Vec<Expr>),

    Range {
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        inclusive: bool,
    },

    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },

    Field {
        expr: Box<Expr>,
        field: String,
        safe: bool,
    },
    Index {
        expr: Box<Expr>,
        index: Box<Expr>,
        safe: bool,
    },
    Slice {
        expr: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        inclusive: bool,
    },
    Call {
        callee: Box<Expr>,
        generics: Vec<TypeExpr>,
        args: Vec<Expr>,
        safe: bool,
    },
    Cast {
        expr: Box<Expr>,
        ty: TypeExpr,
        safe: bool,
    },
    NonNullAssert(Box<Expr>), // !!
    TryPropagate(Box<Expr>),  // ?

    Elvis {
        lhs: Box<Expr>,
        rhs: ElvisRhs,
    },
    ErrorFallback {
        lhs: Box<Expr>,
        body: Block,
    },
    GuardReturn {
        cond: Box<Expr>,
        ret: ElvisRhs,
    },
    Pipe {
        lhs: Box<Expr>,
        kind: PipeKind,
        stage: PipeStage,
    },

    Func(Box<FuncDef>),
    If {
        branches: Vec<(Expr, Block)>,
        else_block: Option<Block>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    Try {
        body: Block,
        catches: Vec<CatchClause>,
        finally_block: Option<Block>,
    },
    Loop {
        head: LoopHead,
        body: Block,
    },
    Labeled {
        label: String,
        expr: Box<Expr>, // Loop or BlockExpr
    },
    BlockExpr(Block),
    AsyncSpawn(Block),
    Comptime(Block),
    Import(String),
    ExternC {
        symbol: String,
        params: Vec<TypeExpr>,
        ret: TypeExpr,
    },
}

/// Deterministic canonical AST formatter for golden snapshot tests (`tests/parser_golden/`).
pub fn format_program(prog: &Program) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Program");
    for item in &prog.items {
        format_stmt(item, 1, &mut out);
    }
    out
}

fn indent_str(level: usize) -> String {
    "  ".repeat(level)
}

fn format_stmt(stmt: &Stmt, indent: usize, out: &mut String) {
    let pad = indent_str(indent);
    match &stmt.kind {
        StmtKind::TypeDef {
            is_pub,
            name,
            generics,
            def,
        } => {
            let vis = if *is_pub { "+" } else { "" };
            let gens = if generics.is_empty() {
                String::new()
            } else {
                let gs: Vec<String> = generics
                    .iter()
                    .map(|g| match &g.bound {
                        Some(b) => format!("{} [{}]", g.name, b.format_short()),
                        None => g.name.clone(),
                    })
                    .collect();
                format!("<{}>", gs.join(", "))
            };
            match def {
                TypeDefKind::Struct(fields) => {
                    let _ = writeln!(out, "{}TypeDef {}{}{} (Struct)", pad, vis, name, gens);
                    for f in fields {
                        let ty_s = f
                            .ty
                            .as_ref()
                            .map(|t| format!(" [{}]", t.format_short()))
                            .unwrap_or_default();
                        let _ = writeln!(out, "{}  Field {}{}", pad, f.name, ty_s);
                        if let Some(d) = &f.default {
                            format_expr(d, indent + 2, out);
                        }
                    }
                }
                TypeDefKind::Enum(variants) => {
                    let _ = writeln!(out, "{}TypeDef {}{}{} (Enum)", pad, vis, name, gens);
                    for v in variants {
                        match &v.payload {
                            VariantPayload::Unit => {
                                let _ = writeln!(out, "{}  Variant {}", pad, v.name);
                            }
                            VariantPayload::Tuple(ts) => {
                                let t_str: Vec<String> =
                                    ts.iter().map(|t| t.format_short()).collect();
                                let _ = writeln!(
                                    out,
                                    "{}  Variant {}({})",
                                    pad,
                                    v.name,
                                    t_str.join(", ")
                                );
                            }
                            VariantPayload::Struct(fs) => {
                                let _ = writeln!(out, "{}  Variant {} [Struct]", pad, v.name);
                                for f in fs {
                                    let ty_s = f
                                        .ty
                                        .as_ref()
                                        .map(|t| format!(" [{}]", t.format_short()))
                                        .unwrap_or_default();
                                    let _ = writeln!(out, "{}    Field {}{}", pad, f.name, ty_s);
                                }
                            }
                        }
                    }
                }
                TypeDefKind::Interface(methods) => {
                    let _ = writeln!(out, "{}TypeDef {}{}{} (Interface)", pad, vis, name, gens);
                    for (m, ty) in methods {
                        let _ = writeln!(out, "{}  Sig {}: {}", pad, m, ty.format_short());
                    }
                }
                TypeDefKind::Alias(ty) => {
                    let _ = writeln!(
                        out,
                        "{}TypeDef {}{}{} = {}",
                        pad,
                        vis,
                        name,
                        gens,
                        ty.format_short()
                    );
                }
            }
        }
        StmtKind::MethodDef {
            is_pub,
            type_name,
            method_name,
            is_mutating,
            func,
            ..
        } => {
            let vis = if *is_pub { "+" } else { "" };
            let bang = if *is_mutating { "!" } else { "" };
            let _ = writeln!(
                out,
                "{}MethodDef {}{}.{}{}",
                pad, vis, type_name, method_name, bang
            );
            format_func(func, indent + 1, out);
        }
        StmtKind::Bind {
            is_pub,
            is_mut,
            target,
            value,
        } => {
            let vis = if *is_pub { "+" } else { "" };
            let op = if *is_mut { "~=" } else { "=" };
            match target {
                BindTarget::Single { name, ty } => {
                    let ty_s = ty
                        .as_ref()
                        .map(|t| format!(" [{}]", t.format_short()))
                        .unwrap_or_default();
                    let _ = writeln!(out, "{}Bind {}{}{} {}", pad, vis, name, ty_s, op);
                }
                BindTarget::Destruct(names) => {
                    let _ = writeln!(out, "{}Bind [{}] {}", pad, names.join(", "), op);
                }
            }
            format_expr(value, indent + 1, out);
        }
        StmtKind::Mutate { target, op, value } => {
            let _ = writeln!(out, "{}Mutate {:?}", pad, op);
            match target {
                MutTarget::LValue(lv) => format_expr(lv, indent + 1, out),
                MutTarget::Destruct(names) => {
                    let _ = writeln!(out, "{}  Destruct [{}]", pad, names.join(", "));
                }
            }
            format_expr(value, indent + 1, out);
        }
        StmtKind::Defer { is_errdefer, body } => {
            let tag = if *is_errdefer { "ErrDefer" } else { "Defer" };
            let _ = writeln!(out, "{}{}", pad, tag);
            format_block(body, indent + 1, out);
        }
        StmtKind::Flow(flow) => {
            format_flow(flow, indent, out);
        }
        StmtKind::Expr(expr) => {
            format_expr(expr, indent, out);
        }
    }
}

fn format_flow(flow: &FlowKind, indent: usize, out: &mut String) {
    let pad = indent_str(indent);
    match flow {
        FlowKind::Return(e) => {
            let _ = writeln!(out, "{}Return", pad);
            if let Some(ex) = e {
                format_expr(ex, indent + 1, out);
            }
        }
        FlowKind::LocalReturn(e) => {
            let _ = writeln!(out, "{}LocalReturn", pad);
            if let Some(ex) = e {
                format_expr(ex, indent + 1, out);
            }
        }
        FlowKind::LabeledReturn(lbl, e) => {
            let _ = writeln!(out, "{}LabeledReturn #{}", pad, lbl);
            if let Some(ex) = e {
                format_expr(ex, indent + 1, out);
            }
        }
        FlowKind::Yield(ex) => {
            let _ = writeln!(out, "{}Yield", pad);
            format_expr(ex, indent + 1, out);
        }
        FlowKind::Throw(ex) => {
            let _ = writeln!(out, "{}Throw", pad);
            format_expr(ex, indent + 1, out);
        }
        FlowKind::Break(e) => {
            let _ = writeln!(out, "{}Break", pad);
            if let Some(ex) = e {
                format_expr(ex, indent + 1, out);
            }
        }
        FlowKind::LabeledBreak(lbl, e) => {
            let _ = writeln!(out, "{}LabeledBreak #{}", pad, lbl);
            if let Some(ex) = e {
                format_expr(ex, indent + 1, out);
            }
        }
        FlowKind::Continue => {
            let _ = writeln!(out, "{}Continue", pad);
        }
        FlowKind::LabeledContinue(lbl) => {
            let _ = writeln!(out, "{}LabeledContinue #{}", pad, lbl);
        }
    }
}

fn format_block(block: &Block, indent: usize, out: &mut String) {
    let pad = indent_str(indent);
    let _ = writeln!(out, "{}Block", pad);
    for s in &block.stmts {
        format_stmt(s, indent + 1, out);
    }
}

fn format_func(func: &FuncDef, indent: usize, out: &mut String) {
    let pad = indent_str(indent);
    let ps: Vec<String> = func
        .params
        .iter()
        .map(|p| match &p.ty {
            Some(t) => format!("{} [{}]", p.name, t.format_short()),
            None => p.name.clone(),
        })
        .collect();
    let ret = func
        .ret_ty
        .as_ref()
        .map(|r| format!(" -> [{}]", r.format_short()))
        .unwrap_or_default();
    let _ = writeln!(out, "{}Func({}){}", pad, ps.join(", "), ret);
    format_block(&func.body, indent + 1, out);
}

fn format_expr(expr: &Expr, indent: usize, out: &mut String) {
    let pad = indent_str(indent);
    match &expr.kind {
        ExprKind::Unit => {
            let _ = writeln!(out, "{}Unit", pad);
        }
        ExprKind::Null => {
            let _ = writeln!(out, "{}Null(~)", pad);
        }
        ExprKind::Bool(b) => {
            let _ = writeln!(out, "{}Bool({})", pad, if *b { "1b" } else { "0b" });
        }
        ExprKind::Int(n) => {
            let _ = writeln!(out, "{}Int({})", pad, n);
        }
        ExprKind::Float(x) => {
            let _ = writeln!(out, "{}Float({:?})", pad, x);
        }
        ExprKind::Char(c) => {
            let _ = writeln!(out, "{}Char({:?})", pad, c);
        }
        ExprKind::Str(s) => {
            let _ = writeln!(out, "{}Str({:?})", pad, s);
        }
        ExprKind::InterpString(segs) => {
            let _ = writeln!(out, "{}InterpString", pad);
            for seg in segs {
                match seg {
                    InterpSegment::Literal(s) => {
                        let _ = writeln!(out, "{}  Lit({:?})", pad, s);
                    }
                    InterpSegment::Expr(e) => format_expr(e, indent + 1, out),
                }
            }
        }
        ExprKind::Ident(id) => {
            let _ = writeln!(out, "{}Ident({})", pad, id);
        }
        ExprKind::TypeRef { name, generics } => {
            let t = TypeExpr::Named {
                name: name.clone(),
                generics: generics.clone(),
            };
            let _ = writeln!(out, "{}TypeRef({})", pad, t.format_short());
        }
        ExprKind::TypeVal(ty) => {
            let _ = writeln!(out, "{}TypeVal([{}])", pad, ty.format_short());
        }
        ExprKind::CurrentItem => {
            let _ = writeln!(out, "{}CurrentItem(@)", pad);
        }
        ExprKind::SelfFn => {
            let _ = writeln!(out, "{}SelfFn($)", pad);
        }
        ExprKind::Array(elems) => {
            let _ = writeln!(out, "{}Array", pad);
            for e in elems {
                format_expr(e, indent + 1, out);
            }
        }
        ExprKind::RepeatArray {
            value,
            elem_ty,
            count,
        } => {
            let ts = elem_ty
                .as_ref()
                .map(|t| format!(" [{}]", t.format_short()))
                .unwrap_or_default();
            let _ = writeln!(out, "{}RepeatArray{}", pad, ts);
            format_expr(value, indent + 1, out);
            format_expr(count, indent + 1, out);
        }
        ExprKind::StructLit {
            type_name,
            spread,
            fields,
            ..
        } => {
            let tn = type_name.as_deref().unwrap_or("<anon>");
            let _ = writeln!(out, "{}StructLit({})", pad, tn);
            if let Some(sp) = spread {
                let _ = writeln!(out, "{}  Spread", pad);
                format_expr(sp, indent + 2, out);
            }
            for (k, v) in fields {
                let _ = writeln!(out, "{}  Field {}", pad, k);
                format_expr(v, indent + 2, out);
            }
        }
        ExprKind::MapLit(entries) => {
            let _ = writeln!(out, "{}MapLit", pad);
            for (k, v) in entries {
                format_expr(k, indent + 1, out);
                format_expr(v, indent + 1, out);
            }
        }
        ExprKind::SetLit(elems) => {
            let _ = writeln!(out, "{}SetLit", pad);
            for e in elems {
                format_expr(e, indent + 1, out);
            }
        }
        ExprKind::Range {
            start,
            end,
            inclusive,
        } => {
            let op = if *inclusive { "..=" } else { ".." };
            let _ = writeln!(out, "{}Range({})", pad, op);
            if let Some(s) = start {
                format_expr(s, indent + 1, out);
            }
            if let Some(e) = end {
                format_expr(e, indent + 1, out);
            }
        }
        ExprKind::Unary { op, expr } => {
            let _ = writeln!(out, "{}Unary({:?})", pad, op);
            format_expr(expr, indent + 1, out);
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let _ = writeln!(out, "{}Binary({:?})", pad, op);
            format_expr(lhs, indent + 1, out);
            format_expr(rhs, indent + 1, out);
        }
        ExprKind::Field { expr, field, safe } => {
            let op = if *safe { "?." } else { "." };
            let _ = writeln!(out, "{}Field({}{})", pad, op, field);
            format_expr(expr, indent + 1, out);
        }
        ExprKind::Index { expr, index, safe } => {
            let op = if *safe { "?[]" } else { "[]" };
            let _ = writeln!(out, "{}Index({})", pad, op);
            format_expr(expr, indent + 1, out);
            format_expr(index, indent + 1, out);
        }
        ExprKind::Slice {
            expr,
            start,
            end,
            inclusive,
        } => {
            let op = if *inclusive { "..=" } else { ".." };
            let _ = writeln!(out, "{}Slice({})", pad, op);
            format_expr(expr, indent + 1, out);
            if let Some(s) = start {
                format_expr(s, indent + 1, out);
            }
            if let Some(e) = end {
                format_expr(e, indent + 1, out);
            }
        }
        ExprKind::Call {
            callee, args, safe, ..
        } => {
            let op = if *safe { "?()" } else { "()" };
            let _ = writeln!(out, "{}Call{}", pad, op);
            format_expr(callee, indent + 1, out);
            for a in args {
                format_expr(a, indent + 1, out);
            }
        }
        ExprKind::Cast { expr, ty, safe } => {
            let op = if *safe { "?[" } else { "[" };
            let _ = writeln!(out, "{}Cast({}{}])", pad, op, ty.format_short());
            format_expr(expr, indent + 1, out);
        }
        ExprKind::NonNullAssert(inner) => {
            let _ = writeln!(out, "{}NonNullAssert(!!)", pad);
            format_expr(inner, indent + 1, out);
        }
        ExprKind::TryPropagate(inner) => {
            let _ = writeln!(out, "{}TryPropagate(?)", pad);
            format_expr(inner, indent + 1, out);
        }
        ExprKind::Elvis { lhs, rhs } => {
            let _ = writeln!(out, "{}Elvis(??)", pad);
            format_expr(lhs, indent + 1, out);
            match rhs {
                ElvisRhs::Expr(e) => format_expr(e, indent + 1, out),
                ElvisRhs::Flow(f) => format_flow(f, indent + 1, out),
            }
        }
        ExprKind::ErrorFallback { lhs, body } => {
            let _ = writeln!(out, "{}ErrorFallback(?!)", pad);
            format_expr(lhs, indent + 1, out);
            format_block(body, indent + 1, out);
        }
        ExprKind::GuardReturn { cond, ret } => {
            let _ = writeln!(out, "{}GuardReturn(?^)", pad);
            format_expr(cond, indent + 1, out);
            match ret {
                ElvisRhs::Expr(e) => format_expr(e, indent + 1, out),
                ElvisRhs::Flow(f) => format_flow(f, indent + 1, out),
            }
        }
        ExprKind::Pipe { lhs, kind, stage } => {
            let _ = writeln!(out, "{}Pipe({:?})", pad, kind);
            format_expr(lhs, indent + 1, out);
            match stage {
                PipeStage::Map { param, body } => {
                    let p = param.as_deref().unwrap_or("@");
                    let _ = writeln!(out, "{}  StageMap({})", pad, p);
                    format_block(body, indent + 2, out);
                }
                PipeStage::Filter { body } => {
                    let _ = writeln!(out, "{}  StageFilter", pad);
                    format_block(body, indent + 2, out);
                }
                PipeStage::Fold {
                    acc_name,
                    init,
                    body,
                } => {
                    let _ = writeln!(out, "{}  StageFold({})", pad, acc_name);
                    format_expr(init, indent + 2, out);
                    format_block(body, indent + 2, out);
                }
                PipeStage::Expr(e) => format_expr(e, indent + 1, out),
            }
        }
        ExprKind::Func(f) => format_func(f, indent, out),
        ExprKind::If {
            branches,
            else_block,
        } => {
            let _ = writeln!(out, "{}If", pad);
            for (cond, blk) in branches {
                format_expr(cond, indent + 1, out);
                format_block(blk, indent + 1, out);
            }
            if let Some(eb) = else_block {
                let _ = writeln!(out, "{}  Else", pad);
                format_block(eb, indent + 2, out);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            let _ = writeln!(out, "{}Match", pad);
            format_expr(scrutinee, indent + 1, out);
            for arm in arms {
                let _ = writeln!(out, "{}  Arm {:?}", pad, arm.pattern);
                if let Some(g) = &arm.guard {
                    let _ = writeln!(out, "{}    Guard", pad);
                    format_expr(g, indent + 3, out);
                }
                format_block(&arm.body, indent + 2, out);
            }
        }
        ExprKind::Try {
            body,
            catches,
            finally_block,
        } => {
            let _ = writeln!(out, "{}Try", pad);
            format_block(body, indent + 1, out);
            for c in catches {
                match c {
                    CatchClause::Pattern { pattern, body, .. } => {
                        let _ = writeln!(out, "{}  CatchPattern {:?}", pad, pattern);
                        format_block(body, indent + 2, out);
                    }
                    CatchClause::Bind { name, body, .. } => {
                        let _ = writeln!(out, "{}  CatchBind({})", pad, name);
                        format_block(body, indent + 2, out);
                    }
                }
            }
            if let Some(fin) = finally_block {
                let _ = writeln!(out, "{}  Finally", pad);
                format_block(fin, indent + 2, out);
            }
        }
        ExprKind::Loop { head, body } => {
            let _ = writeln!(out, "{}Loop", pad);
            match head {
                LoopHead::Infinite => {}
                LoopHead::While(cond) => format_expr(cond, indent + 1, out),
                LoopHead::ForEach { var, iter } => {
                    let _ = writeln!(out, "{}  ForEach({})", pad, var);
                    format_expr(iter, indent + 2, out);
                }
            }
            format_block(body, indent + 1, out);
        }
        ExprKind::Labeled { label, expr } => {
            let _ = writeln!(out, "{}Labeled(#{})", pad, label);
            format_expr(expr, indent + 1, out);
        }
        ExprKind::BlockExpr(blk) => format_block(blk, indent, out),
        ExprKind::AsyncSpawn(blk) => {
            let _ = writeln!(out, "{}AsyncSpawn", pad);
            format_block(blk, indent + 1, out);
        }
        ExprKind::Comptime(blk) => {
            let _ = writeln!(out, "{}Comptime", pad);
            format_block(blk, indent + 1, out);
        }
        ExprKind::Import(path) => {
            let _ = writeln!(out, "{}Import({:?})", pad, path);
        }
        ExprKind::ExternC {
            symbol,
            params,
            ret,
        } => {
            let ps: Vec<String> = params.iter().map(|p| p.format_short()).collect();
            let _ = writeln!(
                out,
                "{}ExternC({:?}, ({}) -> [{}])",
                pad,
                symbol,
                ps.join(", "),
                ret.format_short()
            );
        }
    }
}
