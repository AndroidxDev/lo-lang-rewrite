use crate::codegen::c_types::{c_default_zero, c_type_name};
use crate::lexer::Span;
use crate::parser::*;
use crate::sema::{sanitize_ident, span_key, CheckedProgram, CheckedVariantPayload, SpanKey, Type};
use std::collections::HashMap;
use std::fmt::Write;

#[derive(Clone)]
pub struct DeferEntry {
    pub flag_var: String,
    pub is_errdefer: bool,
    pub body: Block,
    pub captured_scopes: Vec<HashMap<String, (String, Type, bool)>>,
}

#[derive(Clone)]
pub struct LoopTarget {
    pub break_label: String,
    pub cont_label: String,
    pub break_res_var: Option<(String, Type)>,
}

#[derive(Clone)]
pub struct LabelTarget {
    pub name: String,
    pub end_label: String,
    pub cont_label: Option<String>,
    pub res_var: (String, Type),
}

#[derive(Clone)]
pub struct PipeLocalTarget {
    pub cont_label: String,
    pub res_var: Option<(String, Type)>,
}

pub struct FnEmitCtx<'a> {
    pub prog: &'a CheckedProgram,
    pub module_prefix: String,
    pub expr_types: &'a HashMap<SpanKey, Type>,
    pub call_targets: &'a HashMap<SpanKey, String>,
    pub struct_targets: &'a HashMap<SpanKey, String>,
    pub ret_ty: Type,
    pub is_generator_worker: bool,
    pub yield_ty: Option<Type>,
    pub local_vars: Vec<HashMap<String, (String, Type, bool)>>, // lo_name -> (c_name, ty, is_ptr)
    pub current_item_stack: Vec<(String, Type, bool)>,          // (c_expr, ty, is_ptr)
    pub self_fn_c_name: Option<String>,
    pub defers: Vec<DeferEntry>,
    pub masking_defers: bool,
    pub loop_stack: Vec<LoopTarget>,
    pub label_stack: Vec<LabelTarget>,
    pub pipe_stack: Vec<PipeLocalTarget>,
    pub try_catch_stack: Vec<(String, usize)>,
    pub helper_defs: &'a mut String,
    pub next_id: &'a mut usize,
}

impl<'a> FnEmitCtx<'a> {
    pub fn fresh_id(&mut self) -> usize {
        let id = *self.next_id;
        *self.next_id += 1;
        id
    }

    pub fn fresh_tmp(&mut self, prefix: &str) -> String {
        let id = self.fresh_id();
        format!("_{}_{}", prefix, id)
    }

    pub fn expr_ty(&self, span: Span) -> Type {
        self.expr_types
            .get(&span_key(span))
            .cloned()
            .unwrap_or(Type::Unit)
    }

    pub fn push_scope(&mut self) {
        self.local_vars.push(HashMap::new());
    }

    pub fn pop_scope(&mut self) {
        self.local_vars.pop();
    }

    pub fn bind_local(&mut self, lo_name: &str, c_name: String, ty: Type, is_ptr: bool) {
        if let Some(top) = self.local_vars.last_mut() {
            top.insert(lo_name.to_string(), (c_name, ty, is_ptr));
        }
    }

    pub fn lookup_local(&self, lo_name: &str) -> Option<&(String, Type, bool)> {
        for scope in self.local_vars.iter().rev() {
            if let Some(entry) = scope.get(lo_name) {
                return Some(entry);
            }
        }
        None
    }

    pub fn all_visible_locals(&self) -> Vec<(String, String, Type)> {
        let mut seen = HashMap::new();
        for scope in &self.local_vars {
            for (lo_n, (c_n, ty, is_ptr)) in scope {
                let expr_s = if *is_ptr {
                    format!("(*{})", c_n)
                } else {
                    c_n.clone()
                };
                seen.insert(lo_n.clone(), (expr_s, ty.clone()));
            }
        }
        let mut res: Vec<(String, String, Type)> =
            seen.into_iter().map(|(k, (ce, ty))| (k, ce, ty)).collect();
        res.sort_by(|a, b| a.0.cmp(&b.0));
        res
    }

    pub fn coerce(&mut self, val: &str, from: &Type, to: &Type, out: &mut String) -> String {
        if from == to || *to == Type::Unit || *from == Type::Never {
            return val.to_string();
        }
        if let Type::Nullable(inner_to) = to {
            let opt_c = c_type_name(to);
            if *from == Type::Null {
                return format!("(({}){{ .has_val = false }})", opt_c);
            }
            if let Type::Nullable(inner_from) = from {
                let tmp = self.fresh_tmp("optc");
                let src_tmp = self.fresh_tmp("opts");
                let _ = writeln!(out, "    {} {} = {};", c_type_name(from), src_tmp, val);
                let _ = writeln!(out, "    {} {} = {{ .has_val = false }};", opt_c, tmp);
                let _ = writeln!(out, "    if ({}.has_val) {{", src_tmp);
                let inner_v = self.coerce(&format!("{}.val", src_tmp), inner_from, inner_to, out);
                let _ = writeln!(out, "        {}.has_val = true;", tmp);
                let _ = writeln!(out, "        {}.val = {};", tmp, inner_v);
                let _ = writeln!(out, "    }}");
                return tmp;
            }
            let coerced_inner = self.coerce(val, from, inner_to, out);
            return format!(
                "(({}){{ .has_val = true, .val = {} }})",
                opt_c, coerced_inner
            );
        }
        // Smart Cast: variable stored as `lo_opt_T` in C, but Sema narrowed `expr_ty` or `to` to non-null `T`!
        if let Type::Nullable(inner_from) = from {
            let unwrapped = format!("({}).val", val);
            return self.coerce(&unwrapped, inner_from, to, out);
        }
        if *from == Type::Unit {
            if let Type::Array(el) = to {
                return format!("lo_vec_new(sizeof({}), 0)", c_type_name(el));
            }
            if let Type::Map(k, v) = to {
                let is_str = **k == Type::Str;
                return format!(
                    "lo_map_new(sizeof({}), sizeof({}), {})",
                    c_type_name(k),
                    c_type_name(v),
                    if is_str { "true" } else { "false" }
                );
            }
        }
        if from.is_numeric() && to.is_numeric() {
            return format!("(({})({}))", c_type_name(to), val);
        }
        val.to_string()
    }

    pub fn emit_to_str(&mut self, val: &str, ty: &Type, out: &mut String) -> String {
        match ty {
            Type::Str => val.to_string(),
            Type::I8 | Type::I16 | Type::I32 | Type::I64 => {
                format!("lo_fmt_i64((int64_t)({}))", val)
            }
            Type::U8 | Type::U16 | Type::U32 | Type::U64 | Type::Usz => {
                format!("lo_fmt_u64((uint64_t)({}))", val)
            }
            Type::F32 | Type::F64 => format!("lo_fmt_f64((double)({}))", val),
            Type::Bool => format!("lo_fmt_bool({})", val),
            Type::Char => format!("lo_fmt_char((uint32_t)({}))", val),
            Type::Unit => "lo_str_lit(\"[]\", 2)".into(),
            Type::Null => "lo_str_lit(\"~\", 1)".into(),
            Type::ErrorAny => format!("({}).msg", val),
            Type::Nullable(inner) => {
                let tmp_opt = self.fresh_tmp("fopt");
                let tmp_str = self.fresh_tmp("fstr");
                let _ = writeln!(out, "    {} {} = {};", c_type_name(ty), tmp_opt, val);
                let _ = writeln!(out, "    lo_str {};", tmp_str);
                let _ = writeln!(out, "    if ({}.has_val) {{", tmp_opt);
                let inner_s = self.emit_to_str(&format!("{}.val", tmp_opt), inner, out);
                let _ = writeln!(out, "        {} = {};", tmp_str, inner_s);
                let _ = writeln!(out, "    }} else {{");
                let _ = writeln!(out, "        {} = lo_str_lit(\"~\", 1);", tmp_str);
                let _ = writeln!(out, "    }}");
                tmp_str
            }
            Type::Struct(sname) => {
                if let Some(st) = self.prog.structs.get(sname) {
                    let st_tmp = self.fresh_tmp("fst");
                    let _ = writeln!(out, "    {} {} = {};", c_type_name(ty), st_tmp, val);
                    let mut parts_exprs = Vec::new();
                    let header = format!("{}[", sname);
                    parts_exprs.push(format!("lo_str_lit({:?}, {})", header, header.len()));
                    for (i, f) in st.fields.iter().enumerate() {
                        let prefix = if i == 0 {
                            format!("{} = ", f.name)
                        } else {
                            format!(", {} = ", f.name)
                        };
                        parts_exprs.push(format!("lo_str_lit({:?}, {})", prefix, prefix.len()));
                        let fv = self.emit_to_str(
                            &format!("{}.f_{}", st_tmp, sanitize_ident(&f.name)),
                            &f.ty,
                            out,
                        );
                        parts_exprs.push(fv);
                    }
                    parts_exprs.push("lo_str_lit(\"]\", 1)".to_string());
                    let arr_tmp = self.fresh_tmp("fparts");
                    let _ = writeln!(
                        out,
                        "    lo_str {}[{}] = {{ {} }};",
                        arr_tmp,
                        parts_exprs.len(),
                        parts_exprs.join(", ")
                    );
                    return format!("lo_str_join_parts({}, {})", arr_tmp, parts_exprs.len());
                }
                "lo_str_lit(\"<struct>\", 8)".into()
            }
            Type::Array(elem) => {
                let v_tmp = self.fresh_tmp("fvec");
                let acc_tmp = self.fresh_tmp("facc");
                let idx_tmp = self.fresh_tmp("fi");
                let _ = writeln!(out, "    lo_vec* {} = {};", v_tmp, val);
                let _ = writeln!(out, "    lo_str {} = lo_str_lit(\"[\", 1);", acc_tmp);
                let _ = writeln!(
                    out,
                    "    for (int64_t {0} = 0; {1} && {0} < {1}->len; {0}++) {{",
                    idx_tmp, v_tmp
                );
                let _ = writeln!(
                    out,
                    "        if ({} > 0) {} = lo_str_concat({}, lo_str_lit(\", \", 2));",
                    idx_tmp, acc_tmp, acc_tmp
                );
                let el_expr = format!(
                    "(*({}*)lo_vec_at({}, {}, 0))",
                    c_type_name(elem),
                    v_tmp,
                    idx_tmp
                );
                let el_s = self.emit_to_str(&el_expr, elem, out);
                let _ = writeln!(
                    out,
                    "        {} = lo_str_concat({}, {});",
                    acc_tmp, acc_tmp, el_s
                );
                let _ = writeln!(out, "    }}");
                let _ = writeln!(
                    out,
                    "    {} = lo_str_concat({}, lo_str_lit(\"]\", 1));",
                    acc_tmp, acc_tmp
                );
                acc_tmp
            }
            _ => "lo_str_lit(\"<val>\", 5)".into(),
        }
    }

