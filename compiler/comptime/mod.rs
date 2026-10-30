use crate::lexer::Span;
use crate::parser::*;
use std::collections::HashMap;
use std::fmt;

const MAX_COMPTIME_STEPS: usize = 500_000;

#[derive(Debug, Clone)]
pub struct ComptimeError {
    pub message: String,
    pub span: Span,
}

impl fmt::Display for ComptimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ComptimeError at {}:{}: {}",
            self.span.line, self.span.col, self.message
        )
    }
}

impl std::error::Error for ComptimeError {}

#[derive(Debug, Clone, PartialEq)]
pub enum ComptimeValue {
    Unit,
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Char(char),
    Str(String),
    Range {
        start: i64,
        end: i64,
        inclusive: bool,
    },
    Array(Vec<ComptimeValue>),
    Set(Vec<ComptimeValue>),
    Map(Vec<(ComptimeValue, ComptimeValue)>),
    Struct {
        type_name: Option<String>,
        fields: Vec<(String, ComptimeValue)>,
    },
    Func {
        params: Vec<Param>,
        body: Block,
    },
    BuiltinMod(String),
}

impl ComptimeValue {
    pub fn display_str(&self) -> String {
        match self {
            ComptimeValue::Unit => "[]".to_string(),
            ComptimeValue::Null => "~".to_string(),
            ComptimeValue::Bool(b) => if *b { "1b" } else { "0b" }.to_string(),
            ComptimeValue::Int(n) => n.to_string(),
            ComptimeValue::Float(x) => {
                if x.fract() == 0.0 {
                    format!("{:.1}", x)
                } else {
                    format!("{}", x)
                }
            }
            ComptimeValue::Char(c) => c.to_string(),
            ComptimeValue::Str(s) => s.clone(),
            ComptimeValue::Range {
                start,
                end,
                inclusive,
            } => {
                if *inclusive {
                    format!("{}..={}", start, end)
                } else {
                    format!("{}..{}", start, end)
                }
            }
            ComptimeValue::Array(items) => {
                let parts: Vec<String> = items.iter().map(|v| v.display_str()).collect();
                format!("[{}]", parts.join(", "))
            }
            ComptimeValue::Set(items) => {
                let parts: Vec<String> = items.iter().map(|v| v.display_str()).collect();
                format!("[% {}]", parts.join(", "))
            }
            ComptimeValue::Map(entries) => {
                let parts: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{} => {}", k.display_str(), v.display_str()))
                    .collect();
                format!("[{}]", parts.join(", "))
            }
            ComptimeValue::Struct { type_name, fields } => {
                let fs: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{} = {}", k, v.display_str()))
                    .collect();
                match type_name {
                    Some(tn) => format!("{}[{}]", tn, fs.join(", ")),
                    None => format!("[{}]", fs.join(", ")),
                }
            }
            ComptimeValue::Func { .. } => "<fn>".to_string(),
            ComptimeValue::BuiltinMod(m) => format!("<mod:{}>", m),
        }
    }

    pub fn to_ast_expr(&self, span: Span) -> Result<Expr, ComptimeError> {
        let kind = match self {
            ComptimeValue::Unit => ExprKind::Unit,
            ComptimeValue::Null => ExprKind::Null,
            ComptimeValue::Bool(b) => ExprKind::Bool(*b),
            ComptimeValue::Int(n) => ExprKind::Int(*n),
            ComptimeValue::Float(x) => ExprKind::Float(*x),
            ComptimeValue::Char(c) => ExprKind::Char(*c),
            ComptimeValue::Str(s) => ExprKind::Str(s.clone()),
            ComptimeValue::Range {
                start,
                end,
                inclusive,
            } => ExprKind::Range {
                start: Some(Box::new(Expr {
                    kind: ExprKind::Int(*start),
                    span,
                })),
                end: Some(Box::new(Expr {
                    kind: ExprKind::Int(*end),
                    span,
                })),
                inclusive: *inclusive,
            },
            ComptimeValue::Array(items) => {
                let mut exprs = Vec::with_capacity(items.len());
                for item in items {
                    exprs.push(item.to_ast_expr(span)?);
                }
                ExprKind::Array(exprs)
            }
            ComptimeValue::Set(items) => {
                let mut exprs = Vec::with_capacity(items.len());
                for item in items {
                    exprs.push(item.to_ast_expr(span)?);
                }
                ExprKind::SetLit(exprs)
            }
            ComptimeValue::Map(entries) => {
                let mut pairs = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    pairs.push((k.to_ast_expr(span)?, v.to_ast_expr(span)?));
                }
                ExprKind::MapLit(pairs)
            }
            ComptimeValue::Struct { type_name, fields } => {
                let mut fs = Vec::with_capacity(fields.len());
                for (k, v) in fields {
                    fs.push((k.clone(), v.to_ast_expr(span)?));
                }
                ExprKind::StructLit {
                    type_name: type_name.clone(),
                    generics: Vec::new(),
                    spread: None,
                    fields: fs,
                }
            }
            ComptimeValue::Func { .. } | ComptimeValue::BuiltinMod(_) => {
                return Err(ComptimeError {
                    message: "comptime block cannot evaluate to a runtime function or module handle"
                        .into(),
                    span,
                })
            }
        };
        Ok(Expr { kind, span })
    }
}

#[derive(Debug, Clone)]
enum Signal {
    Return(ComptimeValue),
    LocalReturn(ComptimeValue),
    LabeledReturn(String, ComptimeValue),
    Break(Option<ComptimeValue>),
    LabeledBreak(String, Option<ComptimeValue>),
    Continue,
    LabeledContinue(String),
}

#[derive(Debug, Clone)]
struct VarSlot {
    value: ComptimeValue,
    is_mut: bool,
}

pub struct ComptimeEvaluator {
    scopes: Vec<HashMap<String, VarSlot>>,
    struct_defs: HashMap<String, Vec<StructFieldDef>>,
    self_fn_stack: Vec<ComptimeValue>,
    current_item_stack: Vec<ComptimeValue>,
    steps: usize,
}

impl Default for ComptimeEvaluator {
    fn default() -> Self {
        Self::new()
    }
}

impl ComptimeEvaluator {
    pub fn new() -> Self {
        let mut root = HashMap::new();
        root.insert(
            "math".to_string(),
            VarSlot {
                value: ComptimeValue::BuiltinMod("math".into()),
                is_mut: false,
            },
        );
        root.insert(
            "str_mod".to_string(),
            VarSlot {
                value: ComptimeValue::BuiltinMod("str".into()),
                is_mut: false,
            },
        );
        Self {
            scopes: vec![root],
            struct_defs: HashMap::new(),
            self_fn_stack: Vec::new(),
            current_item_stack: Vec::new(),
            steps: 0,
        }
    }

    pub fn evaluate_program(prog: &mut Program) -> Result<(), ComptimeError> {
        let mut eval = Self::new();
        for stmt in &mut prog.items {
            eval.visit_top_stmt(stmt)?;
        }
        Ok(())
    }

