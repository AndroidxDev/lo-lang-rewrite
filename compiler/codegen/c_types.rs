use crate::sema::{CheckedEnum, CheckedProgram, CheckedStruct, CheckedVariantPayload, Type};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

pub struct TypeRegistry {
    pub optionals: Vec<Type>, // inner Type of Nullable(inner)
    pub opt_set: HashSet<String>,
    pub tuples: Vec<Vec<Type>>,
    pub tup_set: HashSet<String>,
    pub generators: Vec<Type>,
    pub gen_set: HashSet<String>,
    pub func_ptrs: Vec<(Vec<Type>, Type)>,
    pub fn_set: HashSet<String>,
}

impl Default for TypeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self {
            optionals: Vec::new(),
            opt_set: HashSet::new(),
            tuples: Vec::new(),
            tup_set: HashSet::new(),
            generators: Vec::new(),
            gen_set: HashSet::new(),
            func_ptrs: Vec::new(),
            fn_set: HashSet::new(),
        }
    }

    pub fn collect_from_program(&mut self, prog: &CheckedProgram) {
        for sname in &prog.struct_order {
            if let Some(st) = prog.structs.get(sname) {
                for f in &st.fields {
                    self.visit_type(&f.ty);
                }
            }
        }
        for en in prog.enums.values() {
            for v in &en.variants {
                match &v.payload {
                    CheckedVariantPayload::Unit => {}
                    CheckedVariantPayload::Tuple(ts) => {
                        for t in ts {
                            self.visit_type(t);
                        }
                    }
                    CheckedVariantPayload::Struct(fs) => {
                        for f in fs {
                            self.visit_type(&f.ty);
                        }
                    }
                }
            }
        }
        for g in &prog.globals {
            self.visit_type(&g.ty);
            for t in g.expr_types.values() {
                self.visit_type(t);
            }
        }
        for ext in &prog.externs {
            for p in &ext.params {
                self.visit_type(p);
            }
            self.visit_type(&ext.ret);
        }
        for f in &prog.funcs {
            if let Some((ref rt, _)) = f.receiver {
                self.visit_type(rt);
            }
            for (_, pt, _) in &f.params {
                self.visit_type(pt);
            }
            self.visit_type(&f.ret_ty);
            if let Some(ref yt) = f.yield_ty {
                self.visit_type(yt);
                self.visit_type(&Type::Generator(Box::new(yt.clone())));
            }
            for t in f.expr_types.values() {
                self.visit_type(t);
                // Safe index/map lookup may synthesize Nullable(elem)
                if let Type::Map(_, v) = t {
                    self.visit_type(&Type::make_nullable((**v).clone()));
                }
                if let Type::Channel(el) = t {
                    self.visit_type(&Type::make_nullable((**el).clone()));
                }
            }
        }
    }

    pub fn visit_type(&mut self, ty: &Type) {
        match ty {
            Type::Nullable(inner) => {
                self.visit_type(inner);
                let key = inner.mangle();
                if self.opt_set.insert(key) {
                    self.optionals.push((**inner).clone());
                }
            }
            Type::Array(inner)
            | Type::Set(inner)
            | Type::Range(inner)
            | Type::Task(inner)
            | Type::Channel(inner) => {
                self.visit_type(inner);
            }
            Type::Generator(inner) => {
                self.visit_type(inner);
                let key = inner.mangle();
                if self.gen_set.insert(key) {
                    self.generators.push((**inner).clone());
                }
            }
            Type::Map(k, v) => {
                self.visit_type(k);
                self.visit_type(v);
                self.visit_type(&Type::make_nullable((**v).clone()));
            }
            Type::Tuple(elems) => {
                for e in elems {
                    self.visit_type(e);
                }
                let key = ty.mangle();
                if self.tup_set.insert(key) {
                    self.tuples.push(elems.clone());
                }
            }
            Type::Func { params, ret } => {
                for p in params {
                    self.visit_type(p);
                }
                self.visit_type(ret);
                let key = ty.mangle();
                if self.fn_set.insert(key) {
                    self.func_ptrs.push((params.clone(), (**ret).clone()));
                }
            }
            _ => {}
        }
    }
}