    pub fn emit_eq(
        &mut self,
        lv: &str,
        rv: &str,
        lty: &Type,
        rty: &Type,
        out: &mut String,
    ) -> String {
        if *lty == Type::Null && *rty == Type::Null {
            return "true".into();
        }
        if *lty == Type::Null {
            if rty.is_nullable() {
                return format!("(!({}).has_val)", rv);
            }
            return "false".into();
        }
        if *rty == Type::Null {
            if lty.is_nullable() {
                return format!("(!({}).has_val)", lv);
            }
            return "false".into();
        }
        if let (Type::Nullable(ia), Type::Nullable(ib)) = (lty, rty) {
            let ta = self.fresh_tmp("eqa");
            let tb = self.fresh_tmp("eqb");
            let tres = self.fresh_tmp("eqr");
            let _ = writeln!(out, "    {} {} = {};", c_type_name(lty), ta, lv);
            let _ = writeln!(out, "    {} {} = {};", c_type_name(rty), tb, rv);
            let _ = writeln!(out, "    bool {} = false;", tres);
            let _ = writeln!(
                out,
                "    if (!{}.has_val && !{}.has_val) {{ {} = true; }}",
                ta, tb, tres
            );
            let _ = writeln!(out, "    else if ({}.has_val && {}.has_val) {{", ta, tb);
            let sub = self.emit_eq(&format!("{}.val", ta), &format!("{}.val", tb), ia, ib, out);
            let _ = writeln!(out, "        {} = {};", tres, sub);
            let _ = writeln!(out, "    }}");
            return tres;
        }
        if let Type::Nullable(ia) = lty {
            let ta = self.fresh_tmp("eqa");
            let tres = self.fresh_tmp("eqr");
            let _ = writeln!(out, "    {} {} = {};", c_type_name(lty), ta, lv);
            let _ = writeln!(out, "    bool {} = false;", tres);
            let _ = writeln!(out, "    if ({}.has_val) {{", ta);
            let sub = self.emit_eq(&format!("{}.val", ta), rv, ia, rty, out);
            let _ = writeln!(out, "        {} = {};", tres, sub);
            let _ = writeln!(out, "    }}");
            return tres;
        }
        if let Type::Nullable(_) = rty {
            return self.emit_eq(rv, lv, rty, lty, out);
        }
        if *lty == Type::Str && *rty == Type::Str {
            return format!("lo_str_eq({}, {})", lv, rv);
        }
        if let (Type::Struct(sa), Type::Struct(sb)) = (lty, rty) {
            if sa == sb {
                if let Some(st) = self.prog.structs.get(sa) {
                    let ta = self.fresh_tmp("st_a");
                    let tb = self.fresh_tmp("st_b");
                    let _ = writeln!(out, "    {} {} = {};", c_type_name(lty), ta, lv);
                    let _ = writeln!(out, "    {} {} = {};", c_type_name(rty), tb, rv);
                    let mut conds = Vec::new();
                    for f in &st.fields {
                        let fnm = sanitize_ident(&f.name);
                        conds.push(self.emit_eq(
                            &format!("{}.f_{}", ta, fnm),
                            &format!("{}.f_{}", tb, fnm),
                            &f.ty,
                            &f.ty,
                            out,
                        ));
                    }
                    if conds.is_empty() {
                        return "true".into();
                    }
                    return format!("({})", conds.join(" && "));
                }
            }
        }
        format!("(({}) == ({}))", lv, rv)
    }