    fn tick(&mut self, span: Span) -> Result<(), ComptimeError> {
        self.steps += 1;
        if self.steps > MAX_COMPTIME_STEPS {
            return Err(ComptimeError {
                message: format!(
                    "comptime evaluation exceeded {} step limit (possible infinite loop)",
                    MAX_COMPTIME_STEPS
                ),
                span,
            });
        }
        Ok(())
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define_var(&mut self, name: String, value: ComptimeValue, is_mut: bool) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, VarSlot { value, is_mut });
        }
    }

    fn lookup_var(&self, name: &str) -> Option<&ComptimeValue> {
        for scope in self.scopes.iter().rev() {
            if let Some(slot) = scope.get(name) {
                return Some(&slot.value);
            }
        }
        None
    }

    fn assign_var(
        &mut self,
        name: &str,
        new_val: ComptimeValue,
        span: Span,
    ) -> Result<(), ComptimeError> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                if !slot.is_mut {
                    return Err(ComptimeError {
                        message: format!("cannot mutate immutable binding `{}` in comptime", name),
                        span,
                    });
                }
                slot.value = new_val;
                return Ok(());
            }
        }
        Err(ComptimeError {
            message: format!("undefined variable `{}` in comptime mutation", name),
            span,
        })
    }

    fn visit_top_stmt(&mut self, stmt: &mut Stmt) -> Result<(), ComptimeError> {
        match &mut stmt.kind {
            StmtKind::TypeDef {
                name,
                def: TypeDefKind::Struct(fields),
                ..
            } => {
                self.struct_defs.insert(name.clone(), fields.clone());
            }
            StmtKind::Bind { target, value, .. } => {
                self.fold_comptime_in_expr(value)?;
                // Record top-level pure constants/imports/functions for subsequent comptime blocks
                if let BindTarget::Single { name, .. } = target {
                    match &value.kind {
                        ExprKind::Import(path) => {
                            if path == "std/math" {
                                self.define_var(
                                    name.clone(),
                                    ComptimeValue::BuiltinMod("math".into()),
                                    false,
                                );
                            } else if path == "std/str" {
                                self.define_var(
                                    name.clone(),
                                    ComptimeValue::BuiltinMod("str".into()),
                                    false,
                                );
                            }
                        }
                        ExprKind::Func(f) => {
                            self.define_var(
                                name.clone(),
                                ComptimeValue::Func {
                                    params: f.params.clone(),
                                    body: f.body.clone(),
                                },
                                false,
                            );
                        }
                        _ => {
                            if let Ok(Ok(val)) = self.eval_expr(value) {
                                self.define_var(name.clone(), val, false);
                            }
                        }
                    }
                }
            }
            StmtKind::MethodDef { func, .. } => {
                self.fold_comptime_in_block(&mut func.body)?;
            }
            StmtKind::Expr(e) => {
                self.fold_comptime_in_expr(e)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn fold_comptime_in_block(&mut self, block: &mut Block) -> Result<(), ComptimeError> {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                StmtKind::Bind { value, .. } | StmtKind::Mutate { value, .. } => {
                    self.fold_comptime_in_expr(value)?;
                }
                StmtKind::Defer { body, .. } => {
                    self.fold_comptime_in_block(body)?;
                }
                StmtKind::Flow(flow) => match flow {
                    FlowKind::Return(Some(e))
                    | FlowKind::LocalReturn(Some(e))
                    | FlowKind::LabeledReturn(_, Some(e))
                    | FlowKind::Yield(e)
                    | FlowKind::Throw(e)
                    | FlowKind::Break(Some(e))
                    | FlowKind::LabeledBreak(_, Some(e)) => {
                        self.fold_comptime_in_expr(e)?;
                    }
                    _ => {}
                },
                StmtKind::Expr(e) => {
                    self.fold_comptime_in_expr(e)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn fold_comptime_in_expr(&mut self, expr: &mut Expr) -> Result<(), ComptimeError> {
        if let ExprKind::Comptime(block) = &expr.kind {
            let block_clone = block.clone();
            let val = match self.eval_block(&block_clone)? {
                Ok(v) => v,
                Err(Signal::Return(v) | Signal::LocalReturn(v)) => v,
                Err(_) => {
                    return Err(ComptimeError {
                        message: "unhandled control flow escaping `<{ ... }>` comptime block".into(),
                        span: expr.span,
                    })
                }
            };
            *expr = val.to_ast_expr(expr.span)?;
            return Ok(());
        }

        match &mut expr.kind {
            ExprKind::InterpString(segs) => {
                for seg in segs {
                    if let InterpSegment::Expr(e) = seg {
                        self.fold_comptime_in_expr(e)?;
                    }
                }
            }
            ExprKind::Array(elems) | ExprKind::SetLit(elems) => {
                for e in elems {
                    self.fold_comptime_in_expr(e)?;
                }
            }
            ExprKind::RepeatArray { value, count, .. } => {
                self.fold_comptime_in_expr(value)?;
                self.fold_comptime_in_expr(count)?;
            }
            ExprKind::StructLit { spread, fields, .. } => {
                if let Some(sp) = spread {
                    self.fold_comptime_in_expr(sp)?;
                }
                for (_, v) in fields {
                    self.fold_comptime_in_expr(v)?;
                }
            }
            ExprKind::MapLit(entries) => {
                for (k, v) in entries {
                    self.fold_comptime_in_expr(k)?;
                    self.fold_comptime_in_expr(v)?;
                }
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.fold_comptime_in_expr(s)?;
                }
                if let Some(e) = end {
                    self.fold_comptime_in_expr(e)?;
                }
            }
            ExprKind::Unary { expr: inner, .. }
            | ExprKind::Field { expr: inner, .. }
            | ExprKind::Cast { expr: inner, .. }
            | ExprKind::NonNullAssert(inner)
            | ExprKind::TryPropagate(inner) => {
                self.fold_comptime_in_expr(inner)?;
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.fold_comptime_in_expr(lhs)?;
                self.fold_comptime_in_expr(rhs)?;
            }
            ExprKind::Index { expr: b, index, .. } => {
                self.fold_comptime_in_expr(b)?;
                self.fold_comptime_in_expr(index)?;
            }
            ExprKind::Slice {
                expr: b,
                start,
                end,
                ..
            } => {
                self.fold_comptime_in_expr(b)?;
                if let Some(s) = start {
                    self.fold_comptime_in_expr(s)?;
                }
                if let Some(e) = end {
                    self.fold_comptime_in_expr(e)?;
                }
            }
            ExprKind::Call { callee, args, .. } => {
                self.fold_comptime_in_expr(callee)?;
                for a in args {
                    self.fold_comptime_in_expr(a)?;
                }
            }
            ExprKind::Elvis { lhs, rhs } => {
                self.fold_comptime_in_expr(lhs)?;
                if let ElvisRhs::Expr(r) = rhs {
                    self.fold_comptime_in_expr(r)?;
                }
            }
            ExprKind::ErrorFallback { lhs, body } => {
                self.fold_comptime_in_expr(lhs)?;
                self.fold_comptime_in_block(body)?;
            }
            ExprKind::GuardReturn { cond, ret } => {
                self.fold_comptime_in_expr(cond)?;
                if let ElvisRhs::Expr(r) = ret {
                    self.fold_comptime_in_expr(r)?;
                }
            }
            ExprKind::Pipe { lhs, stage, .. } => {
                self.fold_comptime_in_expr(lhs)?;
                match stage {
                    PipeStage::Map { body, .. } | PipeStage::Filter { body } => {
                        self.fold_comptime_in_block(body)?;
                    }
                    PipeStage::Fold { init, body, .. } => {
                        self.fold_comptime_in_expr(init)?;
                        self.fold_comptime_in_block(body)?;
                    }
                    PipeStage::Expr(e) => {
                        self.fold_comptime_in_expr(e)?;
                    }
                }
            }
            ExprKind::Func(f) => {
                self.fold_comptime_in_block(&mut f.body)?;
            }
            ExprKind::If {
                branches,
                else_block,
            } => {
                for (c, b) in branches {
                    self.fold_comptime_in_expr(c)?;
                    self.fold_comptime_in_block(b)?;
                }
                if let Some(eb) = else_block {
                    self.fold_comptime_in_block(eb)?;
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.fold_comptime_in_expr(scrutinee)?;
                for arm in arms {
                    if let Some(g) = &mut arm.guard {
                        self.fold_comptime_in_expr(g)?;
                    }
                    self.fold_comptime_in_block(&mut arm.body)?;
                }
            }
            ExprKind::Try {
                body,
                catches,
                finally_block,
            } => {
                self.fold_comptime_in_block(body)?;
                for c in catches {
                    match c {
                        CatchClause::Pattern { body, .. } | CatchClause::Bind { body, .. } => {
                            self.fold_comptime_in_block(body)?;
                        }
                    }
                }
                if let Some(fin) = finally_block {
                    self.fold_comptime_in_block(fin)?;
                }
            }
            ExprKind::Loop { head, body } => {
                match head {
                    LoopHead::Infinite => {}
                    LoopHead::While(c) => self.fold_comptime_in_expr(c)?,
                    LoopHead::ForEach { iter, .. } => self.fold_comptime_in_expr(iter)?,
                }
                self.fold_comptime_in_block(body)?;
            }
            ExprKind::Labeled { expr: inner, .. } => {
                self.fold_comptime_in_expr(inner)?;
            }
            ExprKind::BlockExpr(b) | ExprKind::AsyncSpawn(b) => {
                self.fold_comptime_in_block(b)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn eval_block(&mut self, block: &Block) -> Result<Result<ComptimeValue, Signal>, ComptimeError> {
        self.push_scope();
        let mut last = ComptimeValue::Unit;
        for stmt in &block.stmts {
            match self.eval_stmt(stmt)? {
                Ok(v) => last = v,
                Err(sig) => {
                    self.pop_scope();
                    return Ok(Err(sig));
                }
            }
        }
        self.pop_scope();
        Ok(Ok(last))
    }

    fn eval_stmt(&mut self, stmt: &Stmt) -> Result<Result<ComptimeValue, Signal>, ComptimeError> {
        self.tick(stmt.span)?;
        match &stmt.kind {
            StmtKind::Bind {
                is_mut,
                target,
                value,
                ..
            } => {
                let val = match self.eval_expr(value)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                match target {
                    BindTarget::Single { name, .. } => {
                        self.define_var(name.clone(), val, *is_mut);
                    }
                    BindTarget::Destruct(names) => {
                        if let ComptimeValue::Array(items) = val {
                            for (i, n) in names.iter().enumerate() {
                                if n != "_" {
                                    let elem = items.get(i).cloned().unwrap_or(ComptimeValue::Null);
                                    self.define_var(n.clone(), elem, *is_mut);
                                }
                            }
                        } else {
                            return Err(ComptimeError {
                                message: "expected array/tuple for destructuring in comptime"
                                    .into(),
                                span: stmt.span,
                            });
                        }
                    }
                }
                Ok(Ok(ComptimeValue::Unit))
            }
            StmtKind::Mutate { target, op, value } => {
                let rhs = match self.eval_expr(value)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                match target {
                    MutTarget::Destruct(names) => {
                        if let ComptimeValue::Array(items) = rhs {
                            for (i, n) in names.iter().enumerate() {
                                if n != "_" {
                                    let elem = items.get(i).cloned().unwrap_or(ComptimeValue::Null);
                                    self.assign_var(n, elem, stmt.span)?;
                                }
                            }
                        }
                    }
                    MutTarget::LValue(lv) => {
                        if let ExprKind::Ident(name) = &lv.kind {
                            let final_val = if *op == MutOp::Assign {
                                rhs
                            } else {
                                let cur = self.lookup_var(name).cloned().ok_or_else(|| {
                                    ComptimeError {
                                        message: format!("undefined variable `{}`", name),
                                        span: lv.span,
                                    }
                                })?;
                                let bin_op = match op {
                                    MutOp::Add => BinOp::Add,
                                    MutOp::Sub => BinOp::Sub,
                                    MutOp::Mul => BinOp::Mul,
                                    MutOp::Div => BinOp::Div,
                                    MutOp::Assign => unreachable!(),
                                };
                                self.apply_binary(bin_op, cur, rhs, stmt.span)?
                            };
                            self.assign_var(name, final_val, stmt.span)?;
                        } else {
                            return Err(ComptimeError {
                                message: "unsupported complex lvalue mutation in comptime".into(),
                                span: lv.span,
                            });
                        }
                    }
                }
                Ok(Ok(ComptimeValue::Unit))
            }
            StmtKind::Flow(flow) => self.eval_flow(flow, stmt.span),
            StmtKind::Expr(e) => self.eval_expr(e),
            _ => Ok(Ok(ComptimeValue::Unit)),
        }
    }

    fn eval_flow(
        &mut self,
        flow: &FlowKind,
        span: Span,
    ) -> Result<Result<ComptimeValue, Signal>, ComptimeError> {
        match flow {
            FlowKind::Return(opt) => {
                let v = match opt {
                    Some(e) => match self.eval_expr(e)? {
                        Ok(v) => v,
                        Err(sig) => return Ok(Err(sig)),
                    },
                    None => ComptimeValue::Unit,
                };
                Ok(Err(Signal::Return(v)))
            }
            FlowKind::LocalReturn(opt) => {
                let v = match opt {
                    Some(e) => match self.eval_expr(e)? {
                        Ok(v) => v,
                        Err(sig) => return Ok(Err(sig)),
                    },
                    None => ComptimeValue::Unit,
                };
                Ok(Err(Signal::LocalReturn(v)))
            }
            FlowKind::LabeledReturn(lbl, opt) => {
                let v = match opt {
                    Some(e) => match self.eval_expr(e)? {
                        Ok(v) => v,
                        Err(sig) => return Ok(Err(sig)),
                    },
                    None => ComptimeValue::Unit,
                };
                Ok(Err(Signal::LabeledReturn(lbl.clone(), v)))
            }
            FlowKind::Break(opt) => {
                let v = match opt {
                    Some(e) => match self.eval_expr(e)? {
                        Ok(v) => Some(v),
                        Err(sig) => return Ok(Err(sig)),
                    },
                    None => None,
                };
                Ok(Err(Signal::Break(v)))
            }
            FlowKind::LabeledBreak(lbl, opt) => {
                let v = match opt {
                    Some(e) => match self.eval_expr(e)? {
                        Ok(v) => Some(v),
                        Err(sig) => return Ok(Err(sig)),
                    },
                    None => None,
                };
                Ok(Err(Signal::LabeledBreak(lbl.clone(), v)))
            }
            FlowKind::Continue => Ok(Err(Signal::Continue)),
            FlowKind::LabeledContinue(lbl) => Ok(Err(Signal::LabeledContinue(lbl.clone()))),
            FlowKind::Yield(_) | FlowKind::Throw(_) => Err(ComptimeError {
                message: "generators (`^~`) and throws (`!^`) are not supported inside comptime"
                    .into(),
                span,
            }),
        }
    }

    fn eval_expr(&mut self, expr: &Expr) -> Result<Result<ComptimeValue, Signal>, ComptimeError> {
        self.tick(expr.span)?;
        match &expr.kind {
            ExprKind::Unit => Ok(Ok(ComptimeValue::Unit)),
            ExprKind::Null => Ok(Ok(ComptimeValue::Null)),
            ExprKind::Bool(b) => Ok(Ok(ComptimeValue::Bool(*b))),
            ExprKind::Int(n) => Ok(Ok(ComptimeValue::Int(*n))),
            ExprKind::Float(x) => Ok(Ok(ComptimeValue::Float(*x))),
            ExprKind::Char(c) => Ok(Ok(ComptimeValue::Char(*c))),
            ExprKind::Str(s) => Ok(Ok(ComptimeValue::Str(s.clone()))),
            ExprKind::InterpString(segs) => {
                let mut out = String::new();
                for seg in segs {
                    match seg {
                        InterpSegment::Literal(lit) => out.push_str(lit),
                        InterpSegment::Expr(e) => {
                            let val = match self.eval_expr(e)? {
                                Ok(v) => v,
                                Err(sig) => return Ok(Err(sig)),
                            };
                            out.push_str(&val.display_str());
                        }
                    }
                }
                Ok(Ok(ComptimeValue::Str(out)))
            }
            ExprKind::Ident(name) => {
                let v = self.lookup_var(name).cloned().ok_or_else(|| ComptimeError {
                    message: format!("undefined identifier `{}` in comptime", name),
                    span: expr.span,
                })?;
                Ok(Ok(v))
            }
            ExprKind::CurrentItem => {
                let v = self
                    .current_item_stack
                    .last()
                    .cloned()
                    .ok_or_else(|| ComptimeError {
                        message: "`@` used outside pipeline or method in comptime".into(),
                        span: expr.span,
                    })?;
                Ok(Ok(v))
            }
            ExprKind::SelfFn => {
                let v = self
                    .self_fn_stack
                    .last()
                    .cloned()
                    .ok_or_else(|| ComptimeError {
                        message: "`$` used outside function in comptime".into(),
                        span: expr.span,
                    })?;
                Ok(Ok(v))
            }
            ExprKind::Array(elems) => {
                let mut vals = Vec::with_capacity(elems.len());
                for e in elems {
                    match self.eval_expr(e)? {
                        Ok(v) => vals.push(v),
                        Err(sig) => return Ok(Err(sig)),
                    }
                }
                Ok(Ok(ComptimeValue::Array(vals)))
            }
            ExprKind::RepeatArray { value, count, .. } => {
                let v = match self.eval_expr(value)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                let n = match self.eval_expr(count)? {
                    Ok(ComptimeValue::Int(k)) if k >= 0 => k as usize,
                    Ok(_) => {
                        return Err(ComptimeError {
                            message: "repeat array count must be a non-negative integer".into(),
                            span: count.span,
                        })
                    }
                    Err(sig) => return Ok(Err(sig)),
                };
                Ok(Ok(ComptimeValue::Array(vec![v; n])))
            }
            ExprKind::SetLit(elems) => {
                let mut vals = Vec::new();
                for e in elems {
                    let v = match self.eval_expr(e)? {
                        Ok(v) => v,
                        Err(sig) => return Ok(Err(sig)),
                    };
                    if !vals.contains(&v) {
                        vals.push(v);
                    }
                }
                Ok(Ok(ComptimeValue::Set(vals)))
            }
            ExprKind::MapLit(entries) => {
                let mut pairs = Vec::with_capacity(entries.len());
                for (k_expr, v_expr) in entries {
                    let k = match self.eval_expr(k_expr)? {
                        Ok(v) => v,
                        Err(sig) => return Ok(Err(sig)),
                    };
                    let v = match self.eval_expr(v_expr)? {
                        Ok(v) => v,
                        Err(sig) => return Ok(Err(sig)),
                    };
                    if let Some(existing) = pairs.iter_mut().find(|(ek, _)| ek == &k) {
                        existing.1 = v;
                    } else {
                        pairs.push((k, v));
                    }
                }
                Ok(Ok(ComptimeValue::Map(pairs)))
            }
            ExprKind::StructLit {
                type_name,
                spread,
                fields,
                ..
            } => {
                let mut map_fields: Vec<(String, ComptimeValue)> = Vec::new();
                if let Some(tn) = type_name {
                    if let Some(defs) = self.struct_defs.get(tn).cloned() {
                        for d in defs {
                            if let Some(def_expr) = &d.default {
                                if let Ok(v) = self.eval_expr(def_expr)? {
                                    map_fields.push((d.name, v));
                                }
                            } else {
                                map_fields.push((d.name, ComptimeValue::Null));
                            }
                        }
                    }
                }
                if let Some(sp) = spread {
                    if let ComptimeValue::Struct { fields: base_f, .. } = match self.eval_expr(sp)? {
                        Ok(v) => v,
                        Err(sig) => return Ok(Err(sig)),
                    } {
                        for (bk, bv) in base_f {
                            if let Some(slot) = map_fields.iter_mut().find(|(k, _)| k == &bk) {
                                slot.1 = bv;
                            } else {
                                map_fields.push((bk, bv));
                            }
                        }
                    }
                }
                for (fk, fv_expr) in fields {
                    let fv = match self.eval_expr(fv_expr)? {
                        Ok(v) => v,
                        Err(sig) => return Ok(Err(sig)),
                    };
                    if let Some(slot) = map_fields.iter_mut().find(|(k, _)| k == fk) {
                        slot.1 = fv;
                    } else {
                        map_fields.push((fk.clone(), fv));
                    }
                }
                Ok(Ok(ComptimeValue::Struct {
                    type_name: type_name.clone(),
                    fields: map_fields,
                }))
            }
            ExprKind::Range {
                start: Some(s),
                end: Some(e),
                inclusive,
            } => {
                let sv = match self.eval_expr(s)? {
                    Ok(ComptimeValue::Int(n)) => n,
                    Ok(_) => {
                        return Err(ComptimeError {
                            message: "range start must be integer".into(),
                            span: s.span,
                        })
                    }
                    Err(sig) => return Ok(Err(sig)),
                };
                let ev = match self.eval_expr(e)? {
                    Ok(ComptimeValue::Int(n)) => n,
                    Ok(_) => {
                        return Err(ComptimeError {
                            message: "range end must be integer".into(),
                            span: e.span,
                        })
                    }
                    Err(sig) => return Ok(Err(sig)),
                };
                Ok(Ok(ComptimeValue::Range {
                    start: sv,
                    end: ev,
                    inclusive: *inclusive,
                }))
            }
            ExprKind::Unary { op, expr: inner } => {
                let val = match self.eval_expr(inner)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                let res = match (op, val) {
                    (UnaryOp::Neg, ComptimeValue::Int(n)) => ComptimeValue::Int(-n),
                    (UnaryOp::Neg, ComptimeValue::Float(x)) => ComptimeValue::Float(-x),
                    (UnaryOp::Not, ComptimeValue::Bool(b)) => ComptimeValue::Bool(!b),
                    (UnaryOp::BitNot, ComptimeValue::Int(n)) => ComptimeValue::Int(!n),
                    _ => {
                        return Err(ComptimeError {
                            message: "invalid unary operation in comptime".into(),
                            span: expr.span,
                        })
                    }
                };
                Ok(Ok(res))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                if *op == BinOp::And {
                    let l = match self.eval_expr(lhs)? {
                        Ok(ComptimeValue::Bool(b)) => b,
                        Ok(_) => {
                            return Err(ComptimeError {
                                message: "`&` expects bool".into(),
                                span: lhs.span,
                            })
                        }
                        Err(sig) => return Ok(Err(sig)),
                    };
                    if !l {
                        return Ok(Ok(ComptimeValue::Bool(false)));
                    }
                    return self.eval_expr(rhs);
                }
                if *op == BinOp::Or {
                    let l = match self.eval_expr(lhs)? {
                        Ok(ComptimeValue::Bool(b)) => b,
                        Ok(_) => {
                            return Err(ComptimeError {
                                message: "`|` expects bool".into(),
                                span: lhs.span,
                            })
                        }
                        Err(sig) => return Ok(Err(sig)),
                    };
                    if l {
                        return Ok(Ok(ComptimeValue::Bool(true)));
                    }
                    return self.eval_expr(rhs);
                }
                let l = match self.eval_expr(lhs)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                let r = match self.eval_expr(rhs)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                Ok(Ok(self.apply_binary(*op, l, r, expr.span)?))
            }
            ExprKind::Field {
                expr: obj_expr,
                field,
                safe,
            } => {
                let obj = match self.eval_expr(obj_expr)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                if *safe && obj == ComptimeValue::Null {
                    return Ok(Ok(ComptimeValue::Null));
                }
                match obj {
                    ComptimeValue::Struct { fields, .. } => {
                        if let Some((_, v)) = fields.into_iter().find(|(k, _)| k == field) {
                            Ok(Ok(v))
                        } else {
                            Err(ComptimeError {
                                message: format!("unknown field `{}`", field),
                                span: expr.span,
                            })
                        }
                    }
                    ComptimeValue::Array(items) if field == "len" => {
                        Ok(Ok(ComptimeValue::Int(items.len() as i64)))
                    }
                    ComptimeValue::Str(s) if field == "len" => {
                        Ok(Ok(ComptimeValue::Int(s.len() as i64)))
                    }
                    ComptimeValue::BuiltinMod(m) if m == "math" => match field.as_str() {
                        "pi" => Ok(Ok(ComptimeValue::Float(std::f64::consts::PI))),
                        "e" => Ok(Ok(ComptimeValue::Float(std::f64::consts::E))),
                        "tau" => Ok(Ok(ComptimeValue::Float(std::f64::consts::TAU))),
                        _ => Err(ComptimeError {
                            message: format!("unknown math constant `{}`", field),
                            span: expr.span,
                        }),
                    },
                    _ => Err(ComptimeError {
                        message: format!("cannot access `.{}` on comptime value", field),
                        span: expr.span,
                    }),
                }
            }
            ExprKind::Index {
                expr: obj_expr,
                index,
                safe,
            } => {
                let obj = match self.eval_expr(obj_expr)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                if *safe && obj == ComptimeValue::Null {
                    return Ok(Ok(ComptimeValue::Null));
                }
                let idx = match self.eval_expr(index)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                match (obj, idx) {
                    (ComptimeValue::Array(items), ComptimeValue::Int(i)) => {
                        if i < 0 || (i as usize) >= items.len() {
                            if *safe {
                                Ok(Ok(ComptimeValue::Null))
                            } else {
                                Err(ComptimeError {
                                    message: format!("index {} out of bounds", i),
                                    span: expr.span,
                                })
                            }
                        } else {
                            Ok(Ok(items[i as usize].clone()))
                        }
                    }
                    (ComptimeValue::Map(entries), key) => {
                        let found = entries
                            .into_iter()
                            .find(|(k, _)| k == &key)
                            .map(|(_, v)| v)
                            .unwrap_or(ComptimeValue::Null);
                        Ok(Ok(found))
                    }
                    _ => Err(ComptimeError {
                        message: "invalid indexing operation in comptime".into(),
                        span: expr.span,
                    }),
                }
            }
            ExprKind::Elvis { lhs, rhs } => {
                let l = match self.eval_expr(lhs)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                if l != ComptimeValue::Null {
                    return Ok(Ok(l));
                }
                match rhs {
                    ElvisRhs::Expr(r) => self.eval_expr(r),
                    ElvisRhs::Flow(f) => self.eval_flow(f, expr.span),
                }
            }
            ExprKind::GuardReturn { cond, ret } => {
                let c = match self.eval_expr(cond)? {
                    Ok(ComptimeValue::Bool(b)) => b,
                    Ok(_) => {
                        return Err(ComptimeError {
                            message: "`?^` condition must be bool".into(),
                            span: cond.span,
                        })
                    }
                    Err(sig) => return Ok(Err(sig)),
                };
                if c {
                    match ret {
                        ElvisRhs::Expr(r) => match self.eval_expr(r)? {
                            Ok(v) => Ok(Err(Signal::Return(v))),
                            Err(sig) => Ok(Err(sig)),
                        },
                        ElvisRhs::Flow(f) => self.eval_flow(f, expr.span),
                    }
                } else {
                    Ok(Ok(ComptimeValue::Unit))
                }
            }
            ExprKind::NonNullAssert(inner) => {
                let v = match self.eval_expr(inner)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                if v == ComptimeValue::Null {
                    Err(ComptimeError {
                        message: "`!!` assertion failed on `~` in comptime".into(),
                        span: expr.span,
                    })
                } else {
                    Ok(Ok(v))
                }
            }
            ExprKind::Call {
                callee, args, safe, ..
            } => self.eval_call(callee, args, *safe, expr.span),
            ExprKind::Pipe { lhs, kind, stage } => self.eval_pipe(lhs, *kind, stage, expr.span),
            ExprKind::Func(f) => Ok(Ok(ComptimeValue::Func {
                params: f.params.clone(),
                body: f.body.clone(),
            })),
            ExprKind::If {
                branches,
                else_block,
            } => {
                for (cond, blk) in branches {
                    let b = match self.eval_expr(cond)? {
                        Ok(ComptimeValue::Bool(b)) => b,
                        Ok(_) => {
                            return Err(ComptimeError {
                                message: "if condition must be bool".into(),
                                span: cond.span,
                            })
                        }
                        Err(sig) => return Ok(Err(sig)),
                    };
                    if b {
                        return self.eval_block(blk);
                    }
                }
                if let Some(eb) = else_block {
                    self.eval_block(eb)
                } else {
                    Ok(Ok(ComptimeValue::Unit))
                }
            }
            ExprKind::Loop { head, body } => self.eval_loop(head, body),
            ExprKind::Labeled { label, expr: inner } => match self.eval_expr(inner)? {
                Ok(v) => Ok(Ok(v)),
                Err(Signal::LabeledReturn(ref l, v)) if l == label => Ok(Ok(v)),
                Err(Signal::LabeledBreak(ref l, v)) if l == label => {
                    Ok(Ok(v.unwrap_or(ComptimeValue::Null)))
                }
                Err(other) => Ok(Err(other)),
            },
            ExprKind::BlockExpr(blk) | ExprKind::Comptime(blk) => self.eval_block(blk),
            ExprKind::Cast {
                expr: inner, ty, ..
            } => {
                let v = match self.eval_expr(inner)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                if let TypeExpr::Named { name, .. } = ty {
                    match (name.as_str(), &v) {
                        ("f64" | "f32", ComptimeValue::Int(n)) => {
                            return Ok(Ok(ComptimeValue::Float(*n as f64)))
                        }
                        ("i64" | "i32" | "i16" | "i8" | "u64" | "u32" | "u8" | "usz", ComptimeValue::Float(x)) => {
                            return Ok(Ok(ComptimeValue::Int(*x as i64)))
                        }
                        _ => {}
                    }
                }
                Ok(Ok(v))
            }
            _ => Err(ComptimeError {
                message: "unsupported expression inside comptime `<{ ... }>`".into(),
                span: expr.span,
            }),
        }
    }

    fn eval_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        safe: bool,
        span: Span,
    ) -> Result<Result<ComptimeValue, Signal>, ComptimeError> {
        // Method / module calls: `math.sin(x)`, `s.trim()`, `s.upper()`, etc.
        if let ExprKind::Field {
            expr: recv_expr,
            field,
            safe: field_safe,
        } = &callee.kind
        {
            let recv = match self.eval_expr(recv_expr)? {
                Ok(v) => v,
                Err(sig) => return Ok(Err(sig)),
            };
            if (*field_safe || safe) && recv == ComptimeValue::Null {
                return Ok(Ok(ComptimeValue::Null));
            }
            let mut arg_vals = Vec::with_capacity(args.len());
            for a in args {
                match self.eval_expr(a)? {
                    Ok(v) => arg_vals.push(v),
                    Err(sig) => return Ok(Err(sig)),
                }
            }
            match recv {
                ComptimeValue::BuiltinMod(m) if m == "math" => {
                    return Ok(Ok(self.eval_math_builtin(field, &arg_vals, span)?));
                }
                ComptimeValue::Str(s) => match field.as_str() {
                    "trim" => return Ok(Ok(ComptimeValue::Str(s.trim().to_string()))),
                    "upper" => return Ok(Ok(ComptimeValue::Str(s.to_uppercase()))),
                    "lower" => return Ok(Ok(ComptimeValue::Str(s.to_lowercase()))),
                    "len" => return Ok(Ok(ComptimeValue::Int(s.len() as i64))),
                    "contains" => {
                        if let Some(ComptimeValue::Str(sub)) = arg_vals.first() {
                            return Ok(Ok(ComptimeValue::Bool(s.contains(sub.as_str()))));
                        }
                    }
                    _ => {}
                },
                ComptimeValue::Set(items) if field == "has" => {
                    if let Some(needle) = arg_vals.first() {
                        return Ok(Ok(ComptimeValue::Bool(items.contains(needle))));
                    }
                }
                _ => {}
            }
        }

        let fn_val = match self.eval_expr(callee)? {
            Ok(v) => v,
            Err(sig) => return Ok(Err(sig)),
        };
        if safe && fn_val == ComptimeValue::Null {
            return Ok(Ok(ComptimeValue::Null));
        }

        let ComptimeValue::Func { params, body } = fn_val.clone() else {
            return Err(ComptimeError {
                message: "expression is not callable in comptime".into(),
                span,
            });
        };

        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            match self.eval_expr(a)? {
                Ok(v) => arg_vals.push(v),
                Err(sig) => return Ok(Err(sig)),
            }
        }

        self.push_scope();
        for (i, p) in params.iter().enumerate() {
            let v = if let Some(av) = arg_vals.get(i) {
                av.clone()
            } else if let Some(def) = &p.default {
                match self.eval_expr(def)? {
                    Ok(dv) => dv,
                    Err(sig) => {
                        self.pop_scope();
                        return Ok(Err(sig));
                    }
                }
            } else {
                self.pop_scope();
                return Err(ComptimeError {
                    message: format!("missing argument `{}` in comptime call", p.name),
                    span,
                });
            };
            self.define_var(p.name.clone(), v, false);
        }

        self.self_fn_stack.push(fn_val);
        let res = self.eval_block(&body)?;
        self.self_fn_stack.pop();
        self.pop_scope();

        match res {
            Ok(v) | Err(Signal::Return(v)) => Ok(Ok(v)),
            Err(other) => Ok(Err(other)),
        }
    }

    fn eval_math_builtin(
        &self,
        name: &str,
        args: &[ComptimeValue],
        span: Span,
    ) -> Result<ComptimeValue, ComptimeError> {
        let as_f64 = |v: &ComptimeValue| -> Result<f64, ComptimeError> {
            match v {
                ComptimeValue::Float(x) => Ok(*x),
                ComptimeValue::Int(n) => Ok(*n as f64),
                _ => Err(ComptimeError {
                    message: format!("math.{} expects numeric argument", name),
                    span,
                }),
            }
        };
        match name {
            "sin" => Ok(ComptimeValue::Float(as_f64(&args[0])?.sin())),
            "cos" => Ok(ComptimeValue::Float(as_f64(&args[0])?.cos())),
            "tan" => Ok(ComptimeValue::Float(as_f64(&args[0])?.tan())),
            "sqrt" => Ok(ComptimeValue::Float(as_f64(&args[0])?.sqrt())),
            "floor" => Ok(ComptimeValue::Float(as_f64(&args[0])?.floor())),
            "ceil" => Ok(ComptimeValue::Float(as_f64(&args[0])?.ceil())),
            "round" => Ok(ComptimeValue::Float(as_f64(&args[0])?.round())),
            "abs" => match &args[0] {
                ComptimeValue::Int(n) => Ok(ComptimeValue::Int(n.abs())),
                _ => Ok(ComptimeValue::Float(as_f64(&args[0])?.abs())),
            },
            "pow" => Ok(ComptimeValue::Float(
                as_f64(&args[0])?.powf(as_f64(&args[1])?),
            )),
            "min" => Ok(ComptimeValue::Float(
                as_f64(&args[0])?.min(as_f64(&args[1])?),
            )),
            "max" => Ok(ComptimeValue::Float(
                as_f64(&args[0])?.max(as_f64(&args[1])?),
            )),
            "clamp" => Ok(ComptimeValue::Float(
                as_f64(&args[0])?.clamp(as_f64(&args[1])?, as_f64(&args[2])?),
            )),
            _ => Err(ComptimeError {
                message: format!("unknown math builtin `{}`", name),
                span,
            }),
        }
    }

    fn value_to_items(
        &self,
        val: ComptimeValue,
        span: Span,
    ) -> Result<Vec<ComptimeValue>, ComptimeError> {
        match val {
            ComptimeValue::Array(items) => Ok(items),
            ComptimeValue::Range {
                start,
                end,
                inclusive,
            } => {
                let mut out = Vec::new();
                if inclusive {
                    for i in start..=end {
                        out.push(ComptimeValue::Int(i));
                    }
                } else {
                    for i in start..end {
                        out.push(ComptimeValue::Int(i));
                    }
                }
                Ok(out)
            }
            _ => Err(ComptimeError {
                message: "expected iterable array or range in comptime".into(),
                span,
            }),
        }
    }

    fn eval_pipe(
        &mut self,
        lhs: &Expr,
        kind: PipeKind,
        stage: &PipeStage,
        span: Span,
    ) -> Result<Result<ComptimeValue, Signal>, ComptimeError> {
        let left_val = match self.eval_expr(lhs)? {
            Ok(v) => v,
            Err(sig) => return Ok(Err(sig)),
        };

        // Scalar safe pipe `val ?~> { ... }`
        if kind == PipeKind::Safe
            && !matches!(
                left_val,
                ComptimeValue::Array(_) | ComptimeValue::Range { .. }
            )
        {
            if left_val == ComptimeValue::Null {
                return Ok(Ok(ComptimeValue::Null));
            }
            if let PipeStage::Map { param, body } = stage {
                self.current_item_stack.push(left_val.clone());
                self.push_scope();
                if let Some(p) = param {
                    self.define_var(p.clone(), left_val, false);
                }
                let res = self.eval_block(body)?;
                self.pop_scope();
                self.current_item_stack.pop();
                return match res {
                    Ok(v) | Err(Signal::LocalReturn(v)) => Ok(Ok(v)),
                    Err(other) => Ok(Err(other)),
                };
            }
        }

        let items = self.value_to_items(left_val, span)?;
        match stage {
            PipeStage::Map { param, body } => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    if kind == PipeKind::Safe && item == ComptimeValue::Null {
                        continue;
                    }
                    self.current_item_stack.push(item.clone());
                    self.push_scope();
                    if let Some(p) = param {
                        self.define_var(p.clone(), item, false);
                    }
                    let res = self.eval_block(body)?;
                    self.pop_scope();
                    self.current_item_stack.pop();
                    match res {
                        Ok(v) | Err(Signal::LocalReturn(v)) => out.push(v),
                        Err(other) => return Ok(Err(other)),
                    }
                }
                Ok(Ok(ComptimeValue::Array(out)))
            }
            PipeStage::Filter { body } => {
                let mut out = Vec::new();
                for item in items {
                    self.current_item_stack.push(item.clone());
                    let res = self.eval_block(body)?;
                    self.current_item_stack.pop();
                    match res {
                        Ok(ComptimeValue::Bool(true)) | Err(Signal::LocalReturn(ComptimeValue::Bool(true))) => {
                            out.push(item)
                        }
                        Ok(ComptimeValue::Bool(false))
                        | Err(Signal::LocalReturn(ComptimeValue::Bool(false))) => {}
                        Ok(_) => {
                            return Err(ComptimeError {
                                message: "filter stage `? { ... }` must return bool".into(),
                                span,
                            })
                        }
                        Err(other) => return Ok(Err(other)),
                    }
                }
                Ok(Ok(ComptimeValue::Array(out)))
            }
            PipeStage::Fold {
                acc_name,
                init,
                body,
            } => {
                let mut acc = match self.eval_expr(init)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                for item in items {
                    self.current_item_stack.push(item);
                    self.push_scope();
                    self.define_var(acc_name.clone(), acc, true);
                    let res = self.eval_block(body)?;
                    self.pop_scope();
                    self.current_item_stack.pop();
                    match res {
                        Ok(v) | Err(Signal::LocalReturn(v)) => acc = v,
                        Err(other) => return Ok(Err(other)),
                    }
                }
                Ok(Ok(acc))
            }
            PipeStage::Expr(fn_expr) => {
                let fn_val = match self.eval_expr(fn_expr)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                let ComptimeValue::Func { params, body } = fn_val.clone() else {
                    return Err(ComptimeError {
                        message: "expected function in pipeline stage".into(),
                        span: fn_expr.span,
                    });
                };
                self.push_scope();
                if let Some(p) = params.first() {
                    self.define_var(p.name.clone(), ComptimeValue::Array(items), false);
                }
                let res = self.eval_block(&body)?;
                self.pop_scope();
                match res {
                    Ok(v) | Err(Signal::Return(v)) => Ok(Ok(v)),
                    Err(other) => Ok(Err(other)),
                }
            }
        }
    }

    fn eval_loop(
        &mut self,
        head: &LoopHead,
        body: &Block,
    ) -> Result<Result<ComptimeValue, Signal>, ComptimeError> {
        match head {
            LoopHead::Infinite => loop {
                match self.eval_block(body)? {
                    Ok(_) | Err(Signal::Continue) => {}
                    Err(Signal::Break(v)) => return Ok(Ok(v.unwrap_or(ComptimeValue::Unit))),
                    Err(other) => return Ok(Err(other)),
                }
            },
            LoopHead::While(cond) => loop {
                let c = match self.eval_expr(cond)? {
                    Ok(ComptimeValue::Bool(b)) => b,
                    Ok(_) => {
                        return Err(ComptimeError {
                            message: "loop condition must be bool".into(),
                            span: cond.span,
                        })
                    }
                    Err(sig) => return Ok(Err(sig)),
                };
                if !c {
                    return Ok(Ok(ComptimeValue::Unit));
                }
                match self.eval_block(body)? {
                    Ok(_) | Err(Signal::Continue) => {}
                    Err(Signal::Break(v)) => return Ok(Ok(v.unwrap_or(ComptimeValue::Unit))),
                    Err(other) => return Ok(Err(other)),
                }
            },
            LoopHead::ForEach { var, iter } => {
                let it_val = match self.eval_expr(iter)? {
                    Ok(v) => v,
                    Err(sig) => return Ok(Err(sig)),
                };
                let items = self.value_to_items(it_val, iter.span)?;
                for item in items {
                    self.push_scope();
                    self.define_var(var.clone(), item, false);
                    let res = self.eval_block(body)?;
                    self.pop_scope();
                    match res {
                        Ok(_) | Err(Signal::Continue) => {}
                        Err(Signal::Break(v)) => return Ok(Ok(v.unwrap_or(ComptimeValue::Null))),
                        Err(other) => return Ok(Err(other)),
                    }
                }
                Ok(Ok(ComptimeValue::Null))
            }
        }
    }

    fn apply_binary(
        &self,
        op: BinOp,
        lhs: ComptimeValue,
        rhs: ComptimeValue,
        span: Span,
    ) -> Result<ComptimeValue, ComptimeError> {
        match (op, lhs, rhs) {
            (BinOp::Eq, l, r) => Ok(ComptimeValue::Bool(l == r)),
            (BinOp::NotEq, l, r) => Ok(ComptimeValue::Bool(l != r)),

            (BinOp::Add, ComptimeValue::Str(a), ComptimeValue::Str(b)) => {
                Ok(ComptimeValue::Str(format!("{}{}", a, b)))
            }

            (BinOp::Add, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                Ok(ComptimeValue::Int(a.wrapping_add(b)))
            }
            (BinOp::Sub, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                Ok(ComptimeValue::Int(a.wrapping_sub(b)))
            }
            (BinOp::Mul, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                Ok(ComptimeValue::Int(a.wrapping_mul(b)))
            }
            (BinOp::Div, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                if b == 0 {
                    return Err(ComptimeError {
                        message: "division by zero in comptime".into(),
                        span,
                    });
                }
                Ok(ComptimeValue::Int(a / b))
            }
            (BinOp::Rem, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                if b == 0 {
                    return Err(ComptimeError {
                        message: "modulo by zero in comptime".into(),
                        span,
                    });
                }
                Ok(ComptimeValue::Int(a % b))
            }
            (BinOp::BitAnd, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                Ok(ComptimeValue::Int(a & b))
            }
            (BinOp::BitOr, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                Ok(ComptimeValue::Int(a | b))
            }
            (BinOp::BitXor, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                Ok(ComptimeValue::Int(a ^ b))
            }
            (BinOp::BitShl, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                Ok(ComptimeValue::Int(a.wrapping_shl(b as u32)))
            }
            (BinOp::BitShr, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                Ok(ComptimeValue::Int(a.wrapping_shr(b as u32)))
            }
            (BinOp::Lt, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                Ok(ComptimeValue::Bool(a < b))
            }
            (BinOp::Gt, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                Ok(ComptimeValue::Bool(a > b))
            }
            (BinOp::LtEq, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                Ok(ComptimeValue::Bool(a <= b))
            }
            (BinOp::GtEq, ComptimeValue::Int(a), ComptimeValue::Int(b)) => {
                Ok(ComptimeValue::Bool(a >= b))
            }

            // Mixed numeric (Int/Float) promotion to Float
            (
                op,
                l @ (ComptimeValue::Float(_) | ComptimeValue::Int(_)),
                r @ (ComptimeValue::Float(_) | ComptimeValue::Int(_)),
            ) => {
                let a = match l {
                    ComptimeValue::Float(x) => x,
                    ComptimeValue::Int(n) => n as f64,
                    _ => unreachable!(),
                };
                let b = match r {
                    ComptimeValue::Float(x) => x,
                    ComptimeValue::Int(n) => n as f64,
                    _ => unreachable!(),
                };
                match op {
                    BinOp::Add => Ok(ComptimeValue::Float(a + b)),
                    BinOp::Sub => Ok(ComptimeValue::Float(a - b)),
                    BinOp::Mul => Ok(ComptimeValue::Float(a * b)),
                    BinOp::Div => Ok(ComptimeValue::Float(a / b)),
                    BinOp::Rem => Ok(ComptimeValue::Float(a % b)),
                    BinOp::Lt => Ok(ComptimeValue::Bool(a < b)),
                    BinOp::Gt => Ok(ComptimeValue::Bool(a > b)),
                    BinOp::LtEq => Ok(ComptimeValue::Bool(a <= b)),
                    BinOp::GtEq => Ok(ComptimeValue::Bool(a >= b)),
                    _ => Err(ComptimeError {
                        message: "unsupported float binary operator in comptime".into(),
                        span,
                    }),
                }
            }

            (BinOp::Lt, ComptimeValue::Str(a), ComptimeValue::Str(b)) => {
                Ok(ComptimeValue::Bool(a < b))
            }
            (BinOp::Gt, ComptimeValue::Str(a), ComptimeValue::Str(b)) => {
                Ok(ComptimeValue::Bool(a > b))
            }
            (BinOp::LtEq, ComptimeValue::Str(a), ComptimeValue::Str(b)) => {
                Ok(ComptimeValue::Bool(a <= b))
            }
            (BinOp::GtEq, ComptimeValue::Str(a), ComptimeValue::Str(b)) => {
                Ok(ComptimeValue::Bool(a >= b))
            }

            _ => Err(ComptimeError {
                message: "incompatible operand types in comptime binary expression".into(),
                span,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;

    #[test]
    fn test_comptime_sin_table_from_lo_md() {
        let src = r#"
math = <"std/math">
sin_table = <{
  0..360 ~> { math.sin(@ * 3.14159 / 180.0) }
}>
"#;
        let mut prog = Parser::parse_source(src).unwrap();
        ComptimeEvaluator::evaluate_program(&mut prog).expect("comptime sin_table should evaluate");
        match &prog.items[1].kind {
            StmtKind::Bind { value, .. } => match &value.kind {
                ExprKind::Array(elems) => {
                    assert_eq!(elems.len(), 360);
                    assert_eq!(elems[0].kind, ExprKind::Float(0.0));
                }
                other => panic!("expected folded Array, got {:?}", other),
            },
            other => panic!("expected Bind, got {:?}", other),
        }
    }

    #[test]
    fn test_comptime_map_and_functions() {
        let src = r#"
fact = (n) {
  ? (n <= 1) { ^ 1 }
  n * $(n - 1)
}
fact_6 = <{ fact(6) }>
base_prices = <{
  ["laptop" => 1200.0, "mouse" => 25.0, "keyboard" => 80.0]
}>
"#;
        let mut prog = Parser::parse_source(src).unwrap();
        ComptimeEvaluator::evaluate_program(&mut prog).expect("comptime should evaluate");
        match &prog.items[1].kind {
            StmtKind::Bind { value, .. } => assert_eq!(value.kind, ExprKind::Int(720)),
            _ => panic!("expected Bind"),
        }
        match &prog.items[2].kind {
            StmtKind::Bind { value, .. } => match &value.kind {
                ExprKind::MapLit(entries) => assert_eq!(entries.len(), 3),
                other => panic!("expected MapLit, got {:?}", other),
            },
            _ => panic!("expected Bind"),
        }
    }
}
