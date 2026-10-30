use crate::codegen::c_types::{c_default_zero, c_type_name};
use crate::codegen::emit_expr::{
    escape_c_str, DeferEntry, FnEmitCtx, LabelTarget, LoopTarget, PipeLocalTarget,
};
use crate::lexer::Span;
use crate::parser::*;
use crate::sema::{sanitize_ident, CheckedVariantPayload, Type};
use std::fmt::Write;

impl<'a> FnEmitCtx<'a> {
    pub fn emit_defers_from(&mut self, out: &mut String, start_idx: usize, is_err_exit: bool) {
        if self.masking_defers || self.defers.len() <= start_idx {
            return;
        }
        let entries = self.defers[start_idx..].to_vec();
        self.masking_defers = true;
        let saved_err = self.fresh_tmp("derr_save");
        let _ = writeln!(
            out,
            "    lo_error_t {} = _lo_err; _lo_err.active = false;",
            saved_err
        );
        for entry in entries.iter().rev() {
            if entry.is_errdefer {
                let cond = if is_err_exit {
                    entry.flag_var.clone()
                } else {
                    format!("({} && {}.active)", entry.flag_var, saved_err)
                };
                let _ = writeln!(out, "    if ({}) {{", cond);
                let _ = writeln!(out, "        {} = false;", entry.flag_var);
                let _ = self.emit_block(&entry.body, &Type::Unit, out);
                let _ = writeln!(out, "    }}");
            } else {
                let _ = writeln!(out, "    if ({}) {{", entry.flag_var);
                let _ = writeln!(out, "        {} = false;", entry.flag_var);
                let _ = self.emit_block(&entry.body, &Type::Unit, out);
                let _ = writeln!(out, "    }}");
            }
        }
        let _ = writeln!(out, "    _lo_err = {};", saved_err);
        self.masking_defers = false;
    }

    pub fn emit_active_defers(&mut self, out: &mut String, is_err_exit: bool) {
        self.emit_defers_from(out, 0, is_err_exit);
    }

    pub fn emit_error_unwind(&mut self, out: &mut String) {
        if let Some((catch_lbl, defer_base)) = self.try_catch_stack.last().cloned() {
            self.emit_defers_from(out, defer_base, true);
            let _ = writeln!(out, "        goto {};", catch_lbl);
        } else {
            self.emit_active_defers(out, true);
            if self.is_generator_worker {
                let _ = writeln!(out, "        return;");
            } else {
                let _ = writeln!(out, "        return {};", c_default_zero(&self.ret_ty));
            }
        }
    }

    pub fn emit_block(&mut self, block: &Block, expected_ty: &Type, out: &mut String) -> String {
        self.push_scope();
        let mut last_val = "lo_unit_val()".to_string();
        let mut last_ty = Type::Unit;
        let n = block.stmts.len();

        for (idx, stmt) in block.stmts.iter().enumerate() {
            let is_last = idx + 1 == n;
            // Part 1 shorthand: `? (i == 50) { ~ }` standalone `~` inside a unit loop block -> `continue`!
            if is_last
                && *expected_ty == Type::Unit
                && !self.loop_stack.is_empty()
                && matches!(&stmt.kind, StmtKind::Expr(e) if matches!(e.kind, ExprKind::Null))
            {
                let cont_lbl = self.loop_stack.last().unwrap().cont_label.clone();
                let _ = writeln!(out, "    goto {};", cont_lbl);
                last_val = "lo_unit_val()".into();
                last_ty = Type::Never;
                continue;
            }

            let (sv, sty) = self.emit_stmt(stmt, out);
            if is_last {
                last_val = sv;
                last_ty = sty;
            }
        }

        self.pop_scope();
        if *expected_ty == Type::Unit {
            "lo_unit_val()".into()
        } else if last_ty == Type::Never {
            c_default_zero(expected_ty)
        } else {
            self.coerce(&last_val, &last_ty, expected_ty, out)
        }
    }