    pub fn emit_expr(&mut self, expr: &Expr, out: &mut String) -> String {
        let ety = self.expr_ty(expr.span);
        match &expr.kind {
            ExprKind::Unit => match &ety {
                Type::Array(el) => format!("lo_vec_new(sizeof({}), 1)", c_type_name(el)),
                Type::Map(k, v) => format!(
                    "lo_map_new(sizeof({}), sizeof({}), {})",
                    c_type_name(k),
                    c_type_name(v),
                    if **k == Type::Str { "true" } else { "false" }
                ),
                _ => "lo_unit_val()".into(),
            },
            ExprKind::Null => {
                if matches!(ety, Type::Nullable(_)) {
                    format!("(({}){{ .has_val = false }})", c_type_name(&ety))
                } else {
                    "lo_unit_val()".into()
                }
            }
            ExprKind::Bool(b) => if *b { "true" } else { "false" }.into(),
            ExprKind::Int(n) => {
                if *n == i64::MIN {
                    "(-9223372036854775807LL - 1LL)".into()
                } else {
                    format!("INT64_C({})", n)
                }
            }
            ExprKind::Float(x) => {
                if x.is_nan() {
                    "NAN".into()
                } else if x.is_infinite() {
                    if x.is_sign_positive() {
                        "INFINITY".into()
                    } else {
                        "(-INFINITY)".into()
                    }
                } else if x.fract() == 0.0 {
                    format!("{:.1}", x)
                } else {
                    format!("{:?}", x)
                }
            }
            ExprKind::Char(c) => format!("UINT32_C({})", *c as u32),
            ExprKind::Str(s) => format!("lo_str_lit({}, {})", escape_c_str(s), s.len()),
            ExprKind::InterpString(segs) => {
                if segs.is_empty() {
                    return "lo_str_lit(\"\", 0)".into();
                }
                let mut part_vars = Vec::with_capacity(segs.len());
                for seg in segs {
                    match seg {
                        InterpSegment::Literal(lit) => {
                            part_vars.push(format!(
                                "lo_str_lit({}, {})",
                                escape_c_str(lit),
                                lit.len()
                            ));
                        }
                        InterpSegment::Expr(sub) => {
                            let sv = self.emit_expr(sub, out);
                            let sty = self.expr_ty(sub.span);
                            part_vars.push(self.emit_to_str(&sv, &sty, out));
                        }
                    }
                }
                let arr = self.fresh_tmp("iparts");
                let res = self.fresh_tmp("istr");
                let _ = writeln!(
                    out,
                    "    lo_str {}[{}] = {{ {} }};",
                    arr,
                    part_vars.len(),
                    part_vars.join(", ")
                );
                let _ = writeln!(
                    out,
                    "    lo_str {} = lo_str_join_parts({}, {});",
                    res,
                    arr,
                    part_vars.len()
                );
                res
            }
            ExprKind::Ident(name) => {
                if let Some((c_var, raw_ty, is_ptr)) = self.lookup_local(name).cloned() {
                    let base = if is_ptr {
                        format!("(*{})", c_var)
                    } else {
                        c_var
                    };
                    // Smart Cast automatic unwrapping when raw variable is Nullable(T) and effective expr_ty is T!
                    return self.coerce(&base, &raw_ty, &ety, out);
                }
                if let Some(target_c) = self.call_targets.get(&span_key(expr.span)) {
                    return target_c.clone();
                }
                if !self.module_prefix.is_empty() {
                    if let Some(g) = self
                        .prog
                        .globals
                        .iter()
                        .find(|g| g.module_prefix == self.module_prefix && &g.lo_name == name)
                    {
                        return g.c_name.clone();
                    }
                }
                if let Some(g) = self.prog.globals.iter().find(|g| &g.lo_name == name) {
                    return g.c_name.clone();
                }
                format!("lo_g_{}", sanitize_ident(name))
            }
            ExprKind::TypeRef { name, .. } => {
                // Unit Enum Variant `Idle`
                if let Type::Enum(ref ename) = ety {
                    if let Some(en) = self.prog.enums.get(ename) {
                        if let Some(v) = en.variants.iter().find(|v| &v.name == name) {
                            return format!("((lo_en_{}){{ .tag = {}u }})", ename, v.tag);
                        }
                    }
                }
                "lo_unit_val()".into()
            }
            ExprKind::TypeVal(_) => "lo_unit_val()".into(),
            ExprKind::CurrentItem => {
                if let Some((cexpr, raw_ty, is_ptr)) = self.current_item_stack.last().cloned() {
                    let v = if is_ptr {
                        format!("(*{})", cexpr)
                    } else {
                        cexpr
                    };
                    return self.coerce(&v, &raw_ty, &ety, out);
                }
                "lo_unit_val()".into()
            }
            ExprKind::SelfFn => self.self_fn_c_name.clone().unwrap_or_else(|| "NULL".into()),
            ExprKind::Array(elems) => {
                if let Type::Tuple(ref tup_tys) = ety {
                    let tmp = self.fresh_tmp("tup");
                    let _ = writeln!(out, "    {} {} = {{0}};", c_type_name(&ety), tmp);
                    for (i, (e, tty)) in elems.iter().zip(tup_tys.iter()).enumerate() {
                        let ev = self.emit_expr(e, out);
                        let raw_t = self.expr_ty(e.span);
                        let cv = self.coerce(&ev, &raw_t, tty, out);
                        let _ = writeln!(out, "    {}.t{} = {};", tmp, i, cv);
                    }
                    return tmp;
                }
                let elem_ty = match &ety {
                    Type::Array(el) => (**el).clone(),
                    _ => Type::I64,
                };
                let vec_var = self.fresh_tmp("vec");
                let _ = writeln!(
                    out,
                    "    lo_vec* {} = lo_vec_new(sizeof({}), {});",
                    vec_var,
                    c_type_name(&elem_ty),
                    elems.len().max(1)
                );
                for e in elems {
                    let ev = self.emit_expr(e, out);
                    let raw_t = self.expr_ty(e.span);
                    let cv = self.coerce(&ev, &raw_t, &elem_ty, out);
                    let etmp = self.fresh_tmp("el");
                    let _ = writeln!(out, "    {} {} = {};", c_type_name(&elem_ty), etmp, cv);
                    let _ = writeln!(out, "    lo_vec_push({}, &{});", vec_var, etmp);
                }
                vec_var
            }
            ExprKind::RepeatArray { value, count, .. } => {
                let elem_ty = match &ety {
                    Type::Array(el) => (**el).clone(),
                    _ => Type::U8,
                };
                let vv = self.emit_expr(value, out);
                let vraw = self.expr_ty(value.span);
                let vcoerced = self.coerce(&vv, &vraw, &elem_ty, out);
                let cnt = self.emit_expr(count, out);
                let vtmp = self.fresh_tmp("repv");
                let _ = writeln!(
                    out,
                    "    {} {} = {};",
                    c_type_name(&elem_ty),
                    vtmp,
                    vcoerced
                );
                format!(
                    "lo_vec_repeat(sizeof({}), &{}, (int64_t)({}))",
                    c_type_name(&elem_ty),
                    vtmp,
                    cnt
                )
            }
            ExprKind::SetLit(elems) => {
                let elem_ty = match &ety {
                    Type::Set(el) => (**el).clone(),
                    _ => Type::Str,
                };
                let is_str = elem_ty == Type::Str;
                let s_var = self.fresh_tmp("set");
                let _ = writeln!(
                    out,
                    "    lo_map* {} = lo_map_new(sizeof({}), 0, {});",
                    s_var,
                    c_type_name(&elem_ty),
                    if is_str { "true" } else { "false" }
                );
                for e in elems {
                    let ev = self.emit_expr(e, out);
                    let etmp = self.fresh_tmp("sk");
                    let _ = writeln!(out, "    {} {} = {};", c_type_name(&elem_ty), etmp, ev);
                    let _ = writeln!(out, "    lo_map_set({}, &{}, NULL);", s_var, etmp);
                }
                s_var
            }
            ExprKind::MapLit(entries) => {
                let (k_ty, v_ty) = match &ety {
                    Type::Map(k, v) => ((**k).clone(), (**v).clone()),
                    _ => (Type::Str, Type::I64),
                };
                let is_str = k_ty == Type::Str;
                let m_var = self.fresh_tmp("map");
                let _ = writeln!(
                    out,
                    "    lo_map* {} = lo_map_new(sizeof({}), sizeof({}), {});",
                    m_var,
                    c_type_name(&k_ty),
                    c_type_name(&v_ty),
                    if is_str { "true" } else { "false" }
                );
                for (ke, ve) in entries {
                    let kv = self.emit_expr(ke, out);
                    let kraw = self.expr_ty(ke.span);
                    let kco = self.coerce(&kv, &kraw, &k_ty, out);
                    let vv = self.emit_expr(ve, out);
                    let vraw = self.expr_ty(ve.span);
                    let vco = self.coerce(&vv, &vraw, &v_ty, out);
                    let ktmp = self.fresh_tmp("mk");
                    let vtmp = self.fresh_tmp("mv");
                    let _ = writeln!(out, "    {} {} = {};", c_type_name(&k_ty), ktmp, kco);
                    let _ = writeln!(out, "    {} {} = {};", c_type_name(&v_ty), vtmp, vco);
                    let _ = writeln!(out, "    lo_map_set({}, &{}, &{});", m_var, ktmp, vtmp);
                }
                m_var
            }
            ExprKind::StructLit { spread, fields, .. } => {
                self.emit_struct_or_variant_lit(spread.as_deref(), fields, expr.span, &ety, out)
            }
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                let sv = start
                    .as_ref()
                    .map(|s| self.emit_expr(s, out))
                    .unwrap_or_else(|| "0".into());
                let ev = end
                    .as_ref()
                    .map(|e| self.emit_expr(e, out))
                    .unwrap_or_else(|| "INT64_MAX".into());
                format!(
                    "((lo_range_i64_t){{ .start = (int64_t)({}), .end = (int64_t)({}), .inclusive = {} }})",
                    sv,
                    ev,
                    if *inclusive { "true" } else { "false" }
                )
            }
            ExprKind::Unary { op, expr: inner } => {
                let iv = self.emit_expr(inner, out);
                let ity = self.expr_ty(inner.span);
                match op {
                    UnaryOp::Neg => format!("(-({}))", iv),
                    UnaryOp::Not => format!("(!({}))", iv),
                    UnaryOp::BitNot => format!("(~({}))", iv),
                    UnaryOp::Await => match ity {
                        Type::Task(_) => {
                            let res = self.fresh_tmp("await");
                            let _ = writeln!(
                                out,
                                "    {} {} = {};",
                                c_type_name(&ety),
                                res,
                                c_default_zero(&ety)
                            );
                            let _ = writeln!(out, "    lo_task_await({}, &{});", iv, res);
                            res
                        }
                        Type::Channel(el) => {
                            let res = self.fresh_tmp("chrecv");
                            let _ = writeln!(
                                out,
                                "    {} {} = {{ .has_val = false }};",
                                c_type_name(&ety),
                                res
                            );
                            let _ = writeln!(
                                out,
                                "    {}.has_val = lo_chan_recv({}, &{}.val);",
                                res, iv, res
                            );
                            let _ = el;
                            res
                        }
                        _ => "lo_unit_val()".into(),
                    },
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                if *op == BinOp::And {
                    let res = self.fresh_tmp("and");
                    let lv = self.emit_expr(lhs, out);
                    let _ = writeln!(out, "    bool {} = false;", res);
                    let _ = writeln!(out, "    if ({}) {{", lv);
                    let rv = self.emit_expr(rhs, out);
                    let _ = writeln!(out, "        {} = {};", res, rv);
                    let _ = writeln!(out, "    }}");
                    return res;
                }
                if *op == BinOp::Or {
                    let res = self.fresh_tmp("or");
                    let lv = self.emit_expr(lhs, out);
                    let _ = writeln!(out, "    bool {} = true;", res);
                    let _ = writeln!(out, "    if (!({})) {{", lv);
                    let rv = self.emit_expr(rhs, out);
                    let _ = writeln!(out, "        {} = {};", res, rv);
                    let _ = writeln!(out, "    }}");
                    return res;
                }
                let lv = self.emit_expr(lhs, out);
                let rv = self.emit_expr(rhs, out);
                let lty = self.expr_ty(lhs.span);
                let rty = self.expr_ty(rhs.span);

                if *op == BinOp::ChanSend {
                    let el_ty = match lty {
                        Type::Channel(el) => *el,
                        _ => rty.clone(),
                    };
                    let cv = self.coerce(&rv, &rty, &el_ty, out);
                    let tmp = self.fresh_tmp("chv");
                    let _ = writeln!(out, "    {} {} = {};", c_type_name(&el_ty), tmp, cv);
                    let _ = writeln!(out, "    lo_chan_send({}, &{});", lv, tmp);
                    return "lo_unit_val()".into();
                }

                match op {
                    BinOp::Eq => self.emit_eq(&lv, &rv, &lty, &rty, out),
                    BinOp::NotEq => {
                        let eq = self.emit_eq(&lv, &rv, &lty, &rty, out);
                        format!("(!({}))", eq)
                    }
                    BinOp::Add if ety == Type::Str => format!("lo_str_concat({}, {})", lv, rv),
                    BinOp::Rem if ety.is_float() => {
                        format!("fmod((double)({}), (double)({}))", lv, rv)
                    }
                    BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq
                        if lty == Type::Str && rty == Type::Str =>
                    {
                        let cmp_op = match op {
                            BinOp::Lt => "<",
                            BinOp::Gt => ">",
                            BinOp::LtEq => "<=",
                            _ => ">=",
                        };
                        format!("(lo_str_cmp({}, {}) {} 0)", lv, rv, cmp_op)
                    }
                    _ => {
                        let c_op = match op {
                            BinOp::Add => "+",
                            BinOp::Sub => "-",
                            BinOp::Mul => "*",
                            BinOp::Div => "/",
                            BinOp::Rem => "%",
                            BinOp::BitAnd => "&",
                            BinOp::BitOr => "|",
                            BinOp::BitXor => "^",
                            BinOp::BitShl => "<<",
                            BinOp::BitShr => ">>",
                            BinOp::Lt => "<",
                            BinOp::Gt => ">",
                            BinOp::LtEq => "<=",
                            BinOp::GtEq => ">=",
                            _ => "+",
                        };
                        if matches!(op, BinOp::Div | BinOp::Rem) && ety.is_integer() {
                            let dtmp = self.fresh_tmp("div");
                            let _ = writeln!(out, "    {} {} = {};", c_type_name(&rty), dtmp, rv);
                            let _ = writeln!(
                                out,
                                "    if ({} == 0) lo_panic(\"division by zero\", {});",
                                dtmp, expr.span.line
                            );
                            format!("(({}) {} ({}))", lv, c_op, dtmp)
                        } else {
                            format!("(({}) {} ({}))", lv, c_op, rv)
                        }
                    }
                }
            }
            ExprKind::Field {
                expr: base,
                field,
                safe,
            } => {
                // Qualified unit enum variant `State.Idle`
                if let ExprKind::TypeRef { name: tname, .. } = &base.kind {
                    if let Some(en) = self.prog.enums.get(tname) {
                        if let Some(v) = en.variants.iter().find(|v| &v.name == field) {
                            return format!("((lo_en_{}){{ .tag = {}u }})", tname, v.tag);
                        }
                    }
                }
                let bty = self.expr_ty(base.span);
                if let Type::Module(ref mod_id) = bty {
                    if mod_id == "builtin_std_math" {
                        return match field.as_str() {
                            "pi" => "3.14159265358979323846".into(),
                            "e" => "2.71828182845904523536".into(),
                            _ => "6.28318530717958647692".into(),
                        };
                    }
                    return format!("lo_g_{}_{}", mod_id, sanitize_ident(field));
                }

                let bv = self.emit_expr(base, out);
                if *safe || bty.is_nullable() {
                    let res = self.fresh_tmp("sf");
                    let btmp = self.fresh_tmp("sb");
                    let _ = writeln!(out, "    {} {} = {};", c_type_name(&bty), btmp, bv);
                    let _ = writeln!(
                        out,
                        "    {} {} = {{ .has_val = false }};",
                        c_type_name(&ety),
                        res
                    );
                    let inner_bty = bty.unwrap_nullable().unwrap_or(&bty);
                    let cond = if bty.is_nullable() {
                        format!("{}.has_val", btmp)
                    } else {
                        "true".into()
                    };
                    let inner_expr = if bty.is_nullable() {
                        format!("{}.val", btmp)
                    } else {
                        btmp
                    };
                    let _ = writeln!(out, "    if ({}) {{", cond);
                    let raw_access = self.emit_direct_field(&inner_expr, inner_bty, field);
                    let raw_fty = match inner_bty {
                        Type::Struct(sn) => self
                            .prog
                            .structs
                            .get(sn)
                            .and_then(|st| st.fields.iter().find(|f| &f.name == field))
                            .map(|f| f.ty.clone())
                            .unwrap_or_else(|| ety.unwrap_nullable().unwrap_or(&ety).clone()),
                        _ => ety.unwrap_nullable().unwrap_or(&ety).clone(),
                    };
                    let coerced = self.coerce(&raw_access, &raw_fty, &ety, out);
                    let _ = writeln!(out, "        {} = {};", res, coerced);
                    let _ = writeln!(out, "    }}");
                    res
                } else {
                    self.emit_direct_field(&bv, &bty, field)
                }
            }
            ExprKind::Index {
                expr: base,
                index,
                safe,
            } => {
                let bty = self.expr_ty(base.span);
                let bv = self.emit_expr(base, out);
                let iv = self.emit_expr(index, out);
                let ity = self.expr_ty(index.span);
                let unwrapped_b = bty.unwrap_nullable().unwrap_or(&bty).clone();

                match unwrapped_b {
                    Type::Map(k_ty, v_ty) => {
                        let res = self.fresh_tmp("mget");
                        let btmp = self.fresh_tmp("mb");
                        let ktmp = self.fresh_tmp("mk");
                        let ptmp = self.fresh_tmp("mp");
                        let kco = self.coerce(&iv, &ity, &k_ty, out);
                        let _ = writeln!(out, "    {} {} = {};", c_type_name(&bty), btmp, bv);
                        let _ = writeln!(out, "    {} {} = {};", c_type_name(&k_ty), ktmp, kco);
                        let _ = writeln!(
                            out,
                            "    {} {} = {{ .has_val = false }};",
                            c_type_name(&ety),
                            res
                        );
                        let map_ptr = if bty.is_nullable() {
                            format!("({}.has_val ? {}.val : NULL)", btmp, btmp)
                        } else {
                            btmp
                        };
                        let _ = writeln!(
                            out,
                            "    void* {} = lo_map_get({}, &{});",
                            ptmp, map_ptr, ktmp
                        );
                        let _ = writeln!(out, "    if ({}) {{", ptmp);
                        let raw_mv = format!("(*({}*){})", c_type_name(&v_ty), ptmp);
                        let coerced_mv = self.coerce(&raw_mv, &v_ty, &ety, out);
                        let _ = writeln!(out, "        {} = {};", res, coerced_mv);
                        let _ = writeln!(out, "    }}");
                        res
                    }
                    Type::Array(elem_ty) => {
                        if *safe || bty.is_nullable() {
                            let res = self.fresh_tmp("sidx");
                            let btmp = self.fresh_tmp("sb");
                            let itmp = self.fresh_tmp("si");
                            let _ = writeln!(out, "    {} {} = {};", c_type_name(&bty), btmp, bv);
                            let _ = writeln!(out, "    int64_t {} = (int64_t)({});", itmp, iv);
                            let _ = writeln!(
                                out,
                                "    {} {} = {{ .has_val = false }};",
                                c_type_name(&ety),
                                res
                            );
                            let vptr = if bty.is_nullable() {
                                let _ = writeln!(out, "    if ({}.has_val && {}.val && {} >= 0 && {} < {}.val->len) {{", btmp, btmp, itmp, itmp, btmp);
                                format!("{}.val", btmp)
                            } else {
                                let _ = writeln!(
                                    out,
                                    "    if ({} && {} >= 0 && {} < {}->len) {{",
                                    btmp, itmp, itmp, btmp
                                );
                                btmp
                            };
                            let raw_el = format!(
                                "(*({}*)lo_vec_at({}, {}, {}))",
                                c_type_name(&elem_ty),
                                vptr,
                                itmp,
                                expr.span.line
                            );
                            let coerced = self.coerce(&raw_el, &elem_ty, &ety, out);
                            let _ = writeln!(out, "        {} = {};", res, coerced);
                            let _ = writeln!(out, "    }}");
                            res
                        } else {
                            format!(
                                "(*({}*)lo_vec_at({}, (int64_t)({}), {}))",
                                c_type_name(&elem_ty),
                                bv,
                                iv,
                                expr.span.line
                            )
                        }
                    }
                    Type::Str => {
                        if *safe || bty.is_nullable() {
                            let res = self.fresh_tmp("sch");
                            let btmp = self.fresh_tmp("sb");
                            let itmp = self.fresh_tmp("si");
                            let _ = writeln!(out, "    {} {} = {};", c_type_name(&bty), btmp, bv);
                            let _ = writeln!(out, "    int64_t {} = (int64_t)({});", itmp, iv);
                            let _ = writeln!(
                                out,
                                "    {} {} = {{ .has_val = false }};",
                                c_type_name(&ety),
                                res
                            );
                            let s_expr = if bty.is_nullable() {
                                format!("{}.val", btmp)
                            } else {
                                btmp.clone()
                            };
                            let has_c = if bty.is_nullable() {
                                format!("{}.has_val && ", btmp)
                            } else {
                                String::new()
                            };
                            let _ = writeln!(
                                out,
                                "    if ({}{} >= 0 && {} < {}.len) {{ {}.has_val = true; {}.val = (uint8_t){}.data[{}]; }}",
                                has_c, itmp, itmp, s_expr, res, res, s_expr, itmp
                            );
                            res
                        } else {
                            format!(
                                "lo_str_char_at({}, (int64_t)({}), {})",
                                bv, iv, expr.span.line
                            )
                        }
                    }
                    _ => "lo_unit_val()".into(),
                }
            }
            ExprKind::Slice {
                expr: base,
                start,
                end,
                inclusive,
            } => {
                let bty = self.expr_ty(base.span);
                let bv = self.emit_expr(base, out);
                let sv = start
                    .as_ref()
                    .map(|s| self.emit_expr(s, out))
                    .unwrap_or_else(|| "0".into());
                let btmp = self.fresh_tmp("slb");
                let _ = writeln!(out, "    {} {} = {};", c_type_name(&bty), btmp, bv);
                let default_end = if bty == Type::Str {
                    format!("{}.len", btmp)
                } else {
                    format!("({} ? {}->len : 0)", btmp, btmp)
                };
                let ev = end
                    .as_ref()
                    .map(|e| self.emit_expr(e, out))
                    .unwrap_or(default_end);
                let inc = if *inclusive && end.is_some() {
                    "true"
                } else {
                    "false"
                };
                if bty == Type::Str {
                    format!(
                        "lo_str_slice({}, (int64_t)({}), (int64_t)({}), {})",
                        btmp, sv, ev, inc
                    )
                } else {
                    format!(
                        "lo_vec_slice({}, (int64_t)({}), (int64_t)({}), {})",
                        btmp, sv, ev, inc
                    )
                }
            }
            ExprKind::Call {
                callee, args, safe, ..
            } => self.emit_call(callee, args, *safe, expr.span, &ety, out),
            ExprKind::Cast {
                expr: inner, safe, ..
            } => {
                let iv = self.emit_expr(inner, out);
                let ity = self.expr_ty(inner.span);
                if *safe {
                    let target_inner = ety.unwrap_nullable().unwrap_or(&ety).clone();
                    let res = self.fresh_tmp("scast");
                    let _ = writeln!(
                        out,
                        "    {} {} = {{ .has_val = false }};",
                        c_type_name(&ety),
                        res
                    );
                    if ity == target_inner {
                        let _ = writeln!(out, "    {}.has_val = true; {}.val = {};", res, res, iv);
                    } else if let Type::Nullable(ref u_ity) = ity {
                        if **u_ity == target_inner {
                            let _ = writeln!(out, "    {} = {};", res, iv);
                        }
                    }
                    res
                } else {
                    format!("(({})({}))", c_type_name(&ety), iv)
                }
            }
            ExprKind::NonNullAssert(inner) => {
                let iv = self.emit_expr(inner, out);
                let ity = self.expr_ty(inner.span);
                if ity.is_nullable() {
                    let tmp = self.fresh_tmp("nn");
                    let _ = writeln!(out, "    {} {} = {};", c_type_name(&ity), tmp, iv);
                    let _ = writeln!(
                        out,
                        "    if (!{}.has_val) lo_panic(\"null-assertion `!!` failed on `~`\", {});",
                        tmp, expr.span.line
                    );
                    format!("{}.val", tmp)
                } else {
                    iv
                }
            }
            ExprKind::TryPropagate(inner) => {
                let iv = self.emit_expr(inner, out);
                let ity = self.expr_ty(inner.span);
                let tmp = self.fresh_tmp("tryp");
                let _ = writeln!(out, "    {} {} = {};", c_type_name(&ity), tmp, iv);
                let _ = writeln!(out, "    if (_lo_err.active) {{");
                self.emit_error_unwind(out);
                let _ = writeln!(out, "    }}");
                if ity.is_nullable() && !ety.is_nullable() {
                    let _ = writeln!(out, "    if (!{}.has_val) {{", tmp);
                    self.emit_active_defers(out, false);
                    let _ = writeln!(out, "        return {};", c_default_zero(&self.ret_ty));
                    let _ = writeln!(out, "    }}");
                    format!("{}.val", tmp)
                } else {
                    tmp
                }
            }
            ExprKind::Elvis { lhs, rhs } => {
                let lty = self.expr_ty(lhs.span);
                let res = self.fresh_tmp("elvis");
                let ltmp = self.fresh_tmp("elvl");
                let err_lbl = self.fresh_tmp("elv_err");
                let _ = writeln!(
                    out,
                    "    {} {} = {};",
                    c_type_name(&lty),
                    ltmp,
                    c_default_zero(&lty)
                );
                let _ = writeln!(
                    out,
                    "    {} {} = {};",
                    c_type_name(&ety),
                    res,
                    c_default_zero(&ety)
                );
                self.try_catch_stack
                    .push((err_lbl.clone(), self.defers.len()));
                let lv = self.emit_expr(lhs, out);
                self.try_catch_stack.pop();
                let _ = writeln!(out, "    {} = {};", ltmp, lv);
                let _ = writeln!(out, "    {}: ;", err_lbl);
                let has_ok = if lty.is_nullable() {
                    format!("(!_lo_err.active && {}.has_val)", ltmp)
                } else {
                    "(!_lo_err.active)".into()
                };
                let _ = writeln!(out, "    if ({}) {{", has_ok);
                let unwrapped_val = if let Type::Nullable(ref inner) = lty {
                    self.coerce(&format!("{}.val", ltmp), inner, &ety, out)
                } else {
                    self.coerce(&ltmp, &lty, &ety, out)
                };
                let _ = writeln!(out, "        {} = {};", res, unwrapped_val);
                let _ = writeln!(out, "    }} else {{");
                let _ = writeln!(out, "        lo_err_clear();");
                match rhs {
                    ElvisRhs::Flow(flow) => {
                        self.emit_flow(flow, expr.span, out);
                    }
                    ElvisRhs::Expr(re) => {
                        let rv = self.emit_expr(re, out);
                        let rty = self.expr_ty(re.span);
                        let rco = self.coerce(&rv, &rty, &ety, out);
                        let _ = writeln!(out, "        {} = {};", res, rco);
                    }
                }
                let _ = writeln!(out, "    }}");
                res
            }
            ExprKind::ErrorFallback { lhs, body } => {
                let lty = self.expr_ty(lhs.span);
                let res = self.fresh_tmp("efall");
                let ltmp = self.fresh_tmp("efl");
                let err_lbl = self.fresh_tmp("ef_err");
                let _ = writeln!(
                    out,
                    "    {} {} = {};",
                    c_type_name(&lty),
                    ltmp,
                    c_default_zero(&lty)
                );
                let _ = writeln!(
                    out,
                    "    {} {} = {};",
                    c_type_name(&ety),
                    res,
                    c_default_zero(&ety)
                );
                self.try_catch_stack
                    .push((err_lbl.clone(), self.defers.len()));
                let lv = self.emit_expr(lhs, out);
                self.try_catch_stack.pop();
                let _ = writeln!(out, "    {} = {};", ltmp, lv);
                let _ = writeln!(out, "    {}: ;", err_lbl);
                let _ = writeln!(out, "    if (!_lo_err.active) {{");
                let lco = self.coerce(&ltmp, &lty, &ety, out);
                let _ = writeln!(out, "        {} = {};", res, lco);
                let _ = writeln!(out, "    }} else {{");
                let err_snap = self.fresh_tmp("err");
                let _ = writeln!(out, "        lo_error_t {} = _lo_err;", err_snap);
                let _ = writeln!(out, "        _lo_err.payload = NULL; lo_err_clear();");
                self.current_item_stack
                    .push((err_snap, Type::ErrorAny, false));
                let pipe_lbl = self.fresh_tmp("ef_done");
                self.pipe_stack.push(PipeLocalTarget {
                    cont_label: pipe_lbl.clone(),
                    res_var: Some((res.clone(), ety.clone())),
                });
                let bv = self.emit_block(body, &ety, out);
                self.pipe_stack.pop();
                self.current_item_stack.pop();
                let _ = writeln!(out, "        {} = {};", res, bv);
                let _ = writeln!(out, "        {}: ;", pipe_lbl);
                let _ = writeln!(out, "    }}");
                res
            }
            ExprKind::GuardReturn { cond, ret } => {
                let cv = self.emit_expr(cond, out);
                let _ = writeln!(out, "    if ({}) {{", cv);
                match ret {
                    ElvisRhs::Flow(flow) => {
                        self.emit_flow(flow, expr.span, out);
                    }
                    ElvisRhs::Expr(re) => {
                        let rv = self.emit_expr(re, out);
                        let rty = self.expr_ty(re.span);
                        let ret_ty = self.ret_ty.clone();
                        let rco = self.coerce(&rv, &rty, &ret_ty, out);
                        let rtmp = self.fresh_tmp("gret");
                        let _ =
                            writeln!(out, "        {} {} = {};", c_type_name(&ret_ty), rtmp, rco);
                        self.emit_active_defers(out, false);
                        let _ = writeln!(out, "        return {};", rtmp);
                    }
                }
                let _ = writeln!(out, "    }}");
                "lo_unit_val()".into()
            }
            ExprKind::Pipe { lhs, kind, stage } => {
                self.emit_pipe(lhs, *kind, stage, expr.span, &ety, out)
            }
            ExprKind::Func(_) => {
                if let Some(target_c) = self.call_targets.get(&span_key(expr.span)) {
                    return target_c.clone();
                }
                "NULL".into()
            }
            ExprKind::If {
                branches,
                else_block,
            } => self.emit_if(branches, else_block.as_ref(), &ety, out),
            ExprKind::Match { scrutinee, arms } => self.emit_match(scrutinee, arms, &ety, out),
            ExprKind::Try {
                body,
                catches,
                finally_block,
            } => self.emit_try(body, catches, finally_block.as_ref(), &ety, out),
            ExprKind::Loop { head, body } => self.emit_loop(head, body, &ety, None, out),
            ExprKind::Labeled { label, expr: inner } => self.emit_labeled(label, inner, &ety, out),
            ExprKind::BlockExpr(blk) => self.emit_block(blk, &ety, out),
            ExprKind::AsyncSpawn(blk) => self.emit_async_spawn(blk, &ety, out),
            ExprKind::Comptime(_) | ExprKind::Import(_) | ExprKind::ExternC { .. } => {
                "lo_unit_val()".into()
            }
        }
    }