pub fn c_type_name(ty: &Type) -> String {
    match ty {
        Type::Unit | Type::Never | Type::Null => "lo_unit_t".into(),
        Type::Bool => "bool".into(),
        Type::I8 => "int8_t".into(),
        Type::I16 => "int16_t".into(),
        Type::I32 => "int32_t".into(),
        Type::I64 => "int64_t".into(),
        Type::U8 => "uint8_t".into(),
        Type::U16 => "uint16_t".into(),
        Type::U32 => "uint32_t".into(),
        Type::U64 => "uint64_t".into(),
        Type::Usz => "size_t".into(),
        Type::F32 => "float".into(),
        Type::F64 => "double".into(),
        Type::Char => "uint32_t".into(),
        Type::Str => "lo_str".into(),
        Type::Array(_) => "lo_vec*".into(),
        Type::Map(_, _) | Type::Set(_) => "lo_map*".into(),
        Type::Task(_) => "lo_task_t*".into(),
        Type::Channel(_) => "lo_chan_t*".into(),
        Type::Range(_) => "lo_range_i64_t".into(),
        Type::ErrorAny => "lo_error_t".into(),
        Type::Struct(s) => format!("lo_st_{}", s),
        Type::Enum(e) => format!("lo_en_{}", e),
        Type::Nullable(inner) => format!("lo_opt_{}", inner.mangle()),
        Type::Tuple(_) => format!("lo_{}", ty.mangle()),
        Type::Generator(inner) => format!("lo_gen_{}*", inner.mangle()),
        Type::Func { .. } => format!("lo_{}_t", ty.mangle()),
        Type::Interface(_) | Type::Module(_) => "lo_unit_t".into(),
    }
}

pub fn c_default_zero(ty: &Type) -> String {
    match ty {
        Type::Unit | Type::Never | Type::Null => "lo_unit_val()".into(),
        Type::Bool => "false".into(),
        Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::U8 | Type::U16 | Type::U32
        | Type::U64 | Type::Usz | Type::Char => "0".into(),
        Type::F32 | Type::F64 => "0.0".into(),
        Type::Str => "lo_str_lit(\"\", 0)".into(),
        Type::Array(_)
        | Type::Map(_, _)
        | Type::Set(_)
        | Type::Task(_)
        | Type::Channel(_)
        | Type::Generator(_)
        | Type::Func { .. } => "NULL".into(),
        other => format!("(({}){{0}})", c_type_name(other)),
    }
}

