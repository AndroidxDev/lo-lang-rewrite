use crate::lexer::Span;
use crate::parser::*;
use crate::sema::env::*;
use crate::sema::types::Type;
use std::collections::HashMap;

use super::TypeChecker;

impl TypeChecker {
    pub(crate) fn check_block(
        &mut self,
        block: &Block,
        ctx: &mut FnCheckCtx,
    ) -> Result<Type, SemaError> {
        ctx.push_scope();
        let mut last_ty = Type::Unit;
        let mut diverged = false;

        for stmt in &block.stmts {
            let sty = self.check_stmt_in_fn(stmt, ctx)?;
            if sty == Type::Never {
                diverged = true;
            }
            last_ty = sty;
        }

        ctx.pop_scope();
        if diverged {
            Ok(Type::Never)
        } else {
            Ok(last_ty)
        }
    }

    pub(crate) fn check_stmt_in_fn(
        &mut self,
        stmt: &Stmt,
        ctx: &mut FnCheckCtx,
    ) -> Result<Type, SemaError> {
        match &stmt.kind {
            StmtKind::Bind {
                is_mut,
                target,
                value,
                ..
            } => {
                let expected_hint = match target {
                    BindTarget::Single { ty: Some(te), .. } => {
                        Some(self.resolve_type_expr(te, &ctx.type_subst, stmt.span)?)
                    }
                    _ => None,
                };
                let val_ty = self.check_expr(value, expected_hint.as_ref(), ctx)?;
                match target {
                    BindTarget::Single { name, ty } => {
                        let final_ty = if let Some(te) = ty {
                            let declared =
                                self.resolve_type_expr(te, &ctx.type_subst, stmt.span)?;
                            if !val_ty.is_assignable_to(&declared) {
                                return Err(SemaError {
                                    message: format!(
                                        "cannot assign value of type `{}` to variable `{}` of type `{}`",
                                        val_ty, name, declared
                                    ),
                                    span: stmt.span,
                                });
                            }
                            declared
                        } else if val_ty == Type::Null {
                            return Err(SemaError {
                                message: format!(
                                    "cannot infer type of `{}` from bare `~`; add explicit nullable type tag like `[str?]`",
                                    name
                                ),
                                span: stmt.span,
                            });
                        } else {
                            val_ty
                        };
                        ctx.record_expr_type(stmt.span, final_ty.clone());
                        if name != "_" {
                            ctx.define_var(name, final_ty, *is_mut, stmt.span)?;
                        }
                    }
                    BindTarget::Destruct(names) => match &val_ty {
                        Type::Tuple(elems) => {
                            if elems.len() != names.len() {
                                return Err(SemaError {
                                    message: format!(
                                        "cannot destructure {}-element tuple into {} variables",
                                        elems.len(),
                                        names.len()
                                    ),
                                    span: stmt.span,
                                });
                            }
                            for (n, ety) in names.iter().zip(elems.iter()) {
                                if n != "_" {
                                    ctx.define_var(n, ety.clone(), *is_mut, stmt.span)?;
                                }
                            }
                        }
                        Type::Array(elem_ty) => {
                            for n in names {
                                if n != "_" {
                                    ctx.define_var(n, (**elem_ty).clone(), *is_mut, stmt.span)?;
                                }
                            }
                        }
                        other => {
                            return Err(SemaError {
                                message: format!(
                                    "cannot destructure value of type `{}` with `[...]`",
                                    other
                                ),
                                span: stmt.span,
                            })
                        }
                    },
                }
                Ok(Type::Unit)
            }
            StmtKind::Mutate { target, op, value } => {
                match target {
                    MutTarget::Destruct(names) => {
                        let val_ty = self.check_expr(value, None, ctx)?;
                        for (idx, n) in names.iter().enumerate() {
                            if n == "_" {
                                continue;
                            }
                            let var_info = ctx.lookup_var_raw(n).cloned().ok_or_else(|| {
                                SemaError {
                                    message: format!("undefined variable `{}` in mutation", n),
                                    span: stmt.span,
                                }
                            })?;
                            if !var_info.is_mut {
                                return Err(SemaError {
                                    message: format!(
                                        "cannot mutate immutable binding `{}`; declare with `~=`",
                                        n
                                    ),
                                    span: stmt.span,
                                });
                            }
                            if ctx.par_depth > var_info.par_depth {
                                return Err(SemaError {
                                    message: format!(
                                        "cannot mutate outer variable `{}` inside parallel pipeline `~>>`",
                                        n
                                    ),
                                    span: stmt.span,
                                });
                            }
                            let rhs_elem = match &val_ty {
                                Type::Tuple(ts) => ts.get(idx).cloned().unwrap_or(Type::Unit),
                                Type::Array(et) => (**et).clone(),
                                _ => {
                                    return Err(SemaError {
                                        message: "expected tuple or array in destructuring mutation"
                                            .into(),
                                        span: value.span,
                                    })
                                }
                            };
                            if !rhs_elem.is_assignable_to(&var_info.ty) {
                                return Err(SemaError {
                                    message: format!(
                                        "cannot assign `{}` to `{}` of type `{}`",
                                        rhs_elem, n, var_info.ty
                                    ),
                                    span: stmt.span,
                                });
                            }
                            ctx.clear_narrowing(n);
                        }
                    }
                    MutTarget::LValue(lv) => {
                        self.verify_mutable_lvalue(lv, ctx)?;
                        // Special case: map key insertion/update `cart["banana"] <- 3`
                        if let ExprKind::Index {
                            expr: map_expr,
                            index: key_expr,
                            safe: false,
                        } = &lv.kind
                        {
                            let m_ty = self.check_expr(map_expr, None, ctx)?;
                            if let Type::Map(k_ty, v_ty) = m_ty {
                                let kt = self.check_expr(key_expr, Some(&k_ty), ctx)?;
                                if !kt.is_assignable_to(&k_ty) {
                                    return Err(SemaError {
                                        message: format!(
                                            "map key type mismatch: expected `{}`, found `{}`",
                                            k_ty, kt
                                        ),
                                        span: key_expr.span,
                                    });
                                }
                                let vt = self.check_expr(value, Some(&v_ty), ctx)?;
                                if !vt.is_assignable_to(&v_ty) {
                                    return Err(SemaError {
                                        message: format!(
                                            "map value type mismatch: expected `{}`, found `{}`",
                                            v_ty, vt
                                        ),
                                        span: value.span,
                                    });
                                }
                                ctx.record_expr_type(lv.span, (*v_ty).clone());
                                return Ok(Type::Unit);
                            }
                        }

                        // Lookup raw (unnarrowed) variable type if simple identifier so `x <- ~` works on `x [T?] ~= "hi"`
                        let target_ty = if let ExprKind::Ident(name) = &lv.kind {
                            let raw_ty = ctx.lookup_var_raw(name).unwrap().ty.clone();
                            ctx.record_expr_type(lv.span, raw_ty.clone());
                            ctx.clear_narrowing(name);
                            raw_ty
                        } else {
                            self.check_expr(lv, None, ctx)?
                        };

                        let rhs_ty = self.check_expr(value, Some(&target_ty), ctx)?;
                        if *op == MutOp::Assign {
                            if !rhs_ty.is_assignable_to(&target_ty) {
                                return Err(SemaError {
                                    message: format!(
                                        "cannot assign value of type `{}` to target of type `{}`",
                                        rhs_ty, target_ty
                                    ),
                                    span: stmt.span,
                                });
                            }
                        } else if !(target_ty.is_numeric() && rhs_ty.is_numeric())
                            && !(*op == MutOp::Add && target_ty == Type::Str && rhs_ty == Type::Str)
                        {
                            return Err(SemaError {
                                message: format!(
                                    "compound mutation `{:?}` cannot be applied to `{}` and `{}`",
                                    op, target_ty, rhs_ty
                                ),
                                span: stmt.span,
                            });
                        }
                    }
                }
                Ok(Type::Unit)
            }
            StmtKind::Defer { body, .. } => {
                let _ = self.check_block(body, ctx)?;
                Ok(Type::Unit)
            }
            StmtKind::Flow(flow) => {
                self.check_flow(flow, stmt.span, ctx)?;
                Ok(Type::Never)
            }
            StmtKind::Expr(expr) => {
                // Early-exit Smart Cast narrowing:
                // 1) `? (x == ~) { ^ ... }` where then-block returns Never and there is no else-block
                if let ExprKind::If {
                    branches,
                    else_block: None,
                } = &expr.kind
                {
                    if branches.len() == 1 {
                        let (cond, then_blk) = &branches[0];
                        let narrow =
                            extract_narrowings(cond, &|n| ctx.lookup_var_effective_ty(n));
                        let cond_ty = self.check_cond_with_narrowing(cond, ctx)?;
                        if cond_ty != Type::Bool {
                            return Err(SemaError {
                                message: format!("condition must be `bool`, found `{}`", cond_ty),
                                span: cond.span,
                            });
                        }
                        ctx.push_narrowings(&narrow.when_true);
                        let then_ty = self.check_block(then_blk, ctx)?;
                        ctx.pop_narrowings();
                        ctx.record_expr_type(expr.span, Type::Unit);
                        if then_ty == Type::Never {
                            ctx.apply_block_narrowings(&narrow.when_false);
                        }
                        return Ok(Type::Unit);
                    }
                }

                // 2) `(x == ~) ?^ ...` guard return narrows `x` for the rest of the block
                if let ExprKind::GuardReturn { cond, .. } = &expr.kind {
                    let narrow = extract_narrowings(cond, &|n| ctx.lookup_var_effective_ty(n));
                    let ety = self.check_expr(expr, None, ctx)?;
                    ctx.apply_block_narrowings(&narrow.when_false);
                    return Ok(ety);
                }

                self.check_expr(expr, None, ctx)
            }
            StmtKind::TypeDef { .. } | StmtKind::MethodDef { .. } => Err(SemaError {
                message: "type and method definitions are only allowed at module top level".into(),
                span: stmt.span,
            }),
        }
    }