    fn emit_direct_field(&self, base_c: &str, base_ty: &Type, field: &str) -> String {
        match base_ty {
            Type::Struct(_) => format!("({}).f_{}", base_c, sanitize_ident(field)),
            Type::Str if field == "len" => format!("({}).len", base_c),
            Type::Array(_) | Type::Map(_, _) | Type::Set(_) if field == "len" => {
                format!("({} ? ({})->len : 0)", base_c, base_c)
            }
            Type::ErrorAny => match field {
                "code" => format!("({}).code", base_c),
                _ => format!("({}).msg", base_c),
            },
            _ => "lo_unit_val()".into(),
        }
    }

    fn emit_struct_or_variant_lit(
        &mut self,
        spread: Option<&Expr>,
        fields: &[(String, Expr)],
        span: Span,
        ety: &Type,
        out: &mut String,
    ) -> String {
        // Check Enum Struct-Variant `Attack[target = 1, dmg = 50]`
        if let Some(target) = self.struct_targets.get(&span_key(span)).cloned() {
            if let Some((ename, vname)) = target.split_once("::") {
                if let Some(en) = self.prog.enums.get(ename) {
                    if let Some(v) = en.variants.iter().find(|v| v.name == vname) {
                        let tmp = self.fresh_tmp("env");
                        let _ = writeln!(
                            out,
                            "    lo_en_{} {} = {{ .tag = {}u }};",
                            ename, tmp, v.tag
                        );
                        if let CheckedVariantPayload::Struct(ref vfs) = v.payload {
                            for vf in vfs {
                                if let Some((_, fexpr)) = fields.iter().find(|(n, _)| n == &vf.name)
                                {
                                    let fv = self.emit_expr(fexpr, out);
                                    let fraw = self.expr_ty(fexpr.span);
                                    let fco = self.coerce(&fv, &fraw, &vf.ty, out);
                                    let _ = writeln!(
                                        out,
                                        "    {}.u.{}.f_{} = {};",
                                        tmp,
                                        vname,
                                        sanitize_ident(&vf.name),
                                        fco
                                    );
                                }
                            }
                        }
                        return tmp;
                    }
                }
            }
        }

        let Type::Struct(ref sname) = ety else {
            return "lo_unit_val()".into();
        };
        let Some(schema) = self.prog.structs.get(sname).cloned() else {
            return "lo_unit_val()".into();
        };

        let tmp = self.fresh_tmp("st");
        if let Some(sp_expr) = spread {
            let sp_v = self.emit_expr(sp_expr, out);
            // Perceus structural update in-place copy (`[..user, hp = user.hp - 25]`)
            let _ = writeln!(out, "    lo_st_{} {} = {};", sname, tmp, sp_v);
            let _ = writeln!(out, "    atomic_fetch_add(&_lo_perceus_reuses, 1);");
        } else {
            let _ = writeln!(out, "    lo_st_{} {} = {{0}};", sname, tmp);
            for sf in &schema.fields {
                if fields.iter().any(|(n, _)| n == &sf.name) {
                    continue;
                }
                if let Some(def_e) = &sf.default {
                    let dco = if matches!(def_e.kind, ExprKind::Null) && sf.ty.is_nullable() {
                        format!("(({}){{ .has_val = false }})", c_type_name(&sf.ty))
                    } else {
                        let dv = self.emit_expr(def_e, out);
                        let draw = self
                            .expr_types
                            .get(&span_key(def_e.span))
                            .cloned()
                            .unwrap_or_else(|| match &def_e.kind {
                                ExprKind::Null => Type::Null,
                                ExprKind::Unit => Type::Unit,
                                ExprKind::Bool(_) => Type::Bool,
                                ExprKind::Int(_) => Type::I64,
                                ExprKind::Float(_) => Type::F64,
                                ExprKind::Char(_) => Type::Char,
                                ExprKind::Str(_) | ExprKind::InterpString(_) => Type::Str,
                                _ => sf.ty.clone(),
                            });
                        self.coerce(&dv, &draw, &sf.ty, out)
                    };
                    let _ = writeln!(out, "    {}.f_{} = {};", tmp, sanitize_ident(&sf.name), dco);
                } else {
                    let _ = writeln!(
                        out,
                        "    {}.f_{} = {};",
                        tmp,
                        sanitize_ident(&sf.name),
                        c_default_zero(&sf.ty)
                    );
                }
            }
        }

        for (fname, fexpr) in fields {
            if let Some(sf) = schema.fields.iter().find(|f| &f.name == fname) {
                let fv = self.emit_expr(fexpr, out);
                let fraw = self.expr_ty(fexpr.span);
                let fco = self.coerce(&fv, &fraw, &sf.ty, out);
                let _ = writeln!(out, "    {}.f_{} = {};", tmp, sanitize_ident(fname), fco);
            }
        }
        tmp
    }