pub fn emit_type_definitions(
    prog: &CheckedProgram,
    reg: &TypeRegistry,
    out: &mut String,
) {
    let _ = writeln!(
        out,
        "typedef struct {{ int64_t start; int64_t end; bool inclusive; }} lo_range_i64_t;\n"
    );

    // Forward declarations for structs and enums
    for sname in &prog.struct_order {
        let _ = writeln!(out, "typedef struct lo_st_{0} lo_st_{0};", sname);
    }
    for ename in prog.enums.keys() {
        let _ = writeln!(out, "typedef struct lo_en_{0} lo_en_{0};", ename);
    }
    for inner in &reg.generators {
        let m = inner.mangle();
        let _ = writeln!(out, "typedef struct lo_gen_{0} lo_gen_{0};", m);
    }
    let _ = writeln!(out);

    // Emit interleaved topological order of structs, enums, tuples, optionals, function pointers, generators
    let mut emitted_structs: HashSet<String> = HashSet::new();
    let mut emitted_enums: HashSet<String> = HashSet::new();
    let mut emitted_opts: HashSet<String> = HashSet::new();
    let mut emitted_tups: HashSet<String> = HashSet::new();
    let mut emitted_fns: HashSet<String> = HashSet::new();

    fn ensure_value_type(
        ty: &Type,
        structs: &HashMap<String, CheckedStruct>,
        enums: &HashMap<String, CheckedEnum>,
        emitted_structs: &mut HashSet<String>,
        emitted_enums: &mut HashSet<String>,
        emitted_opts: &mut HashSet<String>,
        emitted_tups: &mut HashSet<String>,
        emitted_fns: &mut HashSet<String>,
        out: &mut String,
    ) {
        match ty {
            Type::Struct(sname) => {
                if emitted_structs.insert(sname.clone()) {
                    if let Some(st) = structs.get(sname) {
                        for f in &st.fields {
                            ensure_value_type(
                                &f.ty,
                                structs,
                                enums,
                                emitted_structs,
                                emitted_enums,
                                emitted_opts,
                                emitted_tups,
                                emitted_fns,
                                out,
                            );
                        }
                        let _ = writeln!(out, "struct lo_st_{} {{", sname);
                        if st.fields.is_empty() {
                            let _ = writeln!(out, "    uint8_t _empty;");
                        } else {
                            for f in &st.fields {
                                let _ = writeln!(
                                    out,
                                    "    {} f_{};",
                                    c_type_name(&f.ty),
                                    crate::sema::sanitize_ident(&f.name)
                                );
                            }
                        }
                        let _ = writeln!(out, "}};\n");
                    }
                }
            }
            Type::Enum(ename) => {
                if emitted_enums.insert(ename.clone()) {
                    if let Some(en) = enums.get(ename) {
                        for v in &en.variants {
                            match &v.payload {
                                CheckedVariantPayload::Unit => {}
                                CheckedVariantPayload::Tuple(ts) => {
                                    for t in ts {
                                        ensure_value_type(
                                            t,
                                            structs,
                                            enums,
                                            emitted_structs,
                                            emitted_enums,
                                            emitted_opts,
                                            emitted_tups,
                                            emitted_fns,
                                            out,
                                        );
                                    }
                                }
                                CheckedVariantPayload::Struct(fs) => {
                                    for f in fs {
                                        ensure_value_type(
                                            &f.ty,
                                            structs,
                                            enums,
                                            emitted_structs,
                                            emitted_enums,
                                            emitted_opts,
                                            emitted_tups,
                                            emitted_fns,
                                            out,
                                        );
                                    }
                                }
                            }
                        }
                        let _ = writeln!(out, "struct lo_en_{} {{", ename);
                        let _ = writeln!(out, "    uint32_t tag;");
                        let _ = writeln!(out, "    union {{");
                        let _ = writeln!(out, "        uint8_t _unit;");
                        for v in &en.variants {
                            match &v.payload {
                                CheckedVariantPayload::Unit => {}
                                CheckedVariantPayload::Tuple(ts) => {
                                    let _ = writeln!(out, "        struct {{");
                                    for (i, t) in ts.iter().enumerate() {
                                        let _ = writeln!(
                                            out,
                                            "            {} v{};",
                                            c_type_name(t),
                                            i
                                        );
                                    }
                                    let _ = writeln!(out, "        }} {};", v.name);
                                }
                                CheckedVariantPayload::Struct(fs) => {
                                    let _ = writeln!(out, "        struct {{");
                                    if fs.is_empty() {
                                        let _ = writeln!(out, "            uint8_t _empty;");
                                    }
                                    for f in fs {
                                        let _ = writeln!(
                                            out,
                                            "            {} f_{};",
                                            c_type_name(&f.ty),
                                            crate::sema::sanitize_ident(&f.name)
                                        );
                                    }
                                    let _ = writeln!(out, "        }} {};", v.name);
                                }
                            }
                        }
                        let _ = writeln!(out, "    }} u;");
                        let _ = writeln!(out, "}};\n");
                    }
                }
            }
            Type::Nullable(inner) => {
                ensure_value_type(
                    inner,
                    structs,
                    enums,
                    emitted_structs,
                    emitted_enums,
                    emitted_opts,
                    emitted_tups,
                    emitted_fns,
                    out,
                );
                let m = inner.mangle();
                if emitted_opts.insert(m.clone()) {
                    let _ = writeln!(
                        out,
                        "typedef struct {{ bool has_val; {} val; }} lo_opt_{};\n",
                        c_type_name(inner),
                        m
                    );
                }
            }
            Type::Tuple(elems) => {
                for e in elems {
                    ensure_value_type(
                        e,
                        structs,
                        enums,
                        emitted_structs,
                        emitted_enums,
                        emitted_opts,
                        emitted_tups,
                        emitted_fns,
                        out,
                    );
                }
                let m = ty.mangle();
                if emitted_tups.insert(m.clone()) {
                    let _ = writeln!(out, "typedef struct {{");
                    for (i, e) in elems.iter().enumerate() {
                        let _ = writeln!(out, "    {} t{};", c_type_name(e), i);
                    }
                    let _ = writeln!(out, "}} lo_{};\n", m);
                }
            }
            Type::Func { params, ret } => {
                for p in params {
                    ensure_value_type(
                        p,
                        structs,
                        enums,
                        emitted_structs,
                        emitted_enums,
                        emitted_opts,
                        emitted_tups,
                        emitted_fns,
                        out,
                    );
                }
                ensure_value_type(
                    ret,
                    structs,
                    enums,
                    emitted_structs,
                    emitted_enums,
                    emitted_opts,
                    emitted_tups,
                    emitted_fns,
                    out,
                );
                let m = ty.mangle();
                if emitted_fns.insert(m.clone()) {
                    let ps: Vec<String> = if params.is_empty() {
                        vec!["void".into()]
                    } else {
                        params.iter().map(c_type_name).collect()
                    };
                    let _ = writeln!(
                        out,
                        "typedef {} (*lo_{}_t)({});\n",
                        c_type_name(ret),
                        m,
                        ps.join(", ")
                    );
                }
            }
            _ => {}
        }
    }

    for sname in &prog.struct_order {
        ensure_value_type(
            &Type::Struct(sname.clone()),
            &prog.structs,
            &prog.enums,
            &mut emitted_structs,
            &mut emitted_enums,
            &mut emitted_opts,
            &mut emitted_tups,
            &mut emitted_fns,
            out,
        );
    }
    let mut enum_names: Vec<String> = prog.enums.keys().cloned().collect();
    enum_names.sort();
    for ename in enum_names {
        ensure_value_type(
            &Type::Enum(ename),
            &prog.structs,
            &prog.enums,
            &mut emitted_structs,
            &mut emitted_enums,
            &mut emitted_opts,
            &mut emitted_tups,
            &mut emitted_fns,
            out,
        );
    }
    for elems in &reg.tuples {
        ensure_value_type(
            &Type::Tuple(elems.clone()),
            &prog.structs,
            &prog.enums,
            &mut emitted_structs,
            &mut emitted_enums,
            &mut emitted_opts,
            &mut emitted_tups,
            &mut emitted_fns,
            out,
        );
    }
    for inner in &reg.optionals {
        ensure_value_type(
            &Type::Nullable(Box::new(inner.clone())),
            &prog.structs,
            &prog.enums,
            &mut emitted_structs,
            &mut emitted_enums,
            &mut emitted_opts,
            &mut emitted_tups,
            &mut emitted_fns,
            out,
        );
    }
    for (params, ret) in &reg.func_ptrs {
        ensure_value_type(
            &Type::Func {
                params: params.clone(),
                ret: Box::new(ret.clone()),
            },
            &prog.structs,
            &prog.enums,
            &mut emitted_structs,
            &mut emitted_enums,
            &mut emitted_opts,
            &mut emitted_tups,
            &mut emitted_fns,
            out,
        );
    }
    for inner in &reg.generators {
        ensure_value_type(
            inner,
            &prog.structs,
            &prog.enums,
            &mut emitted_structs,
            &mut emitted_enums,
            &mut emitted_opts,
            &mut emitted_tups,
            &mut emitted_fns,
            out,
        );
        let m = inner.mangle();
        let yt = c_type_name(inner);
        let _ = writeln!(out, "struct lo_gen_{} {{", m);
        let _ = writeln!(
            out,
            "    void (*run_fn)(void* env, bool (*yield_cb)({}, void*), void* yield_ctx);",
            yt
        );
        let _ = writeln!(out, "    void* env;");
        let _ = writeln!(out, "}};\n");
    }
}