    pub(crate) fn verify_mutable_lvalue(&self, lv: &Expr, ctx: &FnCheckCtx) -> Result<(), SemaError> {
        match &lv.kind {
            ExprKind::Ident(name) => {
                let info = ctx.lookup_var_raw(name).ok_or_else(|| SemaError {
                    message: format!("undefined variable `{}` in mutation", name),
                    span: lv.span,
                })?;
                if !info.is_mut {
                    return Err(SemaError {
                        message: format!(
                            "cannot mutate immutable binding `{}`; declare with `~=` to allow mutation",
                            name
                        ),
                        span: lv.span,
                    });
                }
                if ctx.par_depth > info.par_depth {
                    return Err(SemaError {
                        message: format!(
                            "cannot mutate outer variable `{}` inside parallel pipeline `~>>`",
                            name
                        ),
                        span: lv.span,
                    });
                }
                Ok(())
            }
            ExprKind::CurrentItem => {
                if !ctx.current_item_is_mut() {
                    return Err(SemaError {
                        message:
                            "cannot mutate `@` inside a non-mutating method or pipeline; add `!` suffix to method name"
                                .into(),
                        span: lv.span,
                    });
                }
                Ok(())
            }
            ExprKind::Field { expr: base, safe, .. } => {
                if *safe {
                    return Err(SemaError {
                        message: "cannot mutate through safe-call `?.`".into(),
                        span: lv.span,
                    });
                }
                self.verify_mutable_lvalue(base, ctx)
            }
            ExprKind::Index { expr: base, safe, .. } => {
                if *safe {
                    return Err(SemaError {
                        message: "cannot mutate through safe-index `?[]`".into(),
                        span: lv.span,
                    });
                }
                self.verify_mutable_lvalue(base, ctx)
            }
            _ => Err(SemaError {
                message: "invalid left-hand side in mutation `<-`".into(),
                span: lv.span,
            }),
        }
    }

    pub(crate) fn check_flow(
        &mut self,
        flow: &FlowKind,
        span: Span,
        ctx: &mut FnCheckCtx,
    ) -> Result<(), SemaError> {
        match flow {
            FlowKind::Return(opt) => {
                let expected = ctx.declared_ret.clone();
                let rty = match opt {
                    Some(e) => self.check_expr(e, expected.as_ref(), ctx)?,
                    None => Type::Unit,
                };
                if let Some(ref decl) = expected {
                    if !rty.is_assignable_to(decl) {
                        return Err(SemaError {
                            message: format!(
                                "return value of type `{}` does not match declared return type `{}`",
                                rty, decl
                            ),
                            span,
                        });
                    }
                }
                ctx.record_return_ty(rty, span)?;
            }
            FlowKind::LocalReturn(opt) => {
                let lty = match opt {
                    Some(e) => self.check_expr(e, None, ctx)?,
                    None => Type::Unit,
                };
                if let Some(slot) = ctx.pipe_ret_stack.last_mut() {
                    *slot = match slot.take() {
                        None => Some(lty),
                        Some(prev) => Type::unify(&prev, &lty).or(Some(lty)),
                    };
                } else {
                    ctx.record_return_ty(lty, span)?;
                }
            }
            FlowKind::LabeledReturn(lbl, opt) | FlowKind::LabeledBreak(lbl, opt) => {
                let vty = match opt {
                    Some(e) => self.check_expr(e, None, ctx)?,
                    None => Type::Unit,
                };
                if !ctx.record_label_ty(lbl, vty) {
                    return Err(SemaError {
                        message: format!("unknown block/loop label `#{}`", lbl),
                        span,
                    });
                }
            }
            FlowKind::Yield(e) => {
                let yty = self.check_expr(e, None, ctx)?;
                ctx.is_generator = true;
                ctx.yield_ty = Some(match ctx.yield_ty.take() {
                    None => yty,
                    Some(prev) => Type::unify(&prev, &yty).ok_or_else(|| SemaError {
                        message: "inconsistent types yielded by `^~` in generator".into(),
                        span,
                    })?,
                });
            }
            FlowKind::Throw(e) => {
                let _ = self.check_expr(e, None, ctx)?;
            }
            FlowKind::Break(opt) => {
                if ctx.loop_break_stack.is_empty() {
                    return Err(SemaError {
                        message: "`!@` break used outside of a `@` loop".into(),
                        span,
                    });
                }
                if let Some(e) = opt {
                    let bty = self.check_expr(e, None, ctx)?;
                    if let Some(slot) = ctx.loop_break_stack.last_mut() {
                        *slot = match slot.take() {
                            None => Some(bty),
                            Some(prev) => Type::unify(&prev, &bty).or(Some(bty)),
                        };
                    }
                }
            }
            FlowKind::Continue => {
                if ctx.loop_break_stack.is_empty() && ctx.pipe_ret_stack.is_empty() {
                    return Err(SemaError {
                        message: "`~@` continue used outside of a `@` loop".into(),
                        span,
                    });
                }
            }
            FlowKind::LabeledContinue(lbl) => {
                if !ctx.has_label(lbl) {
                    return Err(SemaError {
                        message: format!("unknown loop label `#{}`", lbl),
                        span,
                    });
                }
            }
        }
        Ok(())
    }

    fn check_cond_with_narrowing(
        &mut self,
        cond: &Expr,
        ctx: &mut FnCheckCtx,
    ) -> Result<Type, SemaError> {
        if let ExprKind::Binary {
            op: BinOp::And,
            lhs,
            rhs,
        } = &cond.kind
        {
            let left_narrow = extract_narrowings(lhs, &|n| ctx.lookup_var_effective_ty(n));
            let lty = self.check_cond_with_narrowing(lhs, ctx)?;
            if lty != Type::Bool {
                return Err(SemaError {
                    message: format!("`&` requires `bool` operands, found `{}`", lty),
                    span: lhs.span,
                });
            }
            ctx.push_narrowings(&left_narrow.when_true);
            let rty = self.check_cond_with_narrowing(rhs, ctx)?;
            ctx.pop_narrowings();
            if rty != Type::Bool {
                return Err(SemaError {
                    message: format!("`&` requires `bool` operands, found `{}`", rty),
                    span: rhs.span,
                });
            }
            ctx.record_expr_type(cond.span, Type::Bool);
            return Ok(Type::Bool);
        }
        if let ExprKind::Binary {
            op: BinOp::Or,
            lhs,
            rhs,
        } = &cond.kind
        {
            let left_narrow = extract_narrowings(lhs, &|n| ctx.lookup_var_effective_ty(n));
            let lty = self.check_cond_with_narrowing(lhs, ctx)?;
            if lty != Type::Bool {
                return Err(SemaError {
                    message: format!("`|` requires `bool` operands, found `{}`", lty),
                    span: lhs.span,
                });
            }
            ctx.push_narrowings(&left_narrow.when_false);
            let rty = self.check_cond_with_narrowing(rhs, ctx)?;
            ctx.pop_narrowings();
            if rty != Type::Bool {
                return Err(SemaError {
                    message: format!("`|` requires `bool` operands, found `{}`", rty),
                    span: rhs.span,
                });
            }
            ctx.record_expr_type(cond.span, Type::Bool);
            return Ok(Type::Bool);
        }
        self.check_expr(cond, Some(&Type::Bool), ctx)
    }