    fn emit_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        safe: bool,
        span: Span,
        ety: &Type,
        out: &mut String,
    ) -> String {
        // 1. Channel constructor `[~i64]()` or Set constructor `[%str]()`
        if let ExprKind::TypeVal(_) = &callee.kind {
            return match ety {
                Type::Channel(el) => format!("lo_chan_new(sizeof({}))", c_type_name(el)),
                Type::Set(el) => format!(
                    "lo_map_new(sizeof({}), 0, {})",
                    c_type_name(el),
                    if **el == Type::Str { "true" } else { "false" }
                ),
                _ => "lo_unit_val()".into(),
            };
        }

        let target = self.call_targets.get(&span_key(span)).cloned();

        // 2. Enum Tuple Variant constructor `Run(3.5)`
        if let Some(ref t) = target {
            if let Some(rest) = t.strip_prefix("__enum_") {
                if let Type::Enum(ref ename) = ety {
                    if let Some(vname) = rest.strip_prefix(&format!("{}_", ename)) {
                        if let Some(en) = self.prog.enums.get(ename) {
                            if let Some(v) = en.variants.iter().find(|v| v.name == vname) {
                                let tmp = self.fresh_tmp("etv");
                                let _ = writeln!(
                                    out,
                                    "    lo_en_{} {} = {{ .tag = {}u }};",
                                    ename, tmp, v.tag
                                );
                                if let CheckedVariantPayload::Tuple(ref vtys) = v.payload {
                                    for (i, (a, vt)) in args.iter().zip(vtys.iter()).enumerate() {
                                        let av = self.emit_expr(a, out);
                                        let araw = self.expr_ty(a.span);
                                        let aco = self.coerce(&av, &araw, vt, out);
                                        let _ = writeln!(
                                            out,
                                            "    {}.u.{}.v{} = {};",
                                            tmp, vname, i, aco
                                        );
                                    }
                                }
                                return tmp;
                            }
                        }
                    }
                }
            }
        }

        // 3. Builtin `print` / `println` / `assert`
        if let Some(ref t) = target {
            if t == "__builtin_print" || t == "__builtin_println" {
                let mut parts = Vec::with_capacity(args.len());
                for a in args {
                    let av = self.emit_expr(a, out);
                    let aty = self.expr_ty(a.span);
                    parts.push(self.emit_to_str(&av, &aty, out));
                }
                let joined = if parts.is_empty() {
                    "lo_str_lit(\"\", 0)".to_string()
                } else if parts.len() == 1 {
                    parts.remove(0)
                } else {
                    let parr = self.fresh_tmp("pp");
                    let _ = writeln!(
                        out,
                        "    lo_str {}[{}] = {{ {} }};",
                        parr,
                        parts.len(),
                        parts.join(", ")
                    );
                    format!("lo_str_join_parts({}, {})", parr, parts.len())
                };
                if t == "__builtin_println" {
                    let _ = writeln!(out, "    lo_rt_println_str(lo_cstr({}));", joined);
                } else {
                    // Notice in lo.md `print(...)` prints a line in examples like `0..5 ~> { print("Шаг {@}") }`!
                    let _ = writeln!(out, "    lo_rt_println_str(lo_cstr({}));", joined);
                }
                return "lo_unit_val()".into();
            }
            if t == "__builtin_assert" {
                if let Some(first) = args.first() {
                    let cv = self.emit_expr(first, out);
                    let _ = writeln!(
                        out,
                        "    if (!({})) lo_panic(\"assertion failed\", {});",
                        cv, span.line
                    );
                }
                return "lo_unit_val()".into();
            }
        }

        // 4. Builtin methods on str / array / set / map / channel or math fallback
        if let Some(ref t) = target {
            if let Some(m) = t.strip_prefix("__math_") {
                let mut avs = Vec::new();
                for a in args {
                    avs.push(self.emit_expr(a, out));
                }
                return match m {
                    "sin" | "cos" | "tan" | "sqrt" | "floor" | "ceil" | "round" => {
                        format!("{}((double)({}))", m, avs[0])
                    }
                    "abs" => format!("fabs((double)({}))", avs[0]),
                    "pow" => format!("pow((double)({}), (double)({}))", avs[0], avs[1]),
                    "min" => format!("fmin((double)({}), (double)({}))", avs[0], avs[1]),
                    "max" => format!("fmax((double)({}), (double)({}))", avs[0], avs[1]),
                    "clamp" => format!(
                        "fmin(fmax((double)({}), (double)({})), (double)({}))",
                        avs[0], avs[1], avs[2]
                    ),
                    _ => "0.0".into(),
                };
            }

            if t.starts_with("__str_")
                || t.starts_with("__arr_")
                || t.starts_with("__set_")
                || t.starts_with("__map_")
                || t == "__chan_close"
            {
                if let ExprKind::Field {
                    expr: recv_expr,
                    safe: field_safe,
                    ..
                } = &callee.kind
                {
                    return self.emit_builtin_method_call(
                        t,
                        recv_expr,
                        *field_safe || safe,
                        args,
                        ety,
                        out,
                    );
                }
            }

            // 5. Extern C FFI call `<"c" "symbol" (...) [...]>`
            if let Some(c_sym) = t.strip_prefix("__extern_") {
                let ext_sig = self
                    .prog
                    .externs
                    .iter()
                    .find(|e| e.c_symbol == c_sym)
                    .cloned();
                let mut c_args = Vec::with_capacity(args.len());
                for (i, a) in args.iter().enumerate() {
                    let av = self.emit_expr(a, out);
                    let aty = self.expr_ty(a.span);
                    let pty = ext_sig
                        .as_ref()
                        .and_then(|s| s.params.get(i))
                        .unwrap_or(&aty);
                    if *pty == Type::Str {
                        c_args.push(format!("lo_cstr({})", av));
                    } else {
                        c_args.push(self.coerce(&av, &aty, pty, out));
                    }
                }
                let raw_call = format!("{}({})", c_sym, c_args.join(", "));
                let ext_res = if *ety == Type::Unit {
                    let _ = writeln!(out, "    {};", raw_call);
                    "lo_unit_val()".to_string()
                } else {
                    let ext_val = if *ety == Type::Str
                        && !c_sym.starts_with("lo_rt_")
                        && !c_sym.starts_with("lo_str_")
                    {
                        format!("lo_str_from_cstr({})", raw_call)
                    } else {
                        raw_call
                    };
                    let tmp = self.fresh_tmp("ext");
                    let _ = writeln!(out, "    {} {} = {};", c_type_name(ety), tmp, ext_val);
                    tmp
                };
                if !self.masking_defers {
                    let _ = writeln!(out, "    if (_lo_err.active) {{");
                    self.emit_error_unwind(out);
                    let _ = writeln!(out, "    }}");
                }
                return ext_res;
            }
        }

        // 6. User-defined Method Call `obj.method(args)` or `obj?.method(args)` or `obj.method!(args)`
        if let ExprKind::Field {
            expr: recv_expr,
            safe: field_safe,
            ..
        } = &callee.kind
        {
            if let Some(ref fn_c_name) = target {
                if let Some(mf) = self
                    .prog
                    .funcs
                    .iter()
                    .find(|f| &f.c_name == fn_c_name && f.receiver.is_some())
                    .cloned()
                {
                    let recv_ty = self.expr_ty(recv_expr.span);
                    let is_mut = mf.receiver.as_ref().map(|(_, m)| *m).unwrap_or(false);
                    if is_mut {
                        let ptr_expr = self.emit_lvalue_addr(recv_expr, out);
                        let mut call_args = vec![ptr_expr];
                        for (idx, (_, pty, pdef)) in mf.params.iter().enumerate() {
                            call_args.push(self.emit_call_arg(
                                args.get(idx),
                                pty,
                                pdef.as_ref(),
                                out,
                            ));
                        }
                        let tmp = self.fresh_tmp("mcall");
                        let _ = writeln!(
                            out,
                            "    {} {} = {}({});",
                            c_type_name(&mf.ret_ty),
                            tmp,
                            fn_c_name,
                            call_args.join(", ")
                        );
                        if !self.masking_defers {
                            let _ = writeln!(out, "    if (_lo_err.active) {{");
                            self.emit_error_unwind(out);
                            let _ = writeln!(out, "    }}");
                        }
                        return self.coerce(&tmp, &mf.ret_ty, ety, out);
                    } else if *field_safe || safe || recv_ty.is_nullable() {
                        let r_val = self.emit_expr(recv_expr, out);
                        let rtmp = self.fresh_tmp("mrecv");
                        let res = self.fresh_tmp("msafe");
                        let _ =
                            writeln!(out, "    {} {} = {};", c_type_name(&recv_ty), rtmp, r_val);
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
                        let cond = if recv_ty.is_nullable() {
                            format!("{}.has_val", rtmp)
                        } else {
                            "true".into()
                        };
                        let unwrapped_r = if recv_ty.is_nullable() {
                            format!("{}.val", rtmp)
                        } else {
                            rtmp
                        };
                        let _ = writeln!(out, "    if ({}) {{", cond);
                        let mut call_args = vec![unwrapped_r];
                        for (idx, (_, pty, pdef)) in mf.params.iter().enumerate() {
                            call_args.push(self.emit_call_arg(
                                args.get(idx),
                                pty,
                                pdef.as_ref(),
                                out,
                            ));
                        }
                        let raw_tmp = self.fresh_tmp("msraw");
                        let _ = writeln!(
                            out,
                            "        {} {} = {}({});",
                            c_type_name(&mf.ret_ty),
                            raw_tmp,
                            fn_c_name,
                            call_args.join(", ")
                        );
                        if !self.masking_defers {
                            let _ = writeln!(out, "        if (_lo_err.active) {{");
                            self.emit_error_unwind(out);
                            let _ = writeln!(out, "        }}");
                        }
                        let coerced_r = self.coerce(&raw_tmp, &mf.ret_ty, ety, out);
                        let _ = writeln!(out, "        {} = {};", res, coerced_r);
                        let _ = writeln!(out, "    }}");
                        return res;
                    } else {
                        let r_val = self.emit_expr(recv_expr, out);
                        let mut call_args = vec![r_val];
                        for (idx, (_, pty, pdef)) in mf.params.iter().enumerate() {
                            call_args.push(self.emit_call_arg(
                                args.get(idx),
                                pty,
                                pdef.as_ref(),
                                out,
                            ));
                        }
                        let tmp = self.fresh_tmp("mcall");
                        let _ = writeln!(
                            out,
                            "    {} {} = {}({});",
                            c_type_name(&mf.ret_ty),
                            tmp,
                            fn_c_name,
                            call_args.join(", ")
                        );
                        if !self.masking_defers {
                            let _ = writeln!(out, "    if (_lo_err.active) {{");
                            self.emit_error_unwind(out);
                            let _ = writeln!(out, "    }}");
                        }
                        return self.coerce(&tmp, &mf.ret_ty, ety, out);
                    }
                }
            }
        }

        // 7. Direct Monomorphized Function Call or `$` Recursive Self-Call or Function Pointer Call
        let fn_c_expr = if matches!(callee.kind, ExprKind::SelfFn) {
            self.self_fn_c_name.clone().unwrap_or_else(|| "NULL".into())
        } else if let Some(t) = target {
            t
        } else {
            self.emit_expr(callee, out)
        };

        let mf_opt = self
            .prog
            .funcs
            .iter()
            .find(|f| f.c_name == fn_c_expr)
            .cloned();

        let mut c_args = Vec::new();
        if let Some(ref mf) = mf_opt {
            for (idx, (_, pty, pdef)) in mf.params.iter().enumerate() {
                c_args.push(self.emit_call_arg(args.get(idx), pty, pdef.as_ref(), out));
            }
        } else {
            for a in args {
                c_args.push(self.emit_expr(a, out));
            }
        }

        let callee_ty = self.expr_ty(callee.span);
        if safe && callee_ty.is_nullable() {
            let ftmp = self.fresh_tmp("fopt");
            let res = self.fresh_tmp("fres");
            let _ = writeln!(
                out,
                "    {} {} = {};",
                c_type_name(&callee_ty),
                ftmp,
                fn_c_expr
            );
            let _ = writeln!(
                out,
                "    {} {} = {{ .has_val = false }};",
                c_type_name(ety),
                res
            );
            let _ = writeln!(out, "    if ({}.has_val) {{", ftmp);
            let inner_ret = ety.unwrap_nullable().unwrap_or(ety).clone();
            let raw_c = format!("{}.val({})", ftmp, c_args.join(", "));
            let co = self.coerce(&raw_c, &inner_ret, ety, out);
            let _ = writeln!(out, "        {} = {};", res, co);
            if !self.masking_defers {
                let _ = writeln!(out, "        if (_lo_err.active) {{");
                self.emit_error_unwind(out);
                let _ = writeln!(out, "        }}");
            }
            let _ = writeln!(out, "    }}");
            return res;
        }

        let tmp = self.fresh_tmp("call");
        let call_ret_ty = mf_opt.map(|m| m.ret_ty).unwrap_or_else(|| ety.clone());
        let _ = writeln!(
            out,
            "    {} {} = {}({});",
            c_type_name(&call_ret_ty),
            tmp,
            fn_c_expr,
            c_args.join(", ")
        );
        if !self.masking_defers {
            let _ = writeln!(out, "    if (_lo_err.active) {{");
            self.emit_error_unwind(out);
            let _ = writeln!(out, "    }}");
        }
        self.coerce(&tmp, &call_ret_ty, ety, out)
    }

    fn emit_call_arg(
        &mut self,
        passed: Option<&Expr>,
        pty: &Type,
        pdef: Option<&Expr>,
        out: &mut String,
    ) -> String {
        if let Some(a) = passed {
            let av = self.emit_expr(a, out);
            let aty = self.expr_ty(a.span);
            self.coerce(&av, &aty, pty, out)
        } else if let Some(def_e) = pdef {
            if matches!(def_e.kind, ExprKind::Null) && pty.is_nullable() {
                format!("(({}){{ .has_val = false }})", c_type_name(pty))
            } else {
                let dv = self.emit_expr(def_e, out);
                let draw = self
                    .expr_types
                    .get(&span_key(def_e.span))
                    .cloned()
                    .unwrap_or_else(|| match &def_e.kind {
                        ExprKind::Null => Type::Null,
                        ExprKind::Unit => Type::Unit,
                        ExprKind::Bool(_) => Type::Bool,
                        ExprKind::Int(_) => Type::I64,
                        ExprKind::Float(_) => Type::F64,
                        ExprKind::Char(_) => Type::Char,
                        ExprKind::Str(_) | ExprKind::InterpString(_) => Type::Str,
                        _ => pty.clone(),
                    });
                self.coerce(&dv, &draw, pty, out)
            }
        } else {
            c_default_zero(pty)
        }
    }

    fn emit_builtin_method_call(
        &mut self,
        target: &str,
        recv_expr: &Expr,
        is_safe: bool,
        args: &[Expr],
        ety: &Type,
        out: &mut String,
    ) -> String {
        let rty = self.expr_ty(recv_expr.span);
        let rv = self.emit_expr(recv_expr, out);
        let mut avs = Vec::with_capacity(args.len());
        for a in args {
            avs.push(self.emit_expr(a, out));
        }

        let rtmp = self.fresh_tmp("brecv");
        let _ = writeln!(out, "    {} {} = {};", c_type_name(&rty), rtmp, rv);

        let emit_core = |ctx: &mut Self,
                         recv_unwrapped: &str,
                         base_ty: &Type,
                         buf: &mut String|
         -> (String, Type) {
            match target {
                "__str_trim" => (format!("lo_str_trim({})", recv_unwrapped), Type::Str),
                "__str_upper" => (format!("lo_str_upper({})", recv_unwrapped), Type::Str),
                "__str_lower" => (format!("lo_str_lower({})", recv_unwrapped), Type::Str),
                "__str_contains" => (
                    format!("lo_str_contains({}, {})", recv_unwrapped, avs[0]),
                    Type::Bool,
                ),
                "__str_starts_with" => (
                    format!("lo_str_starts_with({}, {})", recv_unwrapped, avs[0]),
                    Type::Bool,
                ),
                "__str_ends_with" => (
                    format!("lo_str_ends_with({}, {})", recv_unwrapped, avs[0]),
                    Type::Bool,
                ),
                "__str_split" => (
                    format!("lo_str_split({}, {})", recv_unwrapped, avs[0]),
                    Type::Array(Box::new(Type::Str)),
                ),
                "__str_slice" => (
                    format!(
                        "lo_str_slice({}, (int64_t)({}), (int64_t)({}), false)",
                        recv_unwrapped, avs[0], avs[1]
                    ),
                    Type::Str,
                ),
                "__str_to_i64" => {
                    let opt_ty = Type::make_nullable(Type::I64);
                    let ptmp = ctx.fresh_tmp("pi64");
                    let _ = writeln!(
                        buf,
                        "    {} {} = {{ .has_val = false, .val = 0 }};",
                        c_type_name(&opt_ty),
                        ptmp
                    );
                    let _ = writeln!(
                        buf,
                        "    {}.has_val = lo_str_parse_i64({}, &{}.val);",
                        ptmp, recv_unwrapped, ptmp
                    );
                    (ptmp, opt_ty)
                }
                "__str_to_f64" => {
                    let opt_ty = Type::make_nullable(Type::F64);
                    let ptmp = ctx.fresh_tmp("pf64");
                    let _ = writeln!(
                        buf,
                        "    {} {} = {{ .has_val = false, .val = 0.0 }};",
                        c_type_name(&opt_ty),
                        ptmp
                    );
                    let _ = writeln!(
                        buf,
                        "    {}.has_val = lo_str_parse_f64({}, &{}.val);",
                        ptmp, recv_unwrapped, ptmp
                    );
                    (ptmp, opt_ty)
                }
                "__arr_push_mut" => {
                    let elem = match base_ty {
                        Type::Array(el) => (**el).clone(),
                        _ => Type::I64,
                    };
                    let etmp = ctx.fresh_tmp("apush");
                    let _ = writeln!(buf, "    {} {} = {};", c_type_name(&elem), etmp, avs[0]);
                    let _ = writeln!(buf, "    lo_vec_push({}, &{});", recv_unwrapped, etmp);
                    ("lo_unit_val()".into(), Type::Unit)
                }
                "__arr_pop_mut" => {
                    let elem = match base_ty {
                        Type::Array(el) => (**el).clone(),
                        _ => Type::I64,
                    };
                    let opt_ty = Type::make_nullable(elem);
                    let ptmp = ctx.fresh_tmp("apop");
                    let _ = writeln!(
                        buf,
                        "    {} {} = {{ .has_val = false }};",
                        c_type_name(&opt_ty),
                        ptmp
                    );
                    let _ = writeln!(
                        buf,
                        "    {}.has_val = lo_vec_pop({}, &{}.val);",
                        ptmp, recv_unwrapped, ptmp
                    );
                    (ptmp, opt_ty)
                }
                "__arr_is_empty" => (
                    format!("(!{} || ({})->len == 0)", recv_unwrapped, recv_unwrapped),
                    Type::Bool,
                ),
                "__set_has" | "__map_has" => {
                    let key_ty = match base_ty {
                        Type::Set(el) => (**el).clone(),
                        Type::Map(k, _) => (**k).clone(),
                        _ => Type::Str,
                    };
                    let ktmp = ctx.fresh_tmp("hk");
                    let _ = writeln!(buf, "    {} {} = {};", c_type_name(&key_ty), ktmp, avs[0]);
                    (
                        format!("lo_map_has({}, &{})", recv_unwrapped, ktmp),
                        Type::Bool,
                    )
                }
                "__set_add_mut" => {
                    let elem = match base_ty {
                        Type::Set(el) => (**el).clone(),
                        _ => Type::Str,
                    };
                    let ktmp = ctx.fresh_tmp("sak");
                    let _ = writeln!(buf, "    {} {} = {};", c_type_name(&elem), ktmp, avs[0]);
                    let _ = writeln!(buf, "    lo_map_set({}, &{}, NULL);", recv_unwrapped, ktmp);
                    ("lo_unit_val()".into(), Type::Unit)
                }
                "__set_remove_mut" | "__map_remove_mut" => {
                    let key_ty = match base_ty {
                        Type::Set(el) => (**el).clone(),
                        Type::Map(k, _) => (**k).clone(),
                        _ => Type::Str,
                    };
                    let ktmp = ctx.fresh_tmp("rk");
                    let _ = writeln!(buf, "    {} {} = {};", c_type_name(&key_ty), ktmp, avs[0]);
                    let _ = writeln!(buf, "    lo_map_remove({}, &{});", recv_unwrapped, ktmp);
                    ("lo_unit_val()".into(), Type::Unit)
                }
                "__chan_close" => {
                    let _ = writeln!(buf, "    lo_chan_close({});", recv_unwrapped);
                    ("lo_unit_val()".into(), Type::Unit)
                }
                _ => ("lo_unit_val()".into(), Type::Unit),
            }
        };

        let base_ty = rty.unwrap_nullable().unwrap_or(&rty).clone();
        if is_safe || rty.is_nullable() {
            let res = self.fresh_tmp("bsafe");
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
            let cond = if rty.is_nullable() {
                format!("{}.has_val", rtmp)
            } else {
                "true".into()
            };
            let unwrapped_s = if rty.is_nullable() {
                format!("{}.val", rtmp)
            } else {
                rtmp
            };
            let _ = writeln!(out, "    if ({}) {{", cond);
            let (core_val, core_ty) = emit_core(self, &unwrapped_s, &base_ty, out);
            let coerced = self.coerce(&core_val, &core_ty, ety, out);
            let _ = writeln!(out, "        {} = {};", res, coerced);
            let _ = writeln!(out, "    }}");
            res
        } else {
            let (core_val, core_ty) = emit_core(self, &rtmp, &base_ty, out);
            self.coerce(&core_val, &core_ty, ety, out)
        }
    }

    pub fn emit_lvalue_addr(&mut self, lv: &Expr, out: &mut String) -> String {
        match &lv.kind {
            ExprKind::Ident(name) => {
                if let Some((c_var, _, is_ptr)) = self.lookup_local(name) {
                    if *is_ptr {
                        return c_var.clone();
                    }
                    return format!("(&{})", c_var);
                }
                format!("(&lo_g_{})", sanitize_ident(name))
            }
            ExprKind::CurrentItem => {
                if let Some((cexpr, _, is_ptr)) = self.current_item_stack.last() {
                    if *is_ptr {
                        return cexpr.clone();
                    }
                    return format!("(&{})", cexpr);
                }
                "NULL".into()
            }
            ExprKind::Field {
                expr: base, field, ..
            } => {
                let base_ptr = self.emit_lvalue_addr(base, out);
                format!("(&({})->f_{})", base_ptr, sanitize_ident(field))
            }
            ExprKind::Index {
                expr: base, index, ..
            } => {
                let bty = self.expr_ty(base.span);
                let bv = self.emit_expr(base, out);
                let iv = self.emit_expr(index, out);
                if let Type::Array(el) = bty {
                    return format!(
                        "(({}*)lo_vec_at({}, (int64_t)({}), {}))",
                        c_type_name(&el),
                        bv,
                        iv,
                        lv.span.line
                    );
                }
                "NULL".into()
            }
            _ => "NULL".into(),
        }
    }
}

pub fn escape_c_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for b in s.bytes() {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7E => out.push(b as char),
            _ => {
                let _ = write!(out, "\\{:03o}", b);
            }
        }
    }
    out.push('"');
    out
}