    pub fn emit_stmt(&mut self, stmt: &Stmt, out: &mut String) -> (String, Type) {
        match &stmt.kind {
            StmtKind::Bind { target, value, .. } => {
                let vv = self.emit_expr(value, out);
                let vty = self.expr_ty(value.span);
                match target {
                    BindTarget::Single { name, ty } => {
                        let var_ty = self
                            .expr_types
                            .get(&crate::sema::span_key(stmt.span))
                            .cloned()
                            .unwrap_or_else(|| {
                                if ty.is_some() {
                                    self.resolve_declared_bind_ty(ty.as_ref(), &vty)
                                } else {
                                    vty.clone()
                                }
                            });
                        let coerced = self.coerce(&vv, &vty, &var_ty, out);
                        let c_var = self.fresh_tmp(&format!("v_{}", sanitize_ident(name)));
                        let _ =
                            writeln!(out, "    {} {} = {};", c_type_name(&var_ty), c_var, coerced);
                        self.bind_local(name, c_var, var_ty, false);
                    }
                    BindTarget::Destruct(names) => match &vty {
                        Type::Tuple(elems) => {
                            let ttmp = self.fresh_tmp("dtup");
                            let _ = writeln!(out, "    {} {} = {};", c_type_name(&vty), ttmp, vv);
                            for (i, (n, ety)) in names.iter().zip(elems.iter()).enumerate() {
                                if n == "_" {
                                    continue;
                                }
                                let c_var = self.fresh_tmp(&format!("v_{}", sanitize_ident(n)));
                                let _ = writeln!(
                                    out,
                                    "    {} {} = {}.t{};",
                                    c_type_name(ety),
                                    c_var,
                                    ttmp,
                                    i
                                );
                                self.bind_local(n, c_var, ety.clone(), false);
                            }
                        }
                        Type::Array(elem_ty) => {
                            let atmp = self.fresh_tmp("darr");
                            let _ = writeln!(out, "    lo_vec* {} = {};", atmp, vv);
                            for (i, n) in names.iter().enumerate() {
                                if n == "_" {
                                    continue;
                                }
                                let c_var = self.fresh_tmp(&format!("v_{}", sanitize_ident(n)));
                                let _ = writeln!(
                                    out,
                                    "    {} {} = *({}*)lo_vec_at({}, {}, {});",
                                    c_type_name(elem_ty),
                                    c_var,
                                    c_type_name(elem_ty),
                                    atmp,
                                    i,
                                    stmt.span.line
                                );
                                self.bind_local(n, c_var, (**elem_ty).clone(), false);
                            }
                        }
                        _ => {}
                    },
                }
                ("lo_unit_val()".into(), Type::Unit)
            }
            StmtKind::Mutate { target, op, value } => {
                match target {
                    MutTarget::Destruct(names) => {
                        let vv = self.emit_expr(value, out);
                        let vty = self.expr_ty(value.span);
                        match &vty {
                            Type::Tuple(elems) => {
                                let ttmp = self.fresh_tmp("mtup");
                                let _ =
                                    writeln!(out, "    {} {} = {};", c_type_name(&vty), ttmp, vv);
                                for (i, (n, ety)) in names.iter().zip(elems.iter()).enumerate() {
                                    if n == "_" {
                                        continue;
                                    }
                                    if let Some((c_var, dst_ty, is_ptr)) =
                                        self.lookup_local(n).cloned()
                                    {
                                        let lhs = if is_ptr {
                                            format!("(*{})", c_var)
                                        } else {
                                            c_var
                                        };
                                        let co = self.coerce(
                                            &format!("{}.t{}", ttmp, i),
                                            ety,
                                            &dst_ty,
                                            out,
                                        );
                                        let _ = writeln!(out, "    {} = {};", lhs, co);
                                    }
                                }
                            }
                            Type::Array(elem_ty) => {
                                let atmp = self.fresh_tmp("marr");
                                let _ = writeln!(out, "    lo_vec* {} = {};", atmp, vv);
                                for (i, n) in names.iter().enumerate() {
                                    if n == "_" {
                                        continue;
                                    }
                                    if let Some((c_var, dst_ty, is_ptr)) =
                                        self.lookup_local(n).cloned()
                                    {
                                        let lhs = if is_ptr {
                                            format!("(*{})", c_var)
                                        } else {
                                            c_var
                                        };
                                        let raw_el = format!(
                                            "(*({}*)lo_vec_at({}, {}, {}))",
                                            c_type_name(elem_ty),
                                            atmp,
                                            i,
                                            stmt.span.line
                                        );
                                        let co = self.coerce(&raw_el, elem_ty, &dst_ty, out);
                                        let _ = writeln!(out, "    {} = {};", lhs, co);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    MutTarget::LValue(lv) => {
                        // Map key update `cart["banana"] <- 3`
                        if let ExprKind::Index {
                            expr: base,
                            index,
                            safe: false,
                        } = &lv.kind
                        {
                            let bty = self.expr_ty(base.span);
                            if let Type::Map(k_ty, v_ty) = bty {
                                let mv = self.emit_expr(base, out);
                                let kv = self.emit_expr(index, out);
                                let vv = self.emit_expr(value, out);
                                let kraw = self.expr_ty(index.span);
                                let vraw = self.expr_ty(value.span);
                                let kco = self.coerce(&kv, &kraw, &k_ty, out);
                                let vco = self.coerce(&vv, &vraw, &v_ty, out);
                                let kt = self.fresh_tmp("mk");
                                let vt = self.fresh_tmp("mv");
                                let _ =
                                    writeln!(out, "    {} {} = {};", c_type_name(&k_ty), kt, kco);
                                let _ =
                                    writeln!(out, "    {} {} = {};", c_type_name(&v_ty), vt, vco);
                                let _ = writeln!(out, "    lo_map_set({}, &{}, &{});", mv, kt, vt);
                                return ("lo_unit_val()".into(), Type::Unit);
                            }
                        }

                        let lptr = self.emit_lvalue_addr(lv, out);
                        let lty = self.expr_ty(lv.span);
                        let rv = self.emit_expr(value, out);
                        let rty = self.expr_ty(value.span);

                        match op {
                            MutOp::Assign => {
                                let rco = self.coerce(&rv, &rty, &lty, out);
                                let _ = writeln!(out, "    *({}) = {};", lptr, rco);
                            }
                            MutOp::Add if lty == Type::Str => {
                                let ptr_tmp = self.fresh_tmp("sptr");
                                let _ = writeln!(out, "    lo_str* {} = {};", ptr_tmp, lptr);
                                let _ = writeln!(
                                    out,
                                    "    *{} = lo_str_concat(*{}, {});",
                                    ptr_tmp, ptr_tmp, rv
                                );
                            }
                            MutOp::Add => {
                                let _ = writeln!(out, "    *({}) += {};", lptr, rv);
                            }
                            MutOp::Sub => {
                                let _ = writeln!(out, "    *({}) -= {};", lptr, rv);
                            }
                            MutOp::Mul => {
                                let _ = writeln!(out, "    *({}) *= {};", lptr, rv);
                            }
                            MutOp::Div => {
                                let _ = writeln!(out, "    *({}) /= {};", lptr, rv);
                            }
                        }
                    }
                }
                ("lo_unit_val()".into(), Type::Unit)
            }
            StmtKind::Defer { is_errdefer, body } => {
                let flag = self.fresh_tmp("defer_on");
                let _ = writeln!(out, "    bool {} = true;", flag);
                self.defers.push(DeferEntry {
                    flag_var: flag,
                    is_errdefer: *is_errdefer,
                    body: body.clone(),
                });
                ("lo_unit_val()".into(), Type::Unit)
            }
            StmtKind::Flow(flow) => {
                self.emit_flow(flow, stmt.span, out);
                ("lo_unit_val()".into(), Type::Never)
            }
            StmtKind::Expr(e) => {
                let ty = self.expr_ty(e.span);
                let val = self.emit_expr(e, out);
                if ty != Type::Unit && ty != Type::Never {
                    let sink = self.fresh_tmp("unused");
                    let _ = writeln!(
                        out,
                        "    {} {} = {}; (void){};",
                        c_type_name(&ty),
                        sink,
                        val,
                        sink
                    );
                    (sink, ty)
                } else {
                    (val, ty)
                }
            }
            _ => ("lo_unit_val()".into(), Type::Unit),
        }
    }

    fn resolve_declared_bind_ty(&self, te: Option<&TypeExpr>, fallback_val_ty: &Type) -> Type {
        let Some(t) = te else {
            return fallback_val_ty.clone();
        };
        match t {
            TypeExpr::Nullable(inner) => {
                let inner_resolved = self.resolve_declared_bind_ty(Some(inner), fallback_val_ty);
                Type::make_nullable(inner_resolved)
            }
            TypeExpr::Array(inner) => {
                if let Type::Array(fb_el) = fallback_val_ty {
                    Type::Array(Box::new(self.resolve_declared_bind_ty(Some(inner), fb_el)))
                } else {
                    Type::Array(Box::new(self.resolve_declared_bind_ty(Some(inner), &Type::I64)))
                }
            }
            TypeExpr::Named { name, generics } if generics.is_empty() => match name.as_str() {
                "bool" => Type::Bool,
                "i8" => Type::I8,
                "i16" => Type::I16,
                "i32" => Type::I32,
                "i64" => Type::I64,
                "u8" => Type::U8,
                "u16" => Type::U16,
                "u32" => Type::U32,
                "u64" => Type::U64,
                "usz" => Type::Usz,
                "f32" => Type::F32,
                "f64" => Type::F64,
                "char" => Type::Char,
                "str" => Type::Str,
                other => {
                    if self.prog.structs.contains_key(other) {
                        Type::Struct(other.to_string())
                    } else if self.prog.enums.contains_key(other) {
                        Type::Enum(other.to_string())
                    } else {
                        fallback_val_ty.clone()
                    }
                }
            },
            _ => fallback_val_ty.clone(),
        }
    }

    pub fn emit_flow(&mut self, flow: &FlowKind, span: Span, out: &mut String) {
        let _ = span;
        match flow {
            FlowKind::Return(opt) => {
                if self.is_generator_worker {
                    self.emit_active_defers(out, false);
                    let _ = writeln!(out, "    return;");
                    return;
                }
                let ret_ty = self.ret_ty.clone();
                if let Some(e) = opt {
                    let rv = self.emit_expr(e, out);
                    let rty = self.expr_ty(e.span);
                    let rco = self.coerce(&rv, &rty, &ret_ty, out);
                    let rtmp = self.fresh_tmp("retv");
                    let _ = writeln!(out, "    {} {} = {};", c_type_name(&ret_ty), rtmp, rco);
                    self.emit_active_defers(out, false);
                    let _ = writeln!(out, "    return {};", rtmp);
                } else {
                    self.emit_active_defers(out, false);
                    let _ = writeln!(out, "    return {};", c_default_zero(&ret_ty));
                }
            }
            FlowKind::LocalReturn(opt) => {
                if let Some(pt) = self.pipe_stack.last().cloned() {
                    if let (Some(e), Some((res_v, res_t))) = (opt, pt.res_var) {
                        let v = self.emit_expr(e, out);
                        let vt = self.expr_ty(e.span);
                        let co = self.coerce(&v, &vt, &res_t, out);
                        let _ = writeln!(out, "    {} = {};", res_v, co);
                    }
                    let _ = writeln!(out, "    goto {};", pt.cont_label);
                } else {
                    self.emit_flow(&FlowKind::Return(opt.clone()), span, out);
                }
            }
            FlowKind::LabeledReturn(lbl, opt) => {
                if let Some(lt) = self
                    .label_stack
                    .iter()
                    .rev()
                    .find(|l| &l.name == lbl)
                    .cloned()
                {
                    if let Some(e) = opt {
                        let v = self.emit_expr(e, out);
                        let vt = self.expr_ty(e.span);
                        let co = self.coerce(&v, &vt, &lt.res_var.1, out);
                        let _ = writeln!(out, "    {} = {};", lt.res_var.0, co);
                    }
                    let _ = writeln!(out, "    goto {};", lt.end_label);
                }
            }
            FlowKind::Yield(e) => {
                let yty = self.yield_ty.clone().unwrap_or(Type::I64);
                let yv = self.emit_expr(e, out);
                let yraw = self.expr_ty(e.span);
                let yco = self.coerce(&yv, &yraw, &yty, out);
                let _ = writeln!(out, "    if (!_yield_cb({}, _yield_ctx)) {{", yco);
                self.emit_active_defers(out, false);
                let _ = writeln!(out, "        return;");
                let _ = writeln!(out, "    }}");
            }
            FlowKind::Throw(e) => {
                let ev = self.emit_expr(e, out);
                let ety = self.expr_ty(e.span);
                let etmp = self.fresh_tmp("thr");
                let _ = writeln!(out, "    {} {} = {};", c_type_name(&ety), etmp, ev);
                let (type_id, code_expr, msg_expr) = match &ety {
                    Type::Struct(sn) => {
                        if let Some(st) = self.prog.structs.get(sn) {
                            let c_e = if st.fields.iter().any(|f| f.name == "code") {
                                format!("(int64_t)({}.f_code)", etmp)
                            } else {
                                "1".into()
                            };
                            let m_e = if st.fields.iter().any(|f| f.name == "msg") {
                                format!("{}.f_msg", etmp)
                            } else if st.fields.iter().any(|f| f.name == "reason") {
                                format!("{}.f_reason", etmp)
                            } else {
                                format!("lo_str_lit({}, {})", escape_c_str(sn), sn.len())
                            };
                            (st.type_id, c_e, m_e)
                        } else {
                            (0, "1".into(), "lo_str_lit(\"error\", 5)".into())
                        }
                    }
                    Type::Str => (0, "1".into(), etmp.clone()),
                    _ => {
                        let s = self.emit_to_str(&etmp, &ety, out);
                        (0, "1".into(), s)
                    }
                };
                let _ = writeln!(
                    out,
                    "    lo_throw_val({}u, {}, {}, &{}, sizeof({}));",
                    type_id, code_expr, msg_expr, etmp, etmp
                );
                self.emit_error_unwind(out);
            }
            FlowKind::Break(opt) => {
                if let Some(lt) = self.loop_stack.last().cloned() {
                    if let (Some(e), Some((res_v, res_t))) = (opt, lt.break_res_var) {
                        let bv = self.emit_expr(e, out);
                        let bty = self.expr_ty(e.span);
                        let bco = self.coerce(&bv, &bty, &res_t, out);
                        let _ = writeln!(out, "    {} = {};", res_v, bco);
                    }
                    let _ = writeln!(out, "    goto {};", lt.break_label);
                }
            }
            FlowKind::LabeledBreak(lbl, opt) => {
                if let Some(lt) = self
                    .label_stack
                    .iter()
                    .rev()
                    .find(|l| &l.name == lbl)
                    .cloned()
                {
                    if let Some(e) = opt {
                        let bv = self.emit_expr(e, out);
                        let bty = self.expr_ty(e.span);
                        let bco = self.coerce(&bv, &bty, &lt.res_var.1, out);
                        let _ = writeln!(out, "    {} = {};", lt.res_var.0, bco);
                    }
                    let _ = writeln!(out, "    goto {};", lt.end_label);
                }
            }
            FlowKind::Continue => {
                if let Some(lt) = self.loop_stack.last().cloned() {
                    let _ = writeln!(out, "    goto {};", lt.cont_label);
                } else if let Some(pt) = self.pipe_stack.last().cloned() {
                    let _ = writeln!(out, "    goto {};", pt.cont_label);
                }
            }
            FlowKind::LabeledContinue(lbl) => {
                if let Some(lt) = self
                    .label_stack
                    .iter()
                    .rev()
                    .find(|l| &l.name == lbl)
                    .cloned()
                {
                    if let Some(cl) = lt.cont_label {
                        let _ = writeln!(out, "    goto {};", cl);
                    }
                }
            }
        }
    }

    pub fn emit_if(
        &mut self,
        branches: &[(Expr, Block)],
        else_block: Option<&Block>,
        ety: &Type,
        out: &mut String,
    ) -> String {
        let res = self.fresh_tmp("ifres");
        let end_lbl = self.fresh_tmp("ifend");
        let _ = writeln!(
            out,
            "    {} {} = {};",
            c_type_name(ety),
            res,
            c_default_zero(ety)
        );

        for (cond, blk) in branches {
            let next_lbl = self.fresh_tmp("ifnext");
            let cv = self.emit_expr(cond, out);
            let _ = writeln!(out, "    if (!({})) goto {};", cv, next_lbl);
            let bv = self.emit_block(blk, ety, out);
            if *ety != Type::Unit {
                let _ = writeln!(out, "    {} = {};", res, bv);
            }
            let _ = writeln!(out, "    goto {};", end_lbl);
            let _ = writeln!(out, "    {}: ;", next_lbl);
        }

        if let Some(eb) = else_block {
            let ev = self.emit_block(eb, ety, out);
            if *ety != Type::Unit {
                let _ = writeln!(out, "    {} = {};", res, ev);
            }
        }
        let _ = writeln!(out, "    {}: ;", end_lbl);
        res
    }

    pub fn emit_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        ety: &Type,
        out: &mut String,
    ) -> String {
        let sv = self.emit_expr(scrutinee, out);
        let sty = self.expr_ty(scrutinee.span);
        let stmp = self.fresh_tmp("mscrut");
        let res = self.fresh_tmp("mres");
        let end_lbl = self.fresh_tmp("mend");

        let _ = writeln!(out, "    {} {} = {};", c_type_name(&sty), stmp, sv);
        let _ = writeln!(
            out,
            "    {} {} = {};",
            c_type_name(ety),
            res,
            c_default_zero(ety)
        );

        for arm in arms {
            let next_arm = self.fresh_tmp("mnext");
            self.push_scope();
            let _ = writeln!(out, "    {{");
            let cond = self.emit_pattern_check(&arm.pattern, &stmp, &sty, out);
            let _ = writeln!(out, "        if (!({})) goto {};", cond, next_arm);
            if let Some(guard) = &arm.guard {
                let gv = self.emit_expr(guard, out);
                let _ = writeln!(out, "        if (!({})) goto {};", gv, next_arm);
            }
            let bv = self.emit_block(&arm.body, ety, out);
            if *ety != Type::Unit {
                let _ = writeln!(out, "        {} = {};", res, bv);
            }
            let _ = writeln!(out, "        goto {};", end_lbl);
            let _ = writeln!(out, "    }}");
            self.pop_scope();
            let _ = writeln!(out, "    {}: ;", next_arm);
        }

        let _ = writeln!(out, "    {}: ;", end_lbl);
        res
    }

    fn emit_pattern_check(
        &mut self,
        pat: &Pattern,
        scrut_c: &str,
        scrut_ty: &Type,
        out: &mut String,
    ) -> String {
        match pat {
            Pattern::Wildcard => "true".into(),
            Pattern::Null => {
                if scrut_ty.is_nullable() {
                    format!("(!({}).has_val)", scrut_c)
                } else {
                    "false".into()
                }
            }
            Pattern::Ident(name) => {
                let c_var = self.fresh_tmp(&format!("v_{}", sanitize_ident(name)));
                let _ = writeln!(
                    out,
                    "        {} {} = {};",
                    c_type_name(scrut_ty),
                    c_var,
                    scrut_c
                );
                self.bind_local(name, c_var, scrut_ty.clone(), false);
                "true".into()
            }
            Pattern::Bool(b) => {
                let lit = if *b { "true" } else { "false" };
                self.emit_eq(scrut_c, lit, scrut_ty, &Type::Bool, out)
            }
            Pattern::Int(n) => {
                let lit = format!("INT64_C({})", n);
                self.emit_eq(scrut_c, &lit, scrut_ty, &Type::I64, out)
            }
            Pattern::Float(x) => {
                let lit = format!("{:?}", x);
                self.emit_eq(scrut_c, &lit, scrut_ty, &Type::F64, out)
            }
            Pattern::Char(c) => {
                let lit = format!("UINT32_C({})", *c as u32);
                self.emit_eq(scrut_c, &lit, scrut_ty, &Type::Char, out)
            }
            Pattern::Str(s) => {
                let lit = format!("lo_str_lit({}, {})", escape_c_str(s), s.len());
                self.emit_eq(scrut_c, &lit, scrut_ty, &Type::Str, out)
            }
            Pattern::Range {
                start,
                end,
                inclusive,
            } => {
                let sv = self.emit_expr(start, out);
                let ev = self.emit_expr(end, out);
                let val_c = if scrut_ty.is_nullable() {
                    format!("({}).val", scrut_c)
                } else {
                    scrut_c.to_string()
                };
                let prefix = if scrut_ty.is_nullable() {
                    format!("({}).has_val && ", scrut_c)
                } else {
                    String::new()
                };
                let cmp = if *inclusive { "<=" } else { "<" };
                format!(
                    "({}{} >= ({}) && {} {} ({}))",
                    prefix, val_c, sv, val_c, cmp, ev
                )
            }
            Pattern::Or(alts) => {
                let mut parts = Vec::with_capacity(alts.len());
                for a in alts {
                    parts.push(self.emit_pattern_check(a, scrut_c, scrut_ty, out));
                }
                format!("({})", parts.join(" || "))
            }
            Pattern::VariantTuple {
                type_name,
                subpatterns,
            } => {
                let (base_c, base_ty, non_null_guard) = if let Type::Nullable(inner) = scrut_ty {
                    (
                        format!("({}).val", scrut_c),
                        (**inner).clone(),
                        Some(format!("({}).has_val", scrut_c)),
                    )
                } else {
                    (scrut_c.to_string(), scrut_ty.clone(), None)
                };

                if let Type::Enum(ref ename) = base_ty {
                    if let Some(en) = self.prog.enums.get(ename).cloned() {
                        if let Some(v) = en.variants.iter().find(|v| &v.name == type_name) {
                            let ok_tmp = self.fresh_tmp("vok");
                            let init_cond = match non_null_guard {
                                Some(g) => format!("({} && ({}).tag == {}u)", g, base_c, v.tag),
                                None => format!("(({}).tag == {}u)", base_c, v.tag),
                            };
                            let _ = writeln!(out, "        bool {} = {};", ok_tmp, init_cond);
                            if let CheckedVariantPayload::Tuple(ref vtys) = v.payload {
                                // Pre-declare pattern bound variables with zero defaults before `if (ok_tmp)` so scope covers body!
                                self.predeclare_pattern_vars_list(subpatterns, vtys, out);
                                let _ = writeln!(out, "        if ({}) {{", ok_tmp);
                                for (i, (sp, vt)) in subpatterns.iter().zip(vtys.iter()).enumerate()
                                {
                                    let field_c = format!("({}).u.{}.v{}", base_c, type_name, i);
                                    let sub_ok =
                                        self.emit_pattern_assign_existing(sp, &field_c, vt, out);
                                    let _ = writeln!(
                                        out,
                                        "            {} = {} && ({});",
                                        ok_tmp, ok_tmp, sub_ok
                                    );
                                }
                                let _ = writeln!(out, "        }}");
                            }
                            return ok_tmp;
                        }
                    }
                }
                "false".into()
            }
            Pattern::Struct {
                type_name, fields, ..
            } => {
                let (base_c, base_ty, non_null_guard) = if let Type::Nullable(inner) = scrut_ty {
                    (
                        format!("({}).val", scrut_c),
                        (**inner).clone(),
                        Some(format!("({}).has_val", scrut_c)),
                    )
                } else {
                    (scrut_c.to_string(), scrut_ty.clone(), None)
                };

                // Struct-payload Enum Variant `Attack[dmg = d, ..]` or `Click[x = 0, y = 0]`
                if let Type::Enum(ref ename) = base_ty {
                    if let Some(tn) = type_name {
                        if let Some(en) = self.prog.enums.get(ename).cloned() {
                            if let Some(v) = en.variants.iter().find(|v| &v.name == tn) {
                                let ok_tmp = self.fresh_tmp("esok");
                                let init_cond = match non_null_guard {
                                    Some(g) => format!("({} && ({}).tag == {}u)", g, base_c, v.tag),
                                    None => format!("(({}).tag == {}u)", base_c, v.tag),
                                };
                                let _ = writeln!(out, "        bool {} = {};", ok_tmp, init_cond);
                                if let CheckedVariantPayload::Struct(ref vfs) = v.payload {
                                    for (fname, fpat) in fields {
                                        if let Some(vf) = vfs.iter().find(|f| &f.name == fname) {
                                            self.predeclare_pattern_vars(fpat, &vf.ty, out);
                                        }
                                    }
                                    let _ = writeln!(out, "        if ({}) {{", ok_tmp);
                                    for (fname, fpat) in fields {
                                        if let Some(vf) = vfs.iter().find(|f| &f.name == fname) {
                                            let fc = format!(
                                                "({}).u.{}.f_{}",
                                                base_c,
                                                tn,
                                                sanitize_ident(fname)
                                            );
                                            let sub_ok = self
                                                .emit_pattern_assign_existing(fpat, &fc, &vf.ty, out);
                                            let _ = writeln!(
                                                out,
                                                "            {} = {} && ({});",
                                                ok_tmp, ok_tmp, sub_ok
                                            );
                                        }
                                    }
                                    let _ = writeln!(out, "        }}");
                                }
                                return ok_tmp;
                            }
                        }
                    }
                }

                // Plain Struct Pattern `Click[x, y]`
                if let Type::Struct(ref sname) = base_ty {
                    if let Some(st) = self.prog.structs.get(sname).cloned() {
                        let ok_tmp = self.fresh_tmp("sok");
                        let init_cond = non_null_guard.unwrap_or_else(|| "true".into());
                        let _ = writeln!(out, "        bool {} = {};", ok_tmp, init_cond);
                        for (fname, fpat) in fields {
                            if let Some(sf) = st.fields.iter().find(|f| &f.name == fname) {
                                self.predeclare_pattern_vars(fpat, &sf.ty, out);
                            }
                        }
                        let _ = writeln!(out, "        if ({}) {{", ok_tmp);
                        for (fname, fpat) in fields {
                            if let Some(sf) = st.fields.iter().find(|f| &f.name == fname) {
                                let fc = format!("({}).f_{}", base_c, sanitize_ident(fname));
                                let sub_ok =
                                    self.emit_pattern_assign_existing(fpat, &fc, &sf.ty, out);
                                let _ = writeln!(
                                    out,
                                    "            {} = {} && ({});",
                                    ok_tmp, ok_tmp, sub_ok
                                );
                            }
                        }
                        let _ = writeln!(out, "        }}");
                        return ok_tmp;
                    }
                }
                "false".into()
            }
        }
    }

    fn predeclare_pattern_vars_list(&mut self, pats: &[Pattern], tys: &[Type], out: &mut String) {
        for (p, t) in pats.iter().zip(tys.iter()) {
            self.predeclare_pattern_vars(p, t, out);
        }
    }

    fn predeclare_pattern_vars(&mut self, pat: &Pattern, ty: &Type, out: &mut String) {
        if let Pattern::Ident(name) = pat {
            let c_var = self.fresh_tmp(&format!("v_{}", sanitize_ident(name)));
            let _ = writeln!(
                out,
                "        {} {} = {};",
                c_type_name(ty),
                c_var,
                c_default_zero(ty)
            );
            self.bind_local(name, c_var, ty.clone(), false);
        }
    }

    fn emit_pattern_assign_existing(
        &mut self,
        pat: &Pattern,
        val_c: &str,
        ty: &Type,
        out: &mut String,
    ) -> String {
        if let Pattern::Ident(name) = pat {
            if let Some((c_var, _, _)) = self.lookup_local(name).cloned() {
                let _ = writeln!(out, "            {} = {};", c_var, val_c);
                return "true".into();
            }
        }
        self.emit_pattern_check(pat, val_c, ty, out)
    }

    pub fn emit_try(
        &mut self,
        body: &Block,
        catches: &[CatchClause],
        finally_block: Option<&Block>,
        ety: &Type,
        out: &mut String,
    ) -> String {
        let res = self.fresh_tmp("tryres");
        let catch_lbl = self.fresh_tmp("trycatch");
        let ok_lbl = self.fresh_tmp("tryok");

        let _ = writeln!(
            out,
            "    {} {} = {};",
            c_type_name(ety),
            res,
            c_default_zero(ety)
        );

        let defer_base = self.defers.len();
        self.try_catch_stack.push((catch_lbl.clone(), defer_base));
        let bv = self.emit_block(body, ety, out);
        self.try_catch_stack.pop();

        if *ety != Type::Unit {
            let _ = writeln!(out, "    {} = {};", res, bv);
        }
        let _ = writeln!(out, "    goto {};", ok_lbl);

        let _ = writeln!(out, "    {}: ;", catch_lbl);
        let _ = writeln!(out, "    {{");
        let err_var = self.fresh_tmp("caught_err");
        let handled = self.fresh_tmp("handled");
        let _ = writeln!(out, "        lo_error_t {} = _lo_err;", err_var);
        let _ = writeln!(out, "        bool {} = false;", handled);

        for catch in catches {
            let next_c = self.fresh_tmp("cnext");
            self.push_scope();
            let _ = writeln!(out, "        if (!{}) {{", handled);
            match catch {
                CatchClause::Bind { name, body: cbody, .. } => {
                    let _ = writeln!(out, "            {} = true;", handled);
                    let _ = writeln!(out, "            _lo_err.payload = NULL; lo_err_clear();");
                    if name == "@" {
                        self.current_item_stack
                            .push((err_var.clone(), Type::ErrorAny, false));
                    } else {
                        let cv = self.fresh_tmp(&format!("v_{}", sanitize_ident(name)));
                        let _ = writeln!(out, "            lo_error_t {} = {};", cv, err_var);
                        self.bind_local(name, cv, Type::ErrorAny, false);
                    }
                    let cbv = self.emit_block(cbody, ety, out);
                    if name == "@" {
                        self.current_item_stack.pop();
                    }
                    if *ety != Type::Unit {
                        let _ = writeln!(out, "            {} = {};", res, cbv);
                    }
                }
                CatchClause::Pattern {
                    pattern,
                    body: cbody,
                    ..
                } => {
                    let cond = self.emit_catch_pattern_match(pattern, &err_var, out);
                    let _ = writeln!(out, "            if (!({})) goto {};", cond, next_c);
                    let _ = writeln!(out, "            {} = true;", handled);
                    let _ = writeln!(out, "            _lo_err.payload = NULL; lo_err_clear();");
                    let cbv = self.emit_block(cbody, ety, out);
                    if *ety != Type::Unit {
                        let _ = writeln!(out, "            {} = {};", res, cbv);
                    }
                }
            }
            let _ = writeln!(out, "        }}");
            let _ = writeln!(out, "        {}: ;", next_c);
            self.pop_scope();
        }

        // Run `?>` finally even if error was NOT handled before rethrowing!
        let _ = writeln!(out, "        if (!{}) {{", handled);
        if let Some(fin) = finally_block {
            self.masking_defers = true;
            let _ = self.emit_block(fin, &Type::Unit, out);
            self.masking_defers = false;
        }
        self.emit_error_unwind(out);
        let _ = writeln!(out, "        }}");
        let _ = writeln!(out, "    }}");

        let _ = writeln!(out, "    {}: ;", ok_lbl);
        if let Some(fin) = finally_block {
            let _ = self.emit_block(fin, &Type::Unit, out);
        }
        res
    }

    fn emit_catch_pattern_match(
        &mut self,
        pat: &Pattern,
        err_var: &str,
        out: &mut String,
    ) -> String {
        match pat {
            Pattern::VariantTuple {
                type_name,
                subpatterns,
            } => {
                if let Some(st) = self.prog.structs.get(type_name).cloned() {
                    let ok = self.fresh_tmp("cok");
                    let _ = writeln!(
                        out,
                        "            bool {} = ({}.type_id == {}u && {}.payload != NULL);",
                        ok, err_var, st.type_id, err_var
                    );
                    if let Some(Pattern::Ident(var_n)) = subpatterns.first() {
                        let cv = self.fresh_tmp(&format!("v_{}", sanitize_ident(var_n)));
                        let _ = writeln!(
                            out,
                            "            lo_st_{} {} = {{0}};",
                            type_name, cv
                        );
                        let _ = writeln!(
                            out,
                            "            if ({}) {} = *(lo_st_{}*){}.payload;",
                            ok, cv, type_name, err_var
                        );
                        self.bind_local(var_n, cv, Type::Struct(type_name.clone()), false);
                    }
                    return ok;
                }
                "false".into()
            }
            Pattern::Struct {
                type_name: Some(tn),
                fields,
                ..
            } => {
                if let Some(st) = self.prog.structs.get(tn).cloned() {
                    let ok = self.fresh_tmp("csok");
                    let ptr = self.fresh_tmp("csptr");
                    let _ = writeln!(
                        out,
                        "            bool {} = ({}.type_id == {}u && {}.payload != NULL);",
                        ok, err_var, st.type_id, err_var
                    );
                    for (fname, fpat) in fields {
                        if let Some(sf) = st.fields.iter().find(|f| &f.name == fname) {
                            self.predeclare_pattern_vars(fpat, &sf.ty, out);
                        }
                    }
                    let _ = writeln!(out, "            if ({}) {{", ok);
                    let _ = writeln!(
                        out,
                        "                lo_st_{}* {} = (lo_st_{}*){}.payload;",
                        tn, ptr, tn, err_var
                    );
                    for (fname, fpat) in fields {
                        if let Some(sf) = st.fields.iter().find(|f| &f.name == fname) {
                            let fc = format!("{}->f_{}", ptr, sanitize_ident(fname));
                            let sub_ok =
                                self.emit_pattern_assign_existing(fpat, &fc, &sf.ty, out);
                            let _ = writeln!(
                                out,
                                "                {} = {} && ({});",
                                ok, ok, sub_ok
                            );
                        }
                    }
                    let _ = writeln!(out, "            }}");
                    return ok;
                }
                "false".into()
            }
            _ => "true".into(),
        }
    }

    pub fn emit_labeled(
        &mut self,
        label: &str,
        inner: &Expr,
        ety: &Type,
        out: &mut String,
    ) -> String {
        if let ExprKind::Loop { head, body } = &inner.kind {
            return self.emit_loop(head, body, ety, Some(label), out);
        }
        let res = self.fresh_tmp("lblres");
        let end_lbl = self.fresh_tmp(&format!("lbl_end_{}", sanitize_ident(label)));
        let _ = writeln!(
            out,
            "    {} {} = {};",
            c_type_name(ety),
            res,
            c_default_zero(ety)
        );
        self.label_stack.push(LabelTarget {
            name: label.to_string(),
            end_label: end_lbl.clone(),
            cont_label: None,
            res_var: (res.clone(), ety.clone()),
        });
        let iv = self.emit_expr(inner, out);
        let ity = self.expr_ty(inner.span);
        if *ety != Type::Unit && ity != Type::Never {
            let co = self.coerce(&iv, &ity, ety, out);
            let _ = writeln!(out, "    {} = {};", res, co);
        }
        self.label_stack.pop();
        let _ = writeln!(out, "    {}: ;", end_lbl);
        res
    }

    pub fn emit_loop(
        &mut self,
        head: &LoopHead,
        body: &Block,
        ety: &Type,
        opt_label: Option<&str>,
        out: &mut String,
    ) -> String {
        let res = self.fresh_tmp("loopres");
        let brk_lbl = self.fresh_tmp("lbrk");
        let cnt_lbl = self.fresh_tmp("lcnt");

        let init_zero = if ety.is_nullable() {
            format!("(({}){{ .has_val = false }})", c_type_name(ety))
        } else {
            c_default_zero(ety)
        };
        let _ = writeln!(out, "    {} {} = {};", c_type_name(ety), res, init_zero);

        let break_res_var = if *ety != Type::Unit {
            Some((res.clone(), ety.clone()))
        } else {
            None
        };

        self.loop_stack.push(LoopTarget {
            break_label: brk_lbl.clone(),
            cont_label: cnt_lbl.clone(),
            break_res_var,
        });
        if let Some(lbl) = opt_label {
            self.label_stack.push(LabelTarget {
                name: lbl.to_string(),
                end_label: brk_lbl.clone(),
                cont_label: Some(cnt_lbl.clone()),
                res_var: (res.clone(), ety.clone()),
            });
        }

        match head {
            LoopHead::Infinite => {
                let _ = writeln!(out, "    for (;;) {{");
                let _ = self.emit_block(body, &Type::Unit, out);
                let _ = writeln!(out, "        {}: ;", cnt_lbl);
                let _ = writeln!(out, "    }}");
            }
            LoopHead::While(cond) => {
                let _ = writeln!(out, "    for (;;) {{");
                let cv = self.emit_expr(cond, out);
                let _ = writeln!(out, "        if (!({})) break;", cv);
                let _ = self.emit_block(body, &Type::Unit, out);
                let _ = writeln!(out, "        {}: ;", cnt_lbl);
                let _ = writeln!(out, "    }}");
            }
            LoopHead::ForEach { var, iter } => {
                // Check `@ (msg <- <~ ch)` channel loop!
                if let ExprKind::Unary {
                    op: UnaryOp::Await,
                    expr: ch_expr,
                } = &iter.kind
                {
                    let ch_v = self.emit_expr(ch_expr, out);
                    let ch_ty = self.expr_ty(ch_expr.span);
                    let elem_ty = match ch_ty {
                        Type::Channel(el) => *el,
                        _ => Type::I64,
                    };
                    let ch_tmp = self.fresh_tmp("lch");
                    let _ = writeln!(out, "    lo_chan_t* {} = {};", ch_tmp, ch_v);
                    let _ = writeln!(out, "    for (;;) {{");
                    self.push_scope();
                    let v_c = self.fresh_tmp(&format!("v_{}", sanitize_ident(var)));
                    let _ = writeln!(
                        out,
                        "        {} {} = {};",
                        c_type_name(&elem_ty),
                        v_c,
                        c_default_zero(&elem_ty)
                    );
                    let _ = writeln!(
                        out,
                        "        if (!lo_chan_recv({}, &{})) break;",
                        ch_tmp, v_c
                    );
                    self.bind_local(var, v_c, elem_ty, false);
                    let _ = self.emit_block(body, &Type::Unit, out);
                    self.pop_scope();
                    let _ = writeln!(out, "        {}: ;", cnt_lbl);
                    let _ = writeln!(out, "    }}");
                } else {
                    let it_ty = self.expr_ty(iter.span);
                    if let Type::Generator(yield_ty) = it_ty {
                        let stage = PipeStage::Map {
                            param: Some(var.clone()),
                            body: body.clone(),
                        };
                        let _ = self.emit_generator_pipe(
                            iter,
                            &yield_ty,
                            PipeKind::Seq,
                            &stage,
                            &Type::Unit,
                            out,
                        );
                    } else {
                        let it_v = self.emit_expr(iter, out);
                        match it_ty {
                            Type::Range(_) => {
                                let rtmp = self.fresh_tmp("lrng");
                                let idx = self.fresh_tmp("li");
                                let _ = writeln!(out, "    lo_range_i64_t {} = {};", rtmp, it_v);
                                let _ = writeln!(
                                    out,
                                    "    for (int64_t {0} = {1}.start; {1}.inclusive ? ({0} <= {1}.end) : ({0} < {1}.end); {0}++) {{",
                                    idx, rtmp
                                );
                                self.push_scope();
                                self.bind_local(var, idx, Type::I64, false);
                                let _ = self.emit_block(body, &Type::Unit, out);
                                self.pop_scope();
                                let _ = writeln!(out, "        {}: ;", cnt_lbl);
                                let _ = writeln!(out, "    }}");
                            }
                            Type::Array(elem_ty) => {
                                let vtmp = self.fresh_tmp("lvec");
                                let idx = self.fresh_tmp("li");
                                let _ = writeln!(out, "    lo_vec* {} = {};", vtmp, it_v);
                                let _ = writeln!(
                                    out,
                                    "    for (int64_t {0} = 0; {1} && {0} < {1}->len; {0}++) {{",
                                    idx, vtmp
                                );
                                self.push_scope();
                                let el_var = self.fresh_tmp(&format!("v_{}", sanitize_ident(var)));
                                let _ = writeln!(
                                    out,
                                    "        {} {} = *({}*)lo_vec_at({}, {}, 0);",
                                    c_type_name(&elem_ty),
                                    el_var,
                                    c_type_name(&elem_ty),
                                    vtmp,
                                    idx
                                );
                                self.bind_local(var, el_var, (*elem_ty).clone(), false);
                                let _ = self.emit_block(body, &Type::Unit, out);
                                self.pop_scope();
                                let _ = writeln!(out, "        {}: ;", cnt_lbl);
                                let _ = writeln!(out, "    }}");
                            }
                            Type::Set(elem_ty) => {
                                let stmp = self.fresh_tmp("lset");
                                let idx = self.fresh_tmp("lsi");
                                let _ = writeln!(out, "    lo_map* {} = {};", stmp, it_v);
                                let _ = writeln!(
                                    out,
                                    "    for (size_t {0} = 0; {1} && {0} < {1}->cap; {0}++) {{",
                                    idx, stmp
                                );
                                let _ = writeln!(
                                    out,
                                    "        if (!{}->entries[{}].occupied) continue;",
                                    stmp, idx
                                );
                                self.push_scope();
                                let el_var = self.fresh_tmp(&format!("v_{}", sanitize_ident(var)));
                                let _ = writeln!(
                                    out,
                                    "        {} {} = *({}*){}->entries[{}].key;",
                                    c_type_name(&elem_ty),
                                    el_var,
                                    c_type_name(&elem_ty),
                                    stmp,
                                    idx
                                );
                                self.bind_local(var, el_var, (*elem_ty).clone(), false);
                                let _ = self.emit_block(body, &Type::Unit, out);
                                self.pop_scope();
                                let _ = writeln!(out, "        {}: ;", cnt_lbl);
                                let _ = writeln!(out, "    }}");
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        if opt_label.is_some() {
            self.label_stack.pop();
        }
        self.loop_stack.pop();
        let _ = writeln!(out, "    {}: ;", brk_lbl);
        res
    }

    pub fn emit_pipe(
        &mut self,
        lhs: &Expr,
        kind: PipeKind,
        stage: &PipeStage,
        span: Span,
        ety: &Type,
        out: &mut String,
    ) -> String {
        let lty = self.expr_ty(lhs.span);

        // 1. Scalar Safe Pipe `nick ?~> { ... }`
        if kind == PipeKind::Safe
            && !matches!(
                lty,
                Type::Array(_) | Type::Range(_) | Type::Generator(_) | Type::Set(_)
            )
        {
            let lv = self.emit_expr(lhs, out);
            let ltmp = self.fresh_tmp("sp_in");
            let res = self.fresh_tmp("sp_out");
            let done_lbl = self.fresh_tmp("sp_done");
            let _ = writeln!(out, "    {} {} = {};", c_type_name(&lty), ltmp, lv);
            let _ = writeln!(
                out,
                "    {} {} = {};",
                c_type_name(ety),
                res,
                if ety.is_nullable() {
                    format!("(({}){{ .has_val = false }})", c_type_name(ety))
                } else {
                    c_default_zero(ety)
                }
            );
            let cond = if lty.is_nullable() {
                format!("{}.has_val", ltmp)
            } else {
                "true".into()
            };
            let item_c = if lty.is_nullable() {
                format!("{}.val", ltmp)
            } else {
                ltmp
            };
            let item_ty = lty.unwrap_nullable().cloned().unwrap_or(lty);

            if let PipeStage::Map { param, body } = stage {
                let _ = writeln!(out, "    if ({}) {{", cond);
                self.current_item_stack
                    .push((item_c.clone(), item_ty.clone(), false));
                self.push_scope();
                if let Some(p) = param {
                    let pv = self.fresh_tmp(&format!("v_{}", sanitize_ident(p)));
                    let _ = writeln!(out, "        {} {} = {};", c_type_name(&item_ty), pv, item_c);
                    self.bind_local(p, pv, item_ty, false);
                }
                self.pipe_stack.push(PipeLocalTarget {
                    cont_label: done_lbl.clone(),
                    res_var: if *ety != Type::Unit {
                        Some((res.clone(), ety.clone()))
                    } else {
                        None
                    },
                });
                let inner_exp = ety.unwrap_nullable().unwrap_or(ety).clone();
                let bv = self.emit_block(body, &inner_exp, out);
                self.pipe_stack.pop();
                self.pop_scope();
                self.current_item_stack.pop();
                if *ety != Type::Unit {
                    let co = self.coerce(&bv, &inner_exp, ety, out);
                    let _ = writeln!(out, "        {} = {};", res, co);
                }
                let _ = writeln!(out, "        {}: ;", done_lbl);
                let _ = writeln!(out, "    }}");
            }
            return res;
        }

        // 2. Generator lazy callback consumption (`fibonacci(1000) ~> { print(@) }` — ZERO array alloc!)
        if let Type::Generator(ref yield_ty) = lty {
            return self.emit_generator_pipe(lhs, yield_ty, kind, stage, ety, out);
        }

        // 3. Multi-core Parallel Pipeline (`lhs ~>> { ... }`)
        if kind == PipeKind::Par {
            return self.emit_parallel_pipe(lhs, &lty, stage, ety, out);
        }

        // 4. Loop-Fused Sequential / Stream-Safe Pipeline (`~>`, `?~>`)
        let lv = self.emit_expr(lhs, out);
        let raw_elem_ty = match &lty {
            Type::Array(el) | Type::Range(el) | Type::Set(el) => (**el).clone(),
            _ => Type::I64,
        };
        let stage_elem_ty = if kind == PipeKind::Safe {
            raw_elem_ty
                .unwrap_nullable()
                .cloned()
                .unwrap_or_else(|| raw_elem_ty.clone())
        } else {
            raw_elem_ty.clone()
        };

        // PipeStage::Expr (`items ~> fn_ref`)
        if let PipeStage::Expr(fn_expr) = stage {
            let fv = self.emit_expr(fn_expr, out);
            let _ = span;
            return format!("{}({})", fv, lv);
        }

        let res = self.fresh_tmp("pipe_res");
        match stage {
            PipeStage::Map { .. } => {
                if let Type::Array(ref out_el) = ety {
                    // Perceus in-place reuse if mapping an owned unique lo_vec of identical element size!
                    if matches!(lty, Type::Array(_)) && kind == PipeKind::Seq {
                        let in_vec = self.fresh_tmp("pin");
                        let reuse_ok = self.fresh_tmp("preuse");
                        let _ = writeln!(out, "    lo_vec* {} = {};", in_vec, lv);
                        let _ = writeln!(
                            out,
                            "    bool {} = ({} && lo_rc_is_unique({}) && {}->backing_owner == NULL && sizeof({}) == sizeof({}));",
                            reuse_ok,
                            in_vec,
                            in_vec,
                            in_vec,
                            c_type_name(&raw_elem_ty),
                            c_type_name(out_el)
                        );
                        let _ = writeln!(
                            out,
                            "    lo_vec* {} = {} ? (atomic_fetch_add(&_lo_perceus_reuses, 1), {}) : lo_vec_new(sizeof({}), {} ? {}->len : 4);",
                            res,
                            reuse_ok,
                            in_vec,
                            c_type_name(out_el),
                            in_vec,
                            in_vec
                        );
                        self.emit_pipe_loop_body(
                            &in_vec,
                            &lty,
                            &raw_elem_ty,
                            &stage_elem_ty,
                            kind,
                            stage,
                            &res,
                            Some(&reuse_ok),
                            ety,
                            out,
                        );
                        return res;
                    } else {
                        let _ = writeln!(
                            out,
                            "    lo_vec* {} = lo_vec_new(sizeof({}), 8);",
                            res,
                            c_type_name(out_el)
                        );
                    }
                } else {
                    let _ = writeln!(out, "    lo_unit_t {} = lo_unit_val();", res);
                }
            }
            PipeStage::Filter { .. } => {
                let _ = writeln!(
                    out,
                    "    lo_vec* {} = lo_vec_new(sizeof({}), 8);",
                    res,
                    c_type_name(&stage_elem_ty)
                );
            }
            PipeStage::Fold { init, .. } => {
                let iv = self.emit_expr(init, out);
                let ity = self.expr_ty(init.span);
                let ico = self.coerce(&iv, &ity, ety, out);
                let _ = writeln!(out, "    {} {} = {};", c_type_name(ety), res, ico);
            }
            PipeStage::Expr(_) => unreachable!(),
        }

        let src_var = self.fresh_tmp("psrc");
        let _ = writeln!(out, "    {} {} = {};", c_type_name(&lty), src_var, lv);
        self.emit_pipe_loop_body(
            &src_var,
            &lty,
            &raw_elem_ty,
            &stage_elem_ty,
            kind,
            stage,
            &res,
            None,
            ety,
            out,
        );
        res
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_pipe_loop_body(
        &mut self,
        src_var: &str,
        lty: &Type,
        raw_elem_ty: &Type,
        stage_elem_ty: &Type,
        kind: PipeKind,
        stage: &PipeStage,
        res_var: &str,
        reuse_flag: Option<&str>,
        ety: &Type,
        out: &mut String,
    ) {
        let idx = self.fresh_tmp("pi");
        let raw_item = self.fresh_tmp("praw");
        let item_var = self.fresh_tmp("pitem");
        let step_cont = self.fresh_tmp("pcont");

        match lty {
            Type::Range(_) => {
                let _ = writeln!(
                    out,
                    "    for (int64_t {0} = {1}.start; {1}.inclusive ? ({0} <= {1}.end) : ({0} < {1}.end); {0}++) {{",
                    idx, src_var
                );
                let _ = writeln!(out, "        int64_t {} = {};", raw_item, idx);
            }
            Type::Array(_) => {
                let _ = writeln!(
                    out,
                    "    for (int64_t {0} = 0; {1} && {0} < {1}->len; {0}++) {{",
                    idx, src_var
                );
                let _ = writeln!(
                    out,
                    "        {} {} = *({}*)lo_vec_at({}, {}, 0);",
                    c_type_name(raw_elem_ty),
                    raw_item,
                    c_type_name(raw_elem_ty),
                    src_var,
                    idx
                );
            }
            Type::Set(_) => {
                let _ = writeln!(
                    out,
                    "    for (size_t {0} = 0; {1} && {0} < {1}->cap; {0}++) {{",
                    idx, src_var
                );
                let _ = writeln!(out, "        if (!{}->entries[{}].occupied) continue;", src_var, idx);
                let _ = writeln!(
                    out,
                    "        {} {} = *({}*){}->entries[{}].key;",
                    c_type_name(raw_elem_ty),
                    raw_item,
                    c_type_name(raw_elem_ty),
                    src_var,
                    idx
                );
            }
            _ => return,
        }

        if kind == PipeKind::Safe && raw_elem_ty.is_nullable() {
            let _ = writeln!(out, "        if (!{}.has_val) continue;", raw_item);
            let _ = writeln!(
                out,
                "        {} {} = {}.val;",
                c_type_name(stage_elem_ty),
                item_var,
                raw_item
            );
        } else {
            let _ = writeln!(
                out,
                "        {} {} = {};",
                c_type_name(stage_elem_ty),
                item_var,
                raw_item
            );
        }

        self.current_item_stack
            .push((item_var.clone(), stage_elem_ty.clone(), false));

        match stage {
            PipeStage::Map { param, body } => {
                let out_el = match ety {
                    Type::Array(el) => (**el).clone(),
                    _ => Type::Unit,
                };
                let step_res = self.fresh_tmp("pmv");
                let after_body = self.fresh_tmp("pmafter");
                let _ = writeln!(
                    out,
                    "        {} {} = {};",
                    c_type_name(&out_el),
                    step_res,
                    c_default_zero(&out_el)
                );
                self.push_scope();
                if let Some(p) = param {
                    let pv = self.fresh_tmp(&format!("v_{}", sanitize_ident(p)));
                    let _ = writeln!(
                        out,
                        "        {} {} = {};",
                        c_type_name(stage_elem_ty),
                        pv,
                        item_var
                    );
                    self.bind_local(p, pv, stage_elem_ty.clone(), false);
                }
                self.pipe_stack.push(PipeLocalTarget {
                    cont_label: after_body.clone(),
                    res_var: if out_el != Type::Unit {
                        Some((step_res.clone(), out_el.clone()))
                    } else {
                        None
                    },
                });
                let bv = self.emit_block(body, &out_el, out);
                self.pipe_stack.pop();
                self.pop_scope();
                if out_el != Type::Unit {
                    let _ = writeln!(out, "        {} = {};", step_res, bv);
                }
                let _ = writeln!(out, "        {}: ;", after_body);
                if out_el != Type::Unit {
                    if let Some(rf) = reuse_flag {
                        let _ = writeln!(
                            out,
                            "        if ({}) {{ *({}*)lo_vec_at({}, {}, 0) = {}; }} else {{ lo_vec_push({}, &{}); }}",
                            rf,
                            c_type_name(&out_el),
                            res_var,
                            idx,
                            step_res,
                            res_var,
                            step_res
                        );
                    } else {
                        let _ = writeln!(out, "        lo_vec_push({}, &{});", res_var, step_res);
                    }
                }
            }
            PipeStage::Filter { body } => {
                let keep = self.emit_block(body, &Type::Bool, out);
                let _ = writeln!(
                    out,
                    "        if ({}) lo_vec_push({}, &{});",
                    keep, res_var, item_var
                );
            }
            PipeStage::Fold { acc_name, body, .. } => {
                let step_acc = self.fresh_tmp("pfacc");
                let after_fold = self.fresh_tmp("pfafter");
                let _ = writeln!(out, "        {} {} = {};", c_type_name(ety), step_acc, res_var);
                self.push_scope();
                let acc_v = self.fresh_tmp(&format!("v_{}", sanitize_ident(acc_name)));
                let _ = writeln!(out, "        {} {} = {};", c_type_name(ety), acc_v, res_var);
                self.bind_local(acc_name, acc_v, ety.clone(), false);
                self.pipe_stack.push(PipeLocalTarget {
                    cont_label: after_fold.clone(),
                    res_var: Some((step_acc.clone(), ety.clone())),
                });
                let bv = self.emit_block(body, ety, out);
                self.pipe_stack.pop();
                self.pop_scope();
                let _ = writeln!(out, "        {} = {};", step_acc, bv);
                let _ = writeln!(out, "        {}: ;", after_fold);
                let _ = writeln!(out, "        {} = {};", res_var, step_acc);
            }
            PipeStage::Expr(_) => {}
        }

        self.current_item_stack.pop();
        let _ = writeln!(out, "        {}: ;", step_cont);
        let _ = writeln!(out, "    }}");
    }

    fn emit_generator_pipe(
        &mut self,
        lhs: &Expr,
        yield_ty: &Type,
        _kind: PipeKind,
        stage: &PipeStage,
        ety: &Type,
        out: &mut String,
    ) -> String {
        let gv = self.emit_expr(lhs, out);
        let gid = self.fresh_id();
        let ctx_struct = format!("_lo_gen_consume_ctx_{}", gid);
        let cb_name = format!("_lo_gen_consume_cb_{}", gid);

        let captured = self.all_visible_locals();
        let res_var = self.fresh_tmp("gres");

        // Build consumer context struct & callback
        let mut h_out = String::new();
        let _ = writeln!(h_out, "typedef struct {{");
        if *ety != Type::Unit {
            let _ = writeln!(h_out, "    {} res;", c_type_name(ety));
        }
        for (lo_n, _, vty) in &captured {
            let _ = writeln!(
                h_out,
                "    {} cap_{};",
                c_type_name(vty),
                sanitize_ident(lo_n)
            );
        }
        let _ = writeln!(h_out, "}} {};\n", ctx_struct);

        let _ = writeln!(
            h_out,
            "static bool {}({} _item, void* _raw_ctx) {{",
            cb_name,
            c_type_name(yield_ty)
        );
        let _ = writeln!(
            h_out,
            "    {}* _ctx = ({}*)_raw_ctx; (void)_ctx;",
            ctx_struct, ctx_struct
        );

        let mut cb_body = String::new();
        let mut sub_helper = String::new();
        {
            let mut sub_ctx = FnEmitCtx {
                prog: self.prog,
                module_prefix: self.module_prefix.clone(),
                expr_types: self.expr_types,
                call_targets: self.call_targets,
                struct_targets: self.struct_targets,
                ret_ty: Type::Bool,
                is_generator_worker: false,
                yield_ty: None,
                local_vars: vec![std::collections::HashMap::new()],
                current_item_stack: vec![("_item".into(), yield_ty.clone(), false)],
                self_fn_c_name: None,
                defers: Vec::new(),
                masking_defers: false,
                loop_stack: Vec::new(),
                label_stack: Vec::new(),
                pipe_stack: Vec::new(),
                try_catch_stack: Vec::new(),
                helper_defs: &mut sub_helper,
                next_id: self.next_id,
            };
            for (lo_n, _, vty) in &captured {
                sub_ctx.bind_local(
                    lo_n,
                    format!("_ctx->cap_{}", sanitize_ident(lo_n)),
                    vty.clone(),
                    false,
                );
            }

            match stage {
                PipeStage::Map { param, body } => {
                    let out_el = match ety {
                        Type::Array(el) => (**el).clone(),
                        _ => Type::Unit,
                    };
                    if let Some(p) = param {
                        sub_ctx.bind_local(p, "_item".into(), yield_ty.clone(), false);
                    }
                    let bv = sub_ctx.emit_block(body, &out_el, &mut cb_body);
                    if out_el != Type::Unit {
                        let tmp = sub_ctx.fresh_tmp("gel");
                        let _ = writeln!(
                            cb_body,
                            "    {} {} = {};",
                            c_type_name(&out_el),
                            tmp,
                            bv
                        );
                        let _ = writeln!(cb_body, "    lo_vec_push(_ctx->res, &{});", tmp);
                    }
                }
                PipeStage::Filter { body } => {
                    let keep = sub_ctx.emit_block(body, &Type::Bool, &mut cb_body);
                    let _ = writeln!(
                        cb_body,
                        "    if ({}) lo_vec_push(_ctx->res, &_item);",
                        keep
                    );
                }
                PipeStage::Fold { acc_name, body, .. } => {
                    sub_ctx.bind_local(acc_name, "_ctx->res".into(), ety.clone(), false);
                    let bv = sub_ctx.emit_block(body, ety, &mut cb_body);
                    let _ = writeln!(cb_body, "    _ctx->res = {};", bv);
                }
                PipeStage::Expr(_) => {}
            }
        }

        if !sub_helper.is_empty() {
            self.helper_defs.push_str(&sub_helper);
        }
        h_out.push_str(&cb_body);
        let _ = writeln!(h_out, "    return true;\n}}\n");
        self.helper_defs.push_str(&h_out);

        // Emit call site setup
        let ctx_var = self.fresh_tmp("gctx");
        let gen_var = self.fresh_tmp("gptr");
        let _ = writeln!(out, "    {} {} = {{0}};", ctx_struct, ctx_var);
        match stage {
            PipeStage::Map { .. } | PipeStage::Filter { .. } => {
                if let Type::Array(ref out_el) = ety {
                    let _ = writeln!(
                        out,
                        "    {}.res = lo_vec_new(sizeof({}), 8);",
                        ctx_var,
                        c_type_name(out_el)
                    );
                }
            }
            PipeStage::Fold { init, .. } => {
                let iv = self.emit_expr(init, out);
                let _ = writeln!(out, "    {}.res = {};", ctx_var, iv);
            }
            PipeStage::Expr(_) => {}
        }
        for (lo_n, c_expr, _) in &captured {
            let _ = writeln!(
                out,
                "    {}.cap_{} = {};",
                ctx_var,
                sanitize_ident(lo_n),
                c_expr
            );
        }
        let gen_ty = Type::Generator(Box::new(yield_ty.clone()));
        let _ = writeln!(out, "    {} {} = {};", c_type_name(&gen_ty), gen_var, gv);
        let _ = writeln!(
            out,
            "    if ({}) {}->run_fn({}->env, {}, &{});",
            gen_var, gen_var, gen_var, cb_name, ctx_var
        );
        if *ety != Type::Unit {
            let _ = writeln!(out, "    {} {} = {}.res;", c_type_name(ety), res_var, ctx_var);
            res_var
        } else {
            "lo_unit_val()".into()
        }
    }

    fn emit_parallel_pipe(
        &mut self,
        lhs: &Expr,
        lty: &Type,
        stage: &PipeStage,
        ety: &Type,
        out: &mut String,
    ) -> String {
        let lv = self.emit_expr(lhs, out);
        let in_elem_ty = match lty {
            Type::Array(el) | Type::Range(el) => (**el).clone(),
            _ => Type::I64,
        };

        // Materialize range into lo_vec if input is Range
        let in_vec = self.fresh_tmp("par_in");
        if matches!(lty, Type::Range(_)) {
            let rtmp = self.fresh_tmp("par_rng");
            let ri = self.fresh_tmp("pri");
            let _ = writeln!(out, "    lo_range_i64_t {} = {};", rtmp, lv);
            let _ = writeln!(out, "    lo_vec* {} = lo_vec_new(sizeof(int64_t), 16);", in_vec);
            let _ = writeln!(
                out,
                "    for (int64_t {0} = {1}.start; {1}.inclusive ? ({0} <= {1}.end) : ({0} < {1}.end); {0}++) lo_vec_push({2}, &{0});",
                ri, rtmp, in_vec
            );
        } else {
            let _ = writeln!(out, "    lo_vec* {} = {};", in_vec, lv);
        }

        let PipeStage::Map { param, body } = stage else {
            return in_vec;
        };

        let out_elem_ty = match ety {
            Type::Array(el) => (**el).clone(),
            _ => Type::Unit,
        };

        let pid = self.fresh_id();
        let ctx_struct = format!("_lo_par_ctx_{}", pid);
        let worker_fn = format!("_lo_par_fn_{}", pid);
        let captured = self.all_visible_locals();

        let mut h_out = String::new();
        let _ = writeln!(h_out, "typedef struct {{");
        let _ = writeln!(h_out, "    uint8_t _pad;");
        for (lo_n, _, vty) in &captured {
            let _ = writeln!(
                h_out,
                "    {} cap_{};",
                c_type_name(vty),
                sanitize_ident(lo_n)
            );
        }
        let _ = writeln!(h_out, "}} {};\n", ctx_struct);

        let _ = writeln!(
            h_out,
            "static void {}(size_t _idx, const void* _in_ptr, void* _out_ptr, void* _raw_ctx) {{",
            worker_fn
        );
        let _ = writeln!(h_out, "    (void)_idx;");
        let _ = writeln!(
            h_out,
            "    {}* _ctx = ({}*)_raw_ctx; (void)_ctx;",
            ctx_struct, ctx_struct
        );
        let _ = writeln!(
            h_out,
            "    {} _item = *(const {}*)_in_ptr;",
            c_type_name(&in_elem_ty),
            c_type_name(&in_elem_ty)
        );

        let mut w_body = String::new();
        let mut sub_helpers = String::new();
        {
            let mut sub_ctx = FnEmitCtx {
                prog: self.prog,
                module_prefix: self.module_prefix.clone(),
                expr_types: self.expr_types,
                call_targets: self.call_targets,
                struct_targets: self.struct_targets,
                ret_ty: out_elem_ty.clone(),
                is_generator_worker: false,
                yield_ty: None,
                local_vars: vec![std::collections::HashMap::new()],
                current_item_stack: vec![("_item".into(), in_elem_ty.clone(), false)],
                self_fn_c_name: None,
                defers: Vec::new(),
                masking_defers: false,
                loop_stack: Vec::new(),
                label_stack: Vec::new(),
                pipe_stack: Vec::new(),
                try_catch_stack: Vec::new(),
                helper_defs: &mut sub_helpers,
                next_id: self.next_id,
            };
            for (lo_n, _, vty) in &captured {
                sub_ctx.bind_local(
                    lo_n,
                    format!("_ctx->cap_{}", sanitize_ident(lo_n)),
                    vty.clone(),
                    false,
                );
            }
            if let Some(p) = param {
                sub_ctx.bind_local(p, "_item".into(), in_elem_ty.clone(), false);
            }
            let bv = sub_ctx.emit_block(body, &out_elem_ty, &mut w_body);
            if out_elem_ty != Type::Unit {
                let _ = writeln!(
                    w_body,
                    "    if (_out_ptr) *({}*)_out_ptr = {};",
                    c_type_name(&out_elem_ty),
                    bv
                );
            }
        }
        if !sub_helpers.is_empty() {
            self.helper_defs.push_str(&sub_helpers);
        }
        h_out.push_str(&w_body);
        let _ = writeln!(h_out, "}}\n");
        self.helper_defs.push_str(&h_out);

        let ctx_var = self.fresh_tmp("pctx");
        let _ = writeln!(out, "    {} {} = {{0}};", ctx_struct, ctx_var);
        for (lo_n, c_expr, _) in &captured {
            let _ = writeln!(
                out,
                "    {}.cap_{} = {};",
                ctx_var,
                sanitize_ident(lo_n),
                c_expr
            );
        }

        if out_elem_ty == Type::Unit {
            let _ = writeln!(
                out,
                "    if ({0} && {0}->len > 0) lo_par_for_each((size_t){0}->len, sizeof({1}), {0}->data, 0, NULL, {2}, &{3});",
                in_vec,
                c_type_name(&in_elem_ty),
                worker_fn,
                ctx_var
            );
            "lo_unit_val()".into()
        } else {
            let out_vec = self.fresh_tmp("par_out");
            let _ = writeln!(
                out,
                "    lo_vec* {0} = lo_vec_new(sizeof({1}), {2} ? {2}->len : 1);",
                out_vec,
                c_type_name(&out_elem_ty),
                in_vec
            );
            let _ = writeln!(
                out,
                "    if ({0} && {0}->len > 0) {{ {1}->len = {0}->len; lo_par_for_each((size_t){0}->len, sizeof({2}), {0}->data, sizeof({3}), {1}->data, {4}, &{5}); }}",
                in_vec,
                out_vec,
                c_type_name(&in_elem_ty),
                c_type_name(&out_elem_ty),
                worker_fn,
                ctx_var
            );
            out_vec
        }
    }

    pub fn emit_async_spawn(&mut self, blk: &Block, ety: &Type, out: &mut String) -> String {
        let task_ret = match ety {
            Type::Task(inner) => (**inner).clone(),
            _ => Type::Unit,
        };
        let aid = self.fresh_id();
        let ctx_struct = format!("_lo_async_ctx_{}", aid);
        let entry_fn = format!("_lo_async_fn_{}", aid);
        let captured = self.all_visible_locals();

        let mut h_out = String::new();
        let _ = writeln!(h_out, "typedef struct {{");
        let _ = writeln!(h_out, "    uint8_t _pad;");
        for (lo_n, _, vty) in &captured {
            let _ = writeln!(
                h_out,
                "    {} cap_{};",
                c_type_name(vty),
                sanitize_ident(lo_n)
            );
        }
        let _ = writeln!(h_out, "}} {};\n", ctx_struct);

        let _ = writeln!(
            h_out,
            "static void {}(void* _raw_arg, void* _raw_ret) {{",
            entry_fn
        );
        let _ = writeln!(
            h_out,
            "    {}* _ctx = ({}*)_raw_arg; (void)_ctx; (void)_raw_ret;",
            ctx_struct, ctx_struct
        );

        let mut tbody = String::new();
        let mut sub_helpers = String::new();
        {
            let mut sub_ctx = FnEmitCtx {
                prog: self.prog,
                module_prefix: self.module_prefix.clone(),
                expr_types: self.expr_types,
                call_targets: self.call_targets,
                struct_targets: self.struct_targets,
                ret_ty: task_ret.clone(),
                is_generator_worker: false,
                yield_ty: None,
                local_vars: vec![std::collections::HashMap::new()],
                current_item_stack: Vec::new(),
                self_fn_c_name: None,
                defers: Vec::new(),
                masking_defers: false,
                loop_stack: Vec::new(),
                label_stack: Vec::new(),
                pipe_stack: Vec::new(),
                try_catch_stack: Vec::new(),
                helper_defs: &mut sub_helpers,
                next_id: self.next_id,
            };
            for (lo_n, _, vty) in &captured {
                sub_ctx.bind_local(
                    lo_n,
                    format!("_ctx->cap_{}", sanitize_ident(lo_n)),
                    vty.clone(),
                    false,
                );
            }
            let bv = sub_ctx.emit_block(blk, &task_ret, &mut tbody);
            if task_ret != Type::Unit {
                let _ = writeln!(
                    tbody,
                    "    if (_raw_ret) *({}*)_raw_ret = {};",
                    c_type_name(&task_ret),
                    bv
                );
            }
        }
        if !sub_helpers.is_empty() {
            self.helper_defs.push_str(&sub_helpers);
        }
        h_out.push_str(&tbody);
        let _ = writeln!(h_out, "}}\n");
        self.helper_defs.push_str(&h_out);

        let ctx_var = self.fresh_tmp("actx");
        let _ = writeln!(out, "    {} {} = {{0}};", ctx_struct, ctx_var);
        for (lo_n, c_expr, _) in &captured {
            let _ = writeln!(
                out,
                "    {}.cap_{} = {};",
                ctx_var,
                sanitize_ident(lo_n),
                c_expr
            );
        }
        format!(
            "lo_task_spawn({}, &{}, sizeof({}), sizeof({}))",
            entry_fn,
            ctx_var,
            ctx_struct,
            c_type_name(&task_ret)
        )
    }
}