    pub(crate) fn check_expr(
        &mut self,
        expr: &Expr,
        expected: Option<&Type>,
        ctx: &mut FnCheckCtx,
    ) -> Result<Type, SemaError> {
        let ty = self.check_expr_inner(expr, expected, ctx)?;
        ctx.record_expr_type(expr.span, ty.clone());
        Ok(ty)
    }

    fn check_expr_inner(
        &mut self,
        expr: &Expr,
        expected: Option<&Type>,
        ctx: &mut FnCheckCtx,
    ) -> Result<Type, SemaError> {
        match &expr.kind {
            ExprKind::Unit => {
                if let Some(exp @ (Type::Array(_) | Type::Map(_, _))) = expected {
                    Ok(exp.clone())
                } else {
                    Ok(Type::Unit)
                }
            }
            ExprKind::Null => {
                if let Some(exp @ Type::Nullable(_)) = expected {
                    Ok(exp.clone())
                } else {
                    Ok(Type::Null)
                }
            }
            ExprKind::Bool(_) => Ok(Type::Bool),
            ExprKind::Int(_) => {
                if let Some(exp) = expected {
                    if exp.is_integer() {
                        return Ok(exp.clone());
                    }
                }
                Ok(Type::I64)
            }
            ExprKind::Float(_) => {
                if let Some(Type::F32) = expected {
                    return Ok(Type::F32);
                }
                Ok(Type::F64)
            }
            ExprKind::Char(_) => Ok(Type::Char),
            ExprKind::Str(_) => Ok(Type::Str),
            ExprKind::InterpString(segs) => {
                for seg in segs {
                    if let InterpSegment::Expr(e) = seg {
                        let _ = self.check_expr(e, None, ctx)?;
                    }
                }
                Ok(Type::Str)
            }
            ExprKind::Ident(name) => {
                if let Some(ty) = ctx.lookup_var_effective_ty(name) {
                    return Ok(ty);
                }
                if !ctx.module_prefix.is_empty() {
                    if let Some(mscope) = self.module_scopes.get(&ctx.module_prefix) {
                        if let Some(gty) = mscope.globals.get(name) {
                            return Ok(gty.clone());
                        }
                        if let Some(ext) = mscope.externs.get(name) {
                            return Ok(Type::Func {
                                params: ext.params.clone(),
                                ret: Box::new(ext.ret.clone()),
                            });
                        }
                        if let Some(mod_id) = mscope.aliases.get(name) {
                            return Ok(Type::Module(mod_id.clone()));
                        }
                    }
                    let scoped_fn = format!("{}__{}", ctx.module_prefix, name);
                    if let Some(tpl) = self.func_templates.get(&scoped_fn).cloned() {
                        if tpl.func.generics.is_empty() && tpl.func.params.iter().all(|p| p.ty.is_some()) {
                            let empty = HashMap::new();
                            let mut ptys = Vec::new();
                            for p in &tpl.func.params {
                                ptys.push(self.resolve_type_expr(p.ty.as_ref().unwrap(), &empty, expr.span)?);
                            }
                            let (c_name, ret_ty) = self.instantiate_template(&scoped_fn, None, &ptys, &[], expr.span)?;
                            ctx.call_targets.insert(span_key(expr.span), c_name);
                            return Ok(Type::Func { params: ptys, ret: Box::new(ret_ty) });
                        }
                    }
                }
                if let Some(gty) = self.global_types.get(name) {
                    return Ok(gty.clone());
                }
                if let Some(ext) = self.extern_map.get(name) {
                    return Ok(Type::Func {
                        params: ext.params.clone(),
                        ret: Box::new(ext.ret.clone()),
                    });
                }
                if let Some(mod_id) = self.module_aliases.get(name) {
                    return Ok(Type::Module(mod_id.clone()));
                }
                // Reference to concrete top-level function as value (e.g. `items ~> double_all` or `time.measure_ms(work)`)
                if let Some(tpl) = self.func_templates.get(name).cloned() {
                    if tpl.func.generics.is_empty() && tpl.func.params.iter().all(|p| p.ty.is_some()) {
                        let empty = HashMap::new();
                        let mut ptys = Vec::new();
                        for p in &tpl.func.params {
                            ptys.push(self.resolve_type_expr(p.ty.as_ref().unwrap(), &empty, expr.span)?);
                        }
                        let (c_name, ret_ty) = self.instantiate_template(name, None, &ptys, &[], expr.span)?;
                        ctx.call_targets.insert(span_key(expr.span), c_name);
                        return Ok(Type::Func { params: ptys, ret: Box::new(ret_ty) });
                    }
                }
                Err(SemaError {
                    message: format!("undefined identifier `{}`", name),
                    span: expr.span,
                })
            }
            ExprKind::TypeRef { name, generics } => {
                // Unit enum variant `Idle`
                if let Some((enum_name, v_idx)) = self.variant_lookup.get(name).cloned() {
                    let en = &self.enums[&enum_name];
                    if matches!(en.variants[v_idx].payload, CheckedVariantPayload::Unit) {
                        return Ok(Type::Enum(enum_name));
                    }
                }
                let resolved = self.resolve_type_expr(
                    &TypeExpr::Named {
                        name: name.clone(),
                        generics: generics.clone(),
                    },
                    &ctx.type_subst,
                    expr.span,
                )?;
                Ok(resolved)
            }
            ExprKind::TypeVal(te) => self.resolve_type_expr(te, &ctx.type_subst, expr.span),
            ExprKind::CurrentItem => {
                ctx.current_item_ty().ok_or_else(|| SemaError {
                    message: "`@` used outside of a pipeline `{ ... }`, method, or error handler"
                        .into(),
                    span: expr.span,
                })
            }
            ExprKind::SelfFn => ctx.self_fn_ty.clone().ok_or_else(|| SemaError {
                message: "`$` anonymous recursion used outside of a function".into(),
                span: expr.span,
            }),
            ExprKind::Array(elems) => {
                if elems.is_empty() {
                    if let Some(exp @ Type::Array(_)) = expected {
                        return Ok(exp.clone());
                    }
                    return Ok(Type::Array(Box::new(Type::I64)));
                }
                let exp_elem = match expected {
                    Some(Type::Array(el)) => Some(el.as_ref()),
                    _ => None,
                };
                let mut elem_types = Vec::with_capacity(elems.len());
                for e in elems {
                    elem_types.push(self.check_expr(e, exp_elem, ctx)?);
                }
                // Check if all elements unify into a homogeneous array `[T..]` (including `[Order, ~]` -> `[Order?..]`)
                let mut unified = elem_types[0].clone();
                let mut homogeneous = true;
                for next_t in &elem_types[1..] {
                    if let Some(u) = Type::unify(&unified, next_t) {
                        unified = u;
                    } else {
                        homogeneous = false;
                        break;
                    }
                }
                if homogeneous {
                    if let Some(exp_e) = exp_elem {
                        if unified.is_assignable_to(exp_e) {
                            return Ok(Type::Array(Box::new(exp_e.clone())));
                        }
                    }
                    Ok(Type::Array(Box::new(unified)))
                } else {
                    Ok(Type::Tuple(elem_types))
                }
            }
            ExprKind::RepeatArray {
                value,
                elem_ty,
                count,
            } => {
                let target_el = match elem_ty {
                    Some(te) => Some(self.resolve_type_expr(te, &ctx.type_subst, expr.span)?),
                    None => None,
                };
                let vty = self.check_expr(value, target_el.as_ref(), ctx)?;
                let final_el = if let Some(tel) = target_el {
                    if !vty.is_assignable_to(&tel) {
                        return Err(SemaError {
                            message: format!(
                                "repeat array element `{}` is not assignable to `{}`",
                                vty, tel
                            ),
                            span: value.span,
                        });
                    }
                    tel
                } else {
                    vty
                };
                let cty = self.check_expr(count, Some(&Type::I64), ctx)?;
                if !cty.is_integer() {
                    return Err(SemaError {
                        message: format!("repeat array length must be integer, found `{}`", cty),
                        span: count.span,
                    });
                }
                Ok(Type::Array(Box::new(final_el)))
            }
            ExprKind::SetLit(elems) => {
                if elems.is_empty() {
                    if let Some(exp @ Type::Set(_)) = expected {
                        return Ok(exp.clone());
                    }
                    return Ok(Type::Set(Box::new(Type::Str)));
                }
                let first_ty = self.check_expr(&elems[0], None, ctx)?;
                let mut acc_ty = first_ty;
                for e in &elems[1..] {
                    let ety = self.check_expr(e, Some(&acc_ty), ctx)?;
                    acc_ty = Type::unify(&acc_ty, &ety).ok_or_else(|| SemaError {
                        message: format!(
                            "set elements must have uniform type: `{}` vs `{}`",
                            acc_ty, ety
                        ),
                        span: e.span,
                    })?;
                }
                Ok(Type::Set(Box::new(acc_ty)))
            }
            ExprKind::MapLit(entries) => {
                if entries.is_empty() {
                    if let Some(exp @ Type::Map(_, _)) = expected {
                        return Ok(exp.clone());
                    }
                    return Ok(Type::Map(Box::new(Type::Str), Box::new(Type::I64)));
                }
                let (exp_k, exp_v) = match expected {
                    Some(Type::Map(k, v)) => (Some(k.as_ref()), Some(v.as_ref())),
                    _ => (None, None),
                };
                let mut kt = self.check_expr(&entries[0].0, exp_k, ctx)?;
                let mut vt = self.check_expr(&entries[0].1, exp_v, ctx)?;
                for (ke, ve) in &entries[1..] {
                    let k2 = self.check_expr(ke, Some(&kt), ctx)?;
                    let v2 = self.check_expr(ve, Some(&vt), ctx)?;
                    kt = Type::unify(&kt, &k2).ok_or_else(|| SemaError {
                        message: format!("incompatible map key types `{}` and `{}`", kt, k2),
                        span: ke.span,
                    })?;
                    vt = Type::unify(&vt, &v2).ok_or_else(|| SemaError {
                        message: format!("incompatible map value types `{}` and `{}`", vt, v2),
                        span: ve.span,
                    })?;
                }
                Ok(Type::Map(Box::new(kt), Box::new(vt)))
            }
            ExprKind::StructLit {
                type_name,
                generics,
                spread,
                fields,
            } => self.check_struct_or_variant_lit(
                type_name.as_deref(),
                generics,
                spread.as_deref(),
                fields,
                expr.span,
                ctx,
            ),
            ExprKind::Range { start, end, .. } => {
                let st = match start {
                    Some(s) => self.check_expr(s, Some(&Type::I64), ctx)?,
                    None => Type::I64,
                };
                let et = match end {
                    Some(e) => self.check_expr(e, Some(&st), ctx)?,
                    None => st.clone(),
                };
                if !st.is_integer() || !et.is_integer() {
                    return Err(SemaError {
                        message: format!(
                            "range bounds must be integers, found `{}` and `{}`",
                            st, et
                        ),
                        span: expr.span,
                    });
                }
                Ok(Type::Range(Box::new(Type::I64)))
            }
            ExprKind::Unary { op, expr: inner } => {
                let ity = self.check_expr(inner, None, ctx)?;
                if ity.is_nullable() {
                    return Err(SemaError {
                        message: format!(
                            "cannot apply unary `{:?}` to nullable type `{}`",
                            op, ity
                        ),
                        span: expr.span,
                    });
                }
                match op {
                    UnaryOp::Neg => {
                        if !ity.is_numeric() {
                            return Err(SemaError {
                                message: format!("unary `-` expects numeric type, found `{}`", ity),
                                span: expr.span,
                            });
                        }
                        Ok(ity)
                    }
                    UnaryOp::Not => {
                        if ity != Type::Bool {
                            return Err(SemaError {
                                message: format!("logical `!` expects `bool`, found `{}`", ity),
                                span: expr.span,
                            });
                        }
                        Ok(Type::Bool)
                    }
                    UnaryOp::BitNot => {
                        if !ity.is_integer() {
                            return Err(SemaError {
                                message: format!("bitwise `.~` expects integer, found `{}`", ity),
                                span: expr.span,
                            });
                        }
                        Ok(ity)
                    }
                    UnaryOp::Await => match ity {
                        Type::Task(inner_t) => Ok(*inner_t),
                        Type::Channel(inner_t) => Ok(Type::make_nullable(*inner_t)),
                        other => Err(SemaError {
                            message: format!(
                                "`<~` await/recv expects task `[~T]` or channel, found `{}`",
                                other
                            ),
                            span: expr.span,
                        }),
                    },
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                if matches!(op, BinOp::And | BinOp::Or) {
                    return self.check_cond_with_narrowing(expr, ctx);
                }
                let lty = self.check_expr(lhs, None, ctx)?;
                if *op == BinOp::ChanSend {
                    if let Type::Channel(elem_ty) = lty {
                        let rty = self.check_expr(rhs, Some(&elem_ty), ctx)?;
                        if !rty.is_assignable_to(&elem_ty) {
                            return Err(SemaError {
                                message: format!(
                                    "cannot send `{}` into channel of `{}`",
                                    rty, elem_ty
                                ),
                                span: rhs.span,
                            });
                        }
                        return Ok(Type::Unit);
                    }
                    return Err(SemaError {
                        message: format!(
                            "left side of `<~` send must be a channel, found `{}`",
                            lty
                        ),
                        span: lhs.span,
                    });
                }

                let rty = self.check_expr(rhs, Some(&lty), ctx)?;

                // Equality `==` and `!=` are allowed on nullable types (including comparison with `~`)
                if matches!(op, BinOp::Eq | BinOp::NotEq) {
                    if Type::unify(&lty, &rty).is_none() {
                        return Err(SemaError {
                            message: format!(
                                "cannot compare incompatible types `{}` and `{}`",
                                lty, rty
                            ),
                            span: expr.span,
                        });
                    }
                    return Ok(Type::Bool);
                }

                // All other binary operators forbid nullable operands!
                if lty.is_nullable() || rty.is_nullable() {
                    return Err(SemaError {
                        message: format!(
                            "cannot apply `{:?}` to nullable operands `{}` and `{}`; unwrap with `??`, `!!`, or smart cast first",
                            op, lty, rty
                        ),
                        span: expr.span,
                    });
                }

                match op {
                    BinOp::Add if lty == Type::Str && rty == Type::Str => Ok(Type::Str),
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                        if lty.is_numeric() && rty.is_numeric() {
                            if lty.is_float() || rty.is_float() {
                                if lty == Type::F32 && rty == Type::F32 {
                                    Ok(Type::F32)
                                } else {
                                    Ok(Type::F64)
                                }
                            } else if lty == rty {
                                Ok(lty)
                            } else {
                                Ok(Type::I64)
                            }
                        } else {
                            Err(SemaError {
                                message: format!(
                                    "arithmetic `{:?}` cannot be applied to `{}` and `{}`",
                                    op, lty, rty
                                ),
                                span: expr.span,
                            })
                        }
                    }
                    BinOp::BitAnd
                    | BinOp::BitOr
                    | BinOp::BitXor
                    | BinOp::BitShl
                    | BinOp::BitShr => {
                        if lty.is_integer() && rty.is_integer() {
                            Ok(lty)
                        } else {
                            Err(SemaError {
                                message: format!(
                                    "bitwise `{:?}` expects integers, found `{}` and `{}`",
                                    op, lty, rty
                                ),
                                span: expr.span,
                            })
                        }
                    }
                    BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => {
                        if (lty.is_numeric() && rty.is_numeric())
                            || (lty == Type::Str && rty == Type::Str)
                            || (lty == Type::Char && rty == Type::Char)
                        {
                            Ok(Type::Bool)
                        } else {
                            Err(SemaError {
                                message: format!(
                                    "comparison `{:?}` cannot compare `{}` and `{}`",
                                    op, lty, rty
                                ),
                                span: expr.span,
                            })
                        }
                    }
                    _ => unreachable!(),
                }
            }
            ExprKind::Field {
                expr: base,
                field,
                safe,
            } => {
                // Qualified enum unit variant `State.Idle`
                if let ExprKind::TypeRef { name: tname, .. } = &base.kind {
                    if let Some(en) = self.enums.get(tname) {
                        if let Some(v) = en.variants.iter().find(|v| &v.name == field) {
                            if matches!(v.payload, CheckedVariantPayload::Unit) {
                                return Ok(Type::Enum(tname.clone()));
                            }
                        }
                    }
                }

                let bty = self.check_expr(base, None, ctx)?;
                if bty.is_nullable() && !*safe {
                    return Err(SemaError {
                        message: format!(
                            "cannot access `.{}` on nullable type `{}`; use safe navigation `?.{}` or `!!`",
                            field, bty, field
                        ),
                        span: expr.span,
                    });
                }
                let unwrapped = bty.unwrap_nullable().unwrap_or(&bty).clone();
                let res_ty = self.resolve_field_access(&unwrapped, field, expr.span)?;
                if *safe || bty.is_nullable() {
                    Ok(Type::make_nullable(res_ty))
                } else {
                    Ok(res_ty)
                }
            }
            ExprKind::Index {
                expr: base,
                index,
                safe,
            } => {
                let bty = self.check_expr(base, None, ctx)?;
                if bty.is_nullable() && !*safe {
                    return Err(SemaError {
                        message: format!(
                            "cannot index nullable type `{}` with `[]`; use safe index `?[]` or `!!`",
                            bty
                        ),
                        span: expr.span,
                    });
                }
                let unwrapped = bty.unwrap_nullable().unwrap_or(&bty).clone();
                match unwrapped {
                    Type::Array(elem) => {
                        let ity = self.check_expr(index, Some(&Type::I64), ctx)?;
                        if !ity.is_integer() {
                            return Err(SemaError {
                                message: format!("array index must be integer, found `{}`", ity),
                                span: index.span,
                            });
                        }
                        if *safe || bty.is_nullable() {
                            Ok(Type::make_nullable(*elem))
                        } else {
                            Ok(*elem)
                        }
                    }
                    Type::Map(k_ty, v_ty) => {
                        let ity = self.check_expr(index, Some(&k_ty), ctx)?;
                        if !ity.is_assignable_to(&k_ty) {
                            return Err(SemaError {
                                message: format!(
                                    "map lookup key mismatch: expected `{}`, found `{}`",
                                    k_ty, ity
                                ),
                                span: index.span,
                            });
                        }
                        // Map lookup ALWAYS returns nullable `[V?]` per lo.md §5.4!
                        Ok(Type::make_nullable(*v_ty))
                    }
                    Type::Str => {
                        let ity = self.check_expr(index, Some(&Type::I64), ctx)?;
                        if !ity.is_integer() {
                            return Err(SemaError {
                                message: "string index must be integer".into(),
                                span: index.span,
                            });
                        }
                        if *safe || bty.is_nullable() {
                            Ok(Type::make_nullable(Type::Char))
                        } else {
                            Ok(Type::Char)
                        }
                    }
                    other => Err(SemaError {
                        message: format!("type `{}` cannot be indexed with `[]`", other),
                        span: expr.span,
                    }),
                }
            }
            ExprKind::Slice {
                expr: base,
                start,
                end,
                ..
            } => {
                let bty = self.check_expr(base, None, ctx)?;
                if bty.is_nullable() {
                    return Err(SemaError {
                        message: format!("cannot slice nullable value `{}`", bty),
                        span: expr.span,
                    });
                }
                if let Some(s) = start {
                    let st = self.check_expr(s, Some(&Type::I64), ctx)?;
                    if !st.is_integer() {
                        return Err(SemaError {
                            message: "slice start must be integer".into(),
                            span: s.span,
                        });
                    }
                }
                if let Some(e) = end {
                    let et = self.check_expr(e, Some(&Type::I64), ctx)?;
                    if !et.is_integer() {
                        return Err(SemaError {
                            message: "slice end must be integer".into(),
                            span: e.span,
                        });
                    }
                }
                match bty {
                    Type::Array(_) | Type::Str => Ok(bty),
                    other => Err(SemaError {
                        message: format!("cannot take slice of `{}`", other),
                        span: expr.span,
                    }),
                }
            }
            ExprKind::Call {
                callee,
                generics,
                args,
                safe,
            } => self.check_call(callee, generics, args, *safe, expr.span, ctx),
            ExprKind::Cast {
                expr: inner,
                ty: target_te,
                safe,
            } => {
                let src_ty = self.check_expr(inner, None, ctx)?;
                let dst_ty = self.resolve_type_expr(target_te, &ctx.type_subst, expr.span)?;
                if *safe {
                    Ok(Type::make_nullable(dst_ty))
                } else {
                    if !(src_ty.is_numeric() && dst_ty.is_numeric())
                        && !(src_ty == Type::Char && dst_ty.is_integer())
                        && !(src_ty.is_integer() && dst_ty == Type::Char)
                        && !src_ty.is_assignable_to(&dst_ty)
                    {
                        return Err(SemaError {
                            message: format!(
                                "invalid hard cast from `{}` to `{}`; use `?[{}]` for safe cast",
                                src_ty, dst_ty, dst_ty
                            ),
                            span: expr.span,
                        });
                    }
                    Ok(dst_ty)
                }
            }
            ExprKind::NonNullAssert(inner) => {
                let ity = self.check_expr(inner, None, ctx)?;
                match ity {
                    Type::Nullable(unwrapped) => Ok(*unwrapped),
                    Type::Null => Err(SemaError {
                        message: "`!!` applied directly to literal `~` will always panic".into(),
                        span: expr.span,
                    }),
                    other => Ok(other),
                }
            }
            ExprKind::TryPropagate(inner) => {
                let ity = self.check_expr(inner, None, ctx)?;
                Ok(ity.unwrap_nullable().cloned().unwrap_or(ity))
            }
            ExprKind::Elvis { lhs, rhs } => {
                let lty = self.check_expr(lhs, None, ctx)?;
                let base_ty = lty.unwrap_nullable().cloned().unwrap_or(lty);
                match rhs {
                    ElvisRhs::Flow(flow) => {
                        self.check_flow(flow, expr.span, ctx)?;
                        Ok(base_ty)
                    }
                    ElvisRhs::Expr(r_expr) => {
                        let rty = self.check_expr(r_expr, Some(&base_ty), ctx)?;
                        Type::unify(&base_ty, &rty).ok_or_else(|| SemaError {
                            message: format!(
                                "Elvis `??` branches have incompatible types `{}` and `{}`",
                                base_ty, rty
                            ),
                            span: expr.span,
                        })
                    }
                }
            }
            ExprKind::ErrorFallback { lhs, body } => {
                let lty = self.check_expr(lhs, None, ctx)?;
                let base_ty = lty.unwrap_nullable().cloned().unwrap_or(lty);
                ctx.push_current_item(Type::ErrorAny, false);
                let bty = self.check_block(body, ctx)?;
                ctx.pop_current_item();
                Type::unify(&base_ty, &bty).ok_or_else(|| SemaError {
                    message: format!(
                        "`?!` fallback block returns `{}`, incompatible with `{}`",
                        bty, base_ty
                    ),
                    span: expr.span,
                })
            }
            ExprKind::GuardReturn { cond, ret } => {
                let cty = self.check_cond_with_narrowing(cond, ctx)?;
                if cty != Type::Bool {
                    return Err(SemaError {
                        message: format!("guard `?^` condition must be `bool`, found `{}`", cty),
                        span: cond.span,
                    });
                }
                match ret {
                    ElvisRhs::Flow(flow) => {
                        self.check_flow(flow, expr.span, ctx)?;
                    }
                    ElvisRhs::Expr(r_expr) => {
                        let expected_ret = ctx.declared_ret.clone();
                        let rty = self.check_expr(r_expr, expected_ret.as_ref(), ctx)?;
                        if let Some(ref decl) = expected_ret {
                            if !rty.is_assignable_to(decl) {
                                return Err(SemaError {
                                    message: format!(
                                        "`?^` return value `{}` is not assignable to `{}`",
                                        rty, decl
                                    ),
                                    span: r_expr.span,
                                });
                            }
                        }
                        ctx.record_return_ty(rty, r_expr.span)?;
                    }
                }
                Ok(Type::Unit)
            }
            ExprKind::Pipe { lhs, kind, stage } => {
                self.check_pipe(lhs, *kind, stage, expr.span, ctx)
            }
            ExprKind::Func(fdef) => {
                // Local closure / lambda value
                let mut param_tys = Vec::new();
                for p in &fdef.params {
                    let pt = match &p.ty {
                        Some(te) => self.resolve_type_expr(te, &ctx.type_subst, p.span)?,
                        None => Type::I64,
                    };
                    param_tys.push(pt);
                }
                let key = format!("__anon_fn_{}_{}", expr.span.start, expr.span.end);
                self.func_templates.insert(
                    key.clone(),
                    FuncTemplate {
                        key_name: key.clone(),
                        receiver_type: None,
                        is_mutating_method: false,
                        func: (**fdef).clone(),
                        module_prefix: String::new(),
                    },
                );
                let (c_name, ret_ty) =
                    self.instantiate_template(&key, None, &param_tys, &[], expr.span)?;
                ctx.call_targets.insert(span_key(expr.span), c_name);
                Ok(Type::Func {
                    params: param_tys,
                    ret: Box::new(ret_ty),
                })
            }
            ExprKind::If {
                branches,
                else_block,
            } => {
                let mut accumulated_false: HashMap<String, Type> = HashMap::new();
                let mut result_ty: Option<Type> = None;

                for (cond, blk) in branches {
                    ctx.push_narrowings(&accumulated_false);
                    let narrow = extract_narrowings(cond, &|n| ctx.lookup_var_effective_ty(n));
                    let cty = self.check_cond_with_narrowing(cond, ctx)?;
                    if cty != Type::Bool {
                        ctx.pop_narrowings();
                        return Err(SemaError {
                            message: format!("if condition must be `bool`, found `{}`", cty),
                            span: cond.span,
                        });
                    }
                    ctx.push_narrowings(&narrow.when_true);
                    let bty = self.check_block(blk, ctx)?;
                    ctx.pop_narrowings();
                    ctx.pop_narrowings();

                    accumulated_false.extend(narrow.when_false);

                    result_ty = match result_ty {
                        None => Some(bty),
                        Some(prev) => Some(Type::unify(&prev, &bty).unwrap_or(Type::Unit)),
                    };
                }

                if let Some(eb) = else_block {
                    ctx.push_narrowings(&accumulated_false);
                    let ety = self.check_block(eb, ctx)?;
                    ctx.pop_narrowings();
                    let prev = result_ty.unwrap_or(Type::Unit);
                    Type::unify(&prev, &ety).ok_or_else(|| SemaError {
                        message: format!(
                            "incompatible branch types in `? (...)`: `{}` vs `{}`",
                            prev, ety
                        ),
                        span: expr.span,
                    })
                } else {
                    Ok(Type::Unit)
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                let sty = self.check_expr(scrutinee, None, ctx)?;
                let mut result_ty: Option<Type> = None;
                for arm in arms {
                    ctx.push_scope();
                    self.bind_pattern(&arm.pattern, &sty, arm.span, ctx)?;
                    if let Some(guard) = &arm.guard {
                        let gty = self.check_cond_with_narrowing(guard, ctx)?;
                        if gty != Type::Bool {
                            ctx.pop_scope();
                            return Err(SemaError {
                                message: format!("match guard must be `bool`, found `{}`", gty),
                                span: guard.span,
                            });
                        }
                    }
                    let aty = self.check_block(&arm.body, ctx)?;
                    ctx.pop_scope();
                    result_ty = match result_ty {
                        None => Some(aty),
                        Some(prev) => Some(Type::unify(&prev, &aty).ok_or_else(|| SemaError {
                            message: format!(
                                "match arms return incompatible types `{}` and `{}`",
                                prev, aty
                            ),
                            span: arm.span,
                        })?),
                    };
                }
                Ok(result_ty.unwrap_or(Type::Unit))
            }
            ExprKind::Try {
                body,
                catches,
                finally_block,
            } => {
                let mut res_ty = self.check_block(body, ctx)?;
                for catch in catches {
                    ctx.push_scope();
                    let cbody = match catch {
                        CatchClause::Pattern {
                            pattern,
                            body,
                            span,
                        } => {
                            self.bind_catch_pattern(pattern, *span, ctx)?;
                            body
                        }
                        CatchClause::Bind { name, body, span } => {
                            if name == "@" {
                                ctx.push_current_item(Type::ErrorAny, false);
                            } else {
                                ctx.define_var(name, Type::ErrorAny, false, *span)?;
                            }
                            body
                        }
                    };
                    let cty = self.check_block(cbody, ctx)?;
                    if matches!(catch, CatchClause::Bind { name, .. } if name == "@") {
                        ctx.pop_current_item();
                    }
                    ctx.pop_scope();
                    res_ty = Type::unify(&res_ty, &cty).ok_or_else(|| SemaError {
                        message: format!(
                            "`!{{}}` body (`{}`) and `?!` catch (`{}`) have incompatible types",
                            res_ty, cty
                        ),
                        span: expr.span,
                    })?;
                }
                if let Some(fin) = finally_block {
                    let _ = self.check_block(fin, ctx)?;
                }
                Ok(res_ty)
            }
            ExprKind::Loop { head, body } => {
                ctx.push_scope();
                match head {
                    LoopHead::Infinite => {}
                    LoopHead::While(cond) => {
                        let cty = self.check_cond_with_narrowing(cond, ctx)?;
                        if cty != Type::Bool {
                            ctx.pop_scope();
                            return Err(SemaError {
                                message: format!("`@ (...)` condition must be `bool`, found `{}`", cty),
                                span: cond.span,
                            });
                        }
                    }
                    LoopHead::ForEach { var, iter } => {
                        // Support `@ (item <- list)` AND `@ (msg <- <~ ch)`!
                        let elem_ty = if let ExprKind::Unary {
                            op: UnaryOp::Await,
                            expr: ch_expr,
                        } = &iter.kind
                        {
                            let chty = self.check_expr(ch_expr, None, ctx)?;
                            ctx.record_expr_type(iter.span, chty.clone());
                            match chty {
                                Type::Channel(inner) => *inner,
                                other => {
                                    ctx.pop_scope();
                                    return Err(SemaError {
                                        message: format!(
                                            "`@ (x <- <~ ch)` expects channel, found `{}`",
                                            other
                                        ),
                                        span: iter.span,
                                    });
                                }
                            }
                        } else {
                            let ity = self.check_expr(iter, None, ctx)?;
                            match ity {
                                Type::Array(el)
                                | Type::Range(el)
                                | Type::Set(el)
                                | Type::Generator(el) => *el,
                                other => {
                                    ctx.pop_scope();
                                    return Err(SemaError {
                                        message: format!(
                                            "`@ (x <- iter)` cannot iterate over `{}`",
                                            other
                                        ),
                                        span: iter.span,
                                    });
                                }
                            }
                        };
                        ctx.define_var(var, elem_ty, false, expr.span)?;
                    }
                }
                ctx.loop_break_stack.push(None);
                let _ = self.check_block(body, ctx)?;
                let break_val_ty = ctx.loop_break_stack.pop().flatten();
                ctx.pop_scope();
                if let Some(bty) = break_val_ty {
                    Ok(Type::make_nullable(bty))
                } else {
                    Ok(Type::Unit)
                }
            }
            ExprKind::Labeled { label, expr: inner } => {
                ctx.label_stack.push((label.clone(), None));
                let ity = self.check_expr(inner, expected, ctx)?;
                let (_, lbl_ty) = ctx.label_stack.pop().unwrap();
                if let Some(lt) = lbl_ty {
                    Ok(Type::unify(&ity, &lt).unwrap_or(lt))
                } else {
                    Ok(ity)
                }
            }
            ExprKind::BlockExpr(blk) => self.check_block(blk, ctx),
            ExprKind::AsyncSpawn(blk) => {
                let mut sub_ctx = FnCheckCtx::new(None, ctx.type_subst.clone());
                // Capture outer variables as immutable snapshots inside spawned green task
                for scope in &ctx.scopes {
                    for (k, v) in scope {
                        let _ = sub_ctx.define_var(k, v.ty.clone(), false, blk.span);
                    }
                }
                let bty = self.check_block(blk, &mut sub_ctx)?;
                let task_ret = if let Some(r) = sub_ctx.inferred_ret {
                    Type::unify(&bty, &r).unwrap_or(r)
                } else {
                    bty
                };
                ctx.expr_types.extend(sub_ctx.expr_types);
                ctx.call_targets.extend(sub_ctx.call_targets);
                ctx.struct_targets.extend(sub_ctx.struct_targets);
                Ok(Type::Task(Box::new(task_ret)))
            }
            ExprKind::Comptime(_) => Err(SemaError {
                message: "internal error: unfolded `<{ ... }>` comptime block".into(),
                span: expr.span,
            }),
            ExprKind::Import(_) | ExprKind::ExternC { .. } => Err(SemaError {
                message: "`<...>` import / FFI declarations are only valid at module top level"
                    .into(),
                span: expr.span,
            }),
        }
    }

    fn check_pipe(
        &mut self,
        lhs: &Expr,
        kind: PipeKind,
        stage: &PipeStage,
        span: Span,
        ctx: &mut FnCheckCtx,
    ) -> Result<Type, SemaError> {
        let lty = self.check_expr(lhs, None, ctx)?;

        // Case 1: Scalar safe pipe `nick ?~> { print(@) }` (`[T?]` -> `[U?]` or `[]`)
        if kind == PipeKind::Safe
            && !matches!(
                lty,
                Type::Array(_) | Type::Range(_) | Type::Generator(_) | Type::Set(_)
            )
        {
            let unwrapped = lty.unwrap_nullable().cloned().unwrap_or_else(|| lty.clone());
            if let PipeStage::Map { param, body } = stage {
                ctx.push_current_item(unwrapped.clone(), false);
                ctx.push_scope();
                if let Some(p) = param {
                    ctx.define_var(p, unwrapped, false, body.span)?;
                }
                ctx.pipe_ret_stack.push(None);
                let bty = self.check_block(body, ctx)?;
                let local_ret = ctx.pipe_ret_stack.pop().flatten();
                ctx.pop_scope();
                ctx.pop_current_item();
                let res = match local_ret {
                    Some(lr) => Type::unify(&bty, &lr).unwrap_or(lr),
                    None => bty,
                };
                return if res == Type::Unit {
                    Ok(Type::Unit)
                } else {
                    Ok(Type::make_nullable(res))
                };
            }
        }

        // Case 2: Stream / Array / Range / Generator pipeline
        let mut elem_ty = match lty {
            Type::Array(el) | Type::Range(el) | Type::Generator(el) | Type::Set(el) => *el,
            other => {
                return Err(SemaError {
                    message: format!(
                        "pipeline `{:?}` requires array, range, set, or generator (or nullable for `?~>`), found `{}`",
                        kind, other
                    ),
                    span: lhs.span,
                })
            }
        };

        // For `[T?..] ?~> { ... }` (lo.md line 835), filter out `~` elements and unwrap `@` to `T`!
        if kind == PipeKind::Safe {
            if let Some(inner) = elem_ty.unwrap_nullable() {
                elem_ty = inner.clone();
            }
        }

        if kind == PipeKind::Par {
            ctx.par_depth += 1;
        }

        let res = match stage {
            PipeStage::Map { param, body } => {
                ctx.push_current_item(elem_ty.clone(), false);
                ctx.push_scope();
                if let Some(p) = param {
                    ctx.define_var(p, elem_ty, false, body.span)?;
                }
                ctx.pipe_ret_stack.push(None);
                let bty = self.check_block(body, ctx)?;
                let local_ret = ctx.pipe_ret_stack.pop().flatten();
                ctx.pop_scope();
                ctx.pop_current_item();
                let stage_ty = match local_ret {
                    Some(lr) => Type::unify(&bty, &lr).unwrap_or(lr),
                    None => bty,
                };
                if stage_ty == Type::Unit {
                    Ok(Type::Unit)
                } else {
                    Ok(Type::Array(Box::new(stage_ty)))
                }
            }
            PipeStage::Filter { body } => {
                ctx.push_current_item(elem_ty.clone(), false);
                ctx.pipe_ret_stack.push(None);
                let bty = self.check_block(body, ctx)?;
                let _ = ctx.pipe_ret_stack.pop();
                ctx.pop_current_item();
                if bty != Type::Bool {
                    return Err(SemaError {
                        message: format!("filter stage `? {{ ... }}` must return `bool`, found `{}`", bty),
                        span,
                    });
                }
                Ok(Type::Array(Box::new(elem_ty)))
            }
            PipeStage::Fold {
                acc_name,
                init,
                body,
            } => {
                let init_ty = self.check_expr(init, None, ctx)?;
                ctx.push_current_item(elem_ty, false);
                ctx.push_scope();
                ctx.define_var(acc_name, init_ty.clone(), false, body.span)?;
                ctx.pipe_ret_stack.push(None);
                let bty = self.check_block(body, ctx)?;
                let local_ret = ctx.pipe_ret_stack.pop().flatten();
                ctx.pop_scope();
                ctx.pop_current_item();
                let next_ty = match local_ret {
                    Some(lr) => Type::unify(&bty, &lr).unwrap_or(lr),
                    None => bty,
                };
                Type::unify(&init_ty, &next_ty).ok_or_else(|| SemaError {
                    message: format!(
                        "fold accumulator type `{}` does not match body result `{}`",
                        init_ty, next_ty
                    ),
                    span,
                })
            }
            PipeStage::Expr(fn_expr) => {
                let fty = self.check_expr(fn_expr, None, ctx)?;
                if let Type::Func { ret, .. } = fty {
                    Ok(*ret)
                } else {
                    Err(SemaError {
                        message: format!("expected callable function in pipeline, found `{}`", fty),
                        span: fn_expr.span,
                    })
                }
            }
        };

        if kind == PipeKind::Par {
            ctx.par_depth -= 1;
        }
        res
    }

    pub(crate) fn bind_pattern(
        &mut self,
        pat: &Pattern,
        scrutinee_ty: &Type,
        span: Span,
        ctx: &mut FnCheckCtx,
    ) -> Result<(), SemaError> {
        let base_ty = scrutinee_ty.unwrap_nullable().unwrap_or(scrutinee_ty);
        match pat {
            Pattern::Wildcard | Pattern::Null => Ok(()),
            Pattern::Bool(_) => Ok(()),
            Pattern::Int(_) | Pattern::Float(_) | Pattern::Char(_) | Pattern::Str(_) => Ok(()),
            Pattern::Range { start, end, .. } => {
                let _ = self.check_expr(start, None, ctx)?;
                let _ = self.check_expr(end, None, ctx)?;
                Ok(())
            }
            Pattern::Ident(name) => {
                ctx.define_var(name, scrutinee_ty.clone(), false, span)
            }
            Pattern::Or(alts) => {
                if let Some(first) = alts.first() {
                    self.bind_pattern(first, scrutinee_ty, span, ctx)?;
                }
                Ok(())
            }
            Pattern::VariantTuple {
                type_name,
                subpatterns,
            } => {
                if let Type::Enum(en_name) = base_ty {
                    if let Some(en) = self.enums.get(en_name).cloned() {
                        if let Some(v) = en.variants.iter().find(|v| &v.name == type_name) {
                            if let CheckedVariantPayload::Tuple(tys) = &v.payload {
                                for (sp, ty) in subpatterns.iter().zip(tys.iter()) {
                                    self.bind_pattern(sp, ty, span, ctx)?;
                                }
                                return Ok(());
                            } else if subpatterns.is_empty() {
                                return Ok(());
                            }
                        }
                    }
                }
                for sp in subpatterns {
                    self.bind_pattern(sp, base_ty, span, ctx)?;
                }
                Ok(())
            }
            Pattern::Struct {
                type_name,
                fields,
                ..
            } => {
                if let Type::Enum(en_name) = base_ty {
                    if let Some(tn) = type_name {
                        if let Some(en) = self.enums.get(en_name).cloned() {
                            if let Some(v) = en.variants.iter().find(|v| &v.name == tn) {
                                if let CheckedVariantPayload::Struct(vfs) = &v.payload {
                                    for (fname, fpat) in fields {
                                        if let Some(vf) = vfs.iter().find(|f| &f.name == fname) {
                                            self.bind_pattern(fpat, &vf.ty, span, ctx)?;
                                        }
                                    }
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
                let struct_name = type_name.clone().or_else(|| match base_ty {
                    Type::Struct(sn) => Some(sn.clone()),
                    _ => None,
                });
                if let Some(sn) = struct_name {
                    if let Some(st) = self.structs.get(&sn).cloned() {
                        for (fname, fpat) in fields {
                            let fty = st
                                .fields
                                .iter()
                                .find(|f| &f.name == fname)
                                .map(|f| f.ty.clone())
                                .ok_or_else(|| SemaError {
                                    message: format!("struct `{}` has no field `{}`", sn, fname),
                                    span,
                                })?;
                            self.bind_pattern(fpat, &fty, span, ctx)?;
                        }
                        return Ok(());
                    }
                }
                Ok(())
            }
        }
    }

    fn bind_catch_pattern(
        &mut self,
        pat: &Pattern,
        span: Span,
        ctx: &mut FnCheckCtx,
    ) -> Result<(), SemaError> {
        match pat {
            Pattern::VariantTuple {
                type_name,
                subpatterns,
            } => {
                // e.g. `?! [AuthErr(e)]` where `AuthErr` is a struct error type!
                if self.structs.contains_key(type_name) {
                    if let Some(Pattern::Ident(var)) = subpatterns.first() {
                        ctx.define_var(var, Type::Struct(type_name.clone()), false, span)?;
                    }
                    return Ok(());
                }
                self.bind_pattern(pat, &Type::ErrorAny, span, ctx)
            }
            Pattern::Struct {
                type_name: Some(tn),
                fields,
                ..
            } => {
                if let Some(st) = self.structs.get(tn).cloned() {
                    for (fname, fpat) in fields {
                        if let Some(f) = st.fields.iter().find(|f| &f.name == fname) {
                            self.bind_pattern(fpat, &f.ty, span, ctx)?;
                        }
                    }
                }
                Ok(())
            }
            _ => self.bind_pattern(pat, &Type::ErrorAny, span, ctx),
        }
    }
}

pub(crate) struct FnCheckCtx {
    pub module_prefix: String,
    pub scopes: Vec<HashMap<String, VarInfo>>,
    pub narrow_stack: Vec<HashMap<String, Type>>,
    pub current_items: Vec<(Type, bool)>,
    pub declared_ret: Option<Type>,
    pub inferred_ret: Option<Type>,
    pub self_fn_ty: Option<Type>,
    pub is_generator: bool,
    pub yield_ty: Option<Type>,
    pub par_depth: usize,
    pub loop_break_stack: Vec<Option<Type>>,
    pub pipe_ret_stack: Vec<Option<Type>>,
    pub label_stack: Vec<(String, Option<Type>)>,
    pub type_subst: HashMap<String, Type>,
    pub expr_types: HashMap<SpanKey, Type>,
    pub call_targets: HashMap<SpanKey, String>,
    pub struct_targets: HashMap<SpanKey, String>,
}

impl FnCheckCtx {
    pub fn new(declared_ret: Option<Type>, type_subst: HashMap<String, Type>) -> Self {
        Self {
            module_prefix: String::new(),
            scopes: vec![HashMap::new()],
            narrow_stack: vec![HashMap::new()],
            current_items: Vec::new(),
            declared_ret,
            inferred_ret: None,
            self_fn_ty: None,
            is_generator: false,
            yield_ty: None,
            par_depth: 0,
            loop_break_stack: Vec::new(),
            pipe_ret_stack: Vec::new(),
            label_stack: Vec::new(),
            type_subst,
            expr_types: HashMap::new(),
            call_targets: HashMap::new(),
            struct_targets: HashMap::new(),
        }
    }

    pub fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.narrow_stack.push(HashMap::new());
    }

    pub fn pop_scope(&mut self) {
        self.scopes.pop();
        self.narrow_stack.pop();
    }

    pub fn push_narrowings(&mut self, map: &HashMap<String, Type>) {
        self.narrow_stack.push(map.clone());
    }

    pub fn pop_narrowings(&mut self) {
        self.narrow_stack.pop();
    }

    pub fn apply_block_narrowings(&mut self, map: &HashMap<String, Type>) {
        if let Some(top) = self.narrow_stack.last_mut() {
            top.extend(map.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
    }

    pub fn clear_narrowing(&mut self, name: &str) {
        for layer in &mut self.narrow_stack {
            layer.remove(name);
        }
    }

    pub fn define_var(
        &mut self,
        name: &str,
        ty: Type,
        is_mut: bool,
        span: Span,
    ) -> Result<(), SemaError> {
        // Forbid accidental shadowing in the same lexical scope (`lo.md` §3)
        if let Some(cur) = self.scopes.last() {
            if cur.contains_key(name) {
                return Err(SemaError {
                    message: format!(
                        "variable `{}` is already bound in this scope (shadowing is forbidden)",
                        name
                    ),
                    span,
                });
            }
        }
        let info = VarInfo {
            c_name: format!("lo_v_{}", sanitize_ident(name)),
            ty,
            is_mut,
            par_depth: self.par_depth,
        };
        if let Some(cur) = self.scopes.last_mut() {
            cur.insert(name.to_string(), info);
        }
        Ok(())
    }

    pub fn lookup_var_raw(&self, name: &str) -> Option<&VarInfo> {
        for scope in self.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v);
            }
        }
        None
    }

    pub fn lookup_var_effective_ty(&self, name: &str) -> Option<Type> {
        for layer in self.narrow_stack.iter().rev() {
            if let Some(nty) = layer.get(name) {
                return Some(nty.clone());
            }
        }
        self.lookup_var_raw(name).map(|v| v.ty.clone())
    }

    pub fn push_current_item(&mut self, ty: Type, is_mut: bool) {
        self.current_items.push((ty, is_mut));
    }

    pub fn pop_current_item(&mut self) {
        self.current_items.pop();
    }

    pub fn current_item_ty(&self) -> Option<Type> {
        self.current_items.last().map(|(t, _)| t.clone())
    }

    pub fn current_item_is_mut(&self) -> bool {
        self.current_items
            .last()
            .map(|(_, m)| *m)
            .unwrap_or(false)
    }

    pub fn record_expr_type(&mut self, span: Span, ty: Type) {
        self.expr_types.insert(span_key(span), ty);
    }

    pub fn record_return_ty(&mut self, ty: Type, span: Span) -> Result<(), SemaError> {
        if ty == Type::Never {
            return Ok(());
        }
        self.inferred_ret = Some(match self.inferred_ret.take() {
            None => ty,
            Some(prev) => Type::unify(&prev, &ty).ok_or_else(|| SemaError {
                message: format!(
                    "conflicting return types `{}` and `{}` in function",
                    prev, ty
                ),
                span,
            })?,
        });
        Ok(())
    }

    pub fn has_label(&self, lbl: &str) -> bool {
        self.label_stack.iter().any(|(l, _)| l == lbl)
    }

    pub fn record_label_ty(&mut self, lbl: &str, ty: Type) -> bool {
        for (l, slot) in self.label_stack.iter_mut().rev() {
            if l == lbl {
                *slot = match slot.take() {
                    None => Some(ty),
                    Some(prev) => Type::unify(&prev, &ty).or(Some(ty)),
                };
                return true;
            }
        }
        false
    }
}
