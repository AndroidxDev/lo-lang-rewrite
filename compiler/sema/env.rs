use crate::lexer::Span;
use crate::parser::*;
use crate::sema::types::Type;
use std::collections::HashMap;
use std::fmt;

pub type SpanKey = (usize, usize);

pub fn span_key(sp: Span) -> SpanKey {
    (sp.start, sp.end)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemaError {
    pub message: String,
    pub span: Span,
}

impl fmt::Display for SemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TypeError at {}:{}: {}",
            self.span.line, self.span.col, self.message
        )
    }
}

impl std::error::Error for SemaError {}

#[derive(Debug, Clone)]
pub struct CheckedField {
    pub name: String,
    pub ty: Type,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct CheckedStruct {
    pub name: String,
    pub fields: Vec<CheckedField>,
    pub type_id: u32,
}

#[derive(Debug, Clone)]
pub enum CheckedVariantPayload {
    Unit,
    Tuple(Vec<Type>),
    Struct(Vec<CheckedField>),
}

#[derive(Debug, Clone)]
pub struct CheckedVariant {
    pub name: String,
    pub tag: u32,
    pub payload: CheckedVariantPayload,
}

#[derive(Debug, Clone)]
pub struct CheckedEnum {
    pub name: String,
    pub variants: Vec<CheckedVariant>,
    pub type_id: u32,
}

#[derive(Debug, Clone)]
pub struct GenericStructTemplate {
    pub name: String,
    pub generics: Vec<GenericParam>,
    pub fields: Vec<StructFieldDef>,
}

#[derive(Debug, Clone)]
pub struct InterfaceSchema {
    pub name: String,
    pub methods: Vec<(String, Vec<Type>, Type)>,
}

#[derive(Debug, Clone)]
pub struct ExternCSig {
    pub bind_name: String,
    pub c_symbol: String,
    pub params: Vec<Type>,
    pub ret: Type,
}

#[derive(Debug, Clone)]
pub struct FuncTemplate {
    pub key_name: String,
    pub receiver_type: Option<String>,
    pub is_mutating_method: bool,
    pub func: FuncDef,
    pub module_prefix: String,
}

#[derive(Debug, Clone, Default)]
pub struct ModuleScope {
    pub aliases: HashMap<String, String>,
    pub externs: HashMap<String, ExternCSig>,
    pub globals: HashMap<String, Type>,
}

#[derive(Debug, Clone)]
pub struct MonoFunc {
    pub c_name: String,
    pub module_prefix: String,
    pub receiver: Option<(Type, bool)>, // (Type, is_mutating)
    pub params: Vec<(String, Type, Option<Expr>)>,
    pub ret_ty: Type,
    pub is_generator: bool,
    pub yield_ty: Option<Type>,
    pub body: Block,
    pub expr_types: HashMap<SpanKey, Type>,
    pub call_targets: HashMap<SpanKey, String>,
    pub struct_targets: HashMap<SpanKey, String>,
}

#[derive(Debug, Clone)]
pub struct GlobalConst {
    pub c_name: String,
    pub module_prefix: String,
    pub lo_name: String,
    pub ty: Type,
    pub value: Expr,
    pub expr_types: HashMap<SpanKey, Type>,
}

#[derive(Debug, Clone)]
pub struct ModuleExports {
    pub id: String,
    pub values: HashMap<String, ModuleSym>,
    pub types: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub enum ModuleSym {
    Const { c_name: String, ty: Type },
    ExternC(ExternCSig),
    FuncTemplateKey(String),
}

#[derive(Debug, Clone)]
pub struct CheckedProgram {
    pub struct_order: Vec<String>,
    pub structs: HashMap<String, CheckedStruct>,
    pub enums: HashMap<String, CheckedEnum>,
    pub externs: Vec<ExternCSig>,
    pub globals: Vec<GlobalConst>,
    pub funcs: Vec<MonoFunc>,
    pub main_c_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VarInfo {
    pub c_name: String,
    pub ty: Type,
    pub is_mut: bool,
    pub par_depth: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Narrowings {
    pub when_true: HashMap<String, Type>,
    pub when_false: HashMap<String, Type>,
}

/// Extract Kotlin-style Smart Cast nullability narrowings (`x != ~`, `x == ~`, `!c`, `c1 & c2`, `c1 | c2`).
pub fn extract_narrowings(cond: &Expr, lookup_ty: &dyn Fn(&str) -> Option<Type>) -> Narrowings {
    let mut out = Narrowings::default();
    match &cond.kind {
        ExprKind::Binary { op, lhs, rhs } => match op {
            BinOp::NotEq | BinOp::Eq => {
                let var_name = match (&lhs.kind, &rhs.kind) {
                    (ExprKind::Ident(id), ExprKind::Null) => Some(id.as_str()),
                    (ExprKind::Null, ExprKind::Ident(id)) => Some(id.as_str()),
                    _ => None,
                };
                if let Some(name) = var_name {
                    if let Some(ty) = lookup_ty(name) {
                        if let Some(non_null) = ty.unwrap_nullable() {
                            if *op == BinOp::NotEq {
                                out.when_true.insert(name.to_string(), non_null.clone());
                            } else {
                                out.when_false.insert(name.to_string(), non_null.clone());
                            }
                        }
                    }
                }
            }
            BinOp::And => {
                let left = extract_narrowings(lhs, lookup_ty);
                let right = extract_narrowings(rhs, lookup_ty);
                out.when_true.extend(left.when_true);
                out.when_true.extend(right.when_true);
            }
            BinOp::Or => {
                let left = extract_narrowings(lhs, lookup_ty);
                let right = extract_narrowings(rhs, lookup_ty);
                out.when_false.extend(left.when_false);
                out.when_false.extend(right.when_false);
            }
            _ => {}
        },
        ExprKind::Unary {
            op: UnaryOp::Not,
            expr: inner,
        } => {
            let sub = extract_narrowings(inner, lookup_ty);
            out.when_true = sub.when_false;
            out.when_false = sub.when_true;
        }
        _ => {}
    }
    out
}

pub fn sanitize_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if c == '!' {
            out.push_str("_mut");
        } else {
            out.push('_');
        }
    }
    out
}
