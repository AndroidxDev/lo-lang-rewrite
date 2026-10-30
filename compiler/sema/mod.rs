pub mod checker_expr;
pub mod env;
pub mod types;

pub use env::*;
pub use types::Type;

use crate::comptime::ComptimeEvaluator;
use crate::lexer::Span;
use crate::parser::*;
use checker_expr::FnCheckCtx;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub struct TypeChecker {
    pub(crate) std_dir: Option<PathBuf>,
    pub(crate) loading_stack: Vec<PathBuf>,
    pub(crate) loaded_modules: HashMap<PathBuf, String>,
    pub(crate) modules: HashMap<String, ModuleExports>,
    pub(crate) module_aliases: HashMap<String, String>,
    pub(crate) module_scopes: HashMap<String, ModuleScope>,

    pub(crate) struct_order: Vec<String>,
    pub(crate) structs: HashMap<String, CheckedStruct>,
    pub(crate) generic_structs: HashMap<String, GenericStructTemplate>,
    pub(crate) enums: HashMap<String, CheckedEnum>,
    pub(crate) variant_lookup: HashMap<String, (String, usize)>,
    pub(crate) interfaces: HashMap<String, InterfaceSchema>,
    pub(crate) type_aliases: HashMap<String, Type>,

    pub(crate) externs: Vec<ExternCSig>,
    pub(crate) extern_map: HashMap<String, ExternCSig>,
    pub(crate) globals: Vec<GlobalConst>,
    pub(crate) global_types: HashMap<String, Type>,
    pub(crate) global_c_names: HashMap<String, String>,

    pub(crate) func_templates: HashMap<String, FuncTemplate>,
    pub(crate) method_templates: HashMap<(String, String), String>, // (TypeName, method) -> template_key
    pub(crate) mono_cache: HashMap<String, Type>,                   // c_name -> ret_ty
    pub(crate) active_mono: HashMap<String, Type>,
    pub(crate) mono_funcs: Vec<MonoFunc>,
    pub(crate) next_type_id: u32,
    pub(crate) main_c_name: Option<String>,
}

impl Default for TypeChecker {
    fn default() -> Self {
        Self::new(None)
    }
}

impl TypeChecker {
    pub fn new(std_dir: Option<PathBuf>) -> Self {
        Self {
            std_dir,
            loading_stack: Vec::new(),
            loaded_modules: HashMap::new(),
            modules: HashMap::new(),
            module_aliases: HashMap::new(),
            module_scopes: HashMap::new(),
            struct_order: Vec::new(),
            structs: HashMap::new(),
            generic_structs: HashMap::new(),
            enums: HashMap::new(),
            variant_lookup: HashMap::new(),
            interfaces: HashMap::new(),
            type_aliases: HashMap::new(),
            externs: Vec::new(),
            extern_map: HashMap::new(),
            globals: Vec::new(),
            global_types: HashMap::new(),
            global_c_names: HashMap::new(),
            func_templates: HashMap::new(),
            method_templates: HashMap::new(),
            mono_cache: HashMap::new(),
            active_mono: HashMap::new(),
            mono_funcs: Vec::new(),
            next_type_id: 1,
            main_c_name: None,
        }
    }

    pub fn default_std_dir() -> Option<PathBuf> {
        if let Ok(env_p) = std::env::var("LO_STD_PATH") {
            let p = PathBuf::from(env_p);
            if p.exists() {
                return Some(p);
            }
        }
        let cwd_std = PathBuf::from("std");
        if cwd_std.exists() {
            return Some(cwd_std);
        }
        let manifest_std = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../std");
        if manifest_std.exists() {
            return Some(manifest_std);
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(root) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
                let p = root.join("std");
                if p.exists() {
                    return Some(p);
                }
            }
        }
        None
    }

    pub fn check_source(src: &str) -> Result<CheckedProgram, SemaError> {
        Self::check_source_with_path(src, None, Self::default_std_dir())
    }

    pub fn check_file(path: &Path, std_dir: Option<PathBuf>) -> Result<CheckedProgram, SemaError> {
        let src = fs::read_to_string(path).map_err(|e| SemaError {
            message: format!("failed to read `{}`: {}", path.display(), e),
            span: Span::default(),
        })?;
        let std_resolved = std_dir.or_else(Self::default_std_dir);
        Self::check_source_with_path(&src, Some(path), std_resolved)
    }

    pub fn check_source_with_path(
        src: &str,
        file_path: Option<&Path>,
        std_dir: Option<PathBuf>,
    ) -> Result<CheckedProgram, SemaError> {
        let mut prog = Parser::parse_source(src).map_err(|e| SemaError {
            message: e.message,
            span: e.span,
        })?;
        ComptimeEvaluator::evaluate_program(&mut prog).map_err(|e| SemaError {
            message: e.message,
            span: e.span,
        })?;

        let mut tc = Self::new(std_dir);
        if let Some(fp) = file_path {
            if let Ok(canon) = fp.canonicalize() {
                tc.loading_stack.push(canon);
            }
        }
        tc.process_module(&prog, file_path, "", true)?;
        Ok(tc.finish())
    }

    fn finish(self) -> CheckedProgram {
        CheckedProgram {
            struct_order: self.struct_order,
            structs: self.structs,
            enums: self.enums,
            externs: self.externs,
            globals: self.globals,
            funcs: self.mono_funcs,
            main_c_name: self.main_c_name,
        }
    }

    fn alloc_type_id(&mut self) -> u32 {
        let id = self.next_type_id;
        self.next_type_id += 1;
        id
    }

    fn process_module(
        &mut self,
        prog: &Program,
        base_file: Option<&Path>,
        mod_prefix: &str,
        is_root: bool,
    ) -> Result<ModuleExports, SemaError> {
        let mut exports = ModuleExports {
            id: mod_prefix.to_string(),
            values: HashMap::new(),
            types: HashMap::new(),
        };

        // Pass 1: Register TypeDefs (Structs, Enums, Interfaces, Aliases) and Imports / Externs
        for stmt in &prog.items {
            match &stmt.kind {
                StmtKind::Bind { is_mut: true, .. } => {
                    return Err(SemaError {
                        message: "mutable binding `~=` is only allowed inside functions `{ ... }`"
                            .into(),
                        span: stmt.span,
                    });
                }
                StmtKind::TypeDef {
                    is_pub,
                    name,
                    generics,
                    def,
                } => {
                    self.register_type_def(*is_pub, name, generics, def, stmt.span, &mut exports)?;
                }
                _ => {}
            }
        }

        // Pass 2: Register Functions, Methods, Imports, ExternC, and Top-Level Constants
        let mut root_stmts: Vec<Stmt> = Vec::new();
        for stmt in &prog.items {
            match &stmt.kind {
                StmtKind::TypeDef { .. } => {}
                StmtKind::MethodDef {
                    type_name,
                    method_name,
                    is_mutating,
                    generics,
                    func,
                    ..
                } => {
                    let mut f = func.clone();
                    if f.generics.is_empty() {
                        f.generics = generics.clone();
                    }
                    let key = format!("{}__method_{}_{}", mod_prefix, type_name, method_name);
                    self.func_templates.insert(
                        key.clone(),
                        FuncTemplate {
                            key_name: key.clone(),
                            receiver_type: Some(type_name.clone()),
                            is_mutating_method: *is_mutating,
                            func: f,
                            module_prefix: mod_prefix.to_string(),
                        },
                    );
                    self.method_templates
                        .insert((type_name.clone(), method_name.clone()), key);
                }
                StmtKind::Bind {
                    is_pub,
                    is_mut: false,
                    target: BindTarget::Single { name, ty },
                    value,
                } => match &value.kind {
                    ExprKind::Import(import_spec) => {
                        let mod_id = self.load_import(import_spec, base_file, stmt.span)?;
                        self.module_aliases.insert(name.clone(), mod_id);
                    }
                    ExprKind::ExternC {
                        symbol,
                        params,
                        ret,
                    } => {
                        let empty_sub = HashMap::new();
                        let mut p_tys = Vec::with_capacity(params.len());
                        for p in params {
                            p_tys.push(self.resolve_type_expr(p, &empty_sub, stmt.span)?);
                        }
                        let r_ty = self.resolve_type_expr(ret, &empty_sub, stmt.span)?;
                        let sig = ExternCSig {
                            bind_name: if mod_prefix.is_empty() {
                                name.clone()
                            } else {
                                format!("{}_{}", mod_prefix, name)
                            },
                            c_symbol: symbol.clone(),
                            params: p_tys,
                            ret: r_ty,
                        };
                        self.externs.push(sig.clone());
                        self.extern_map.insert(name.clone(), sig.clone());
                        if *is_pub {
                            exports
                                .values
                                .insert(name.clone(), ModuleSym::ExternC(sig));
                        }
                    }
                    ExprKind::Func(fdef) => {
                        let key = if mod_prefix.is_empty() {
                            name.clone()
                        } else {
                            format!("{}__{}", mod_prefix, name)
                        };
                        self.func_templates.insert(
                            key.clone(),
                            FuncTemplate {
                                key_name: key.clone(),
                                receiver_type: None,
                                is_mutating_method: false,
                                func: (**fdef).clone(),
                                module_prefix: mod_prefix.to_string(),
                            },
                        );
                        if *is_pub {
                            exports
                                .values
                                .insert(name.clone(), ModuleSym::FuncTemplateKey(key));
                        }
                    }
                    _ => {
                        let mut c_ctx = FnCheckCtx::new(None, HashMap::new());
                        let expected_ty = match ty {
                            Some(te) => {
                                Some(self.resolve_type_expr(te, &HashMap::new(), stmt.span)?)
                            }
                            None => None,
                        };
                        let val_ty = self.check_expr(value, expected_ty.as_ref(), &mut c_ctx)?;
                        let final_ty = if let Some(decl) = expected_ty {
                            if !val_ty.is_assignable_to(&decl) {
                                return Err(SemaError {
                                    message: format!(
                                        "cannot assign value of type `{}` to constant `{}` of type `{}`",
                                        val_ty, name, decl
                                    ),
                                    span: stmt.span,
                                });
                            }
                            decl
                        } else if val_ty == Type::Null {
                            return Err(SemaError {
                                message: format!(
                                    "cannot infer type of `{}` from bare `~`; add explicit nullable type tag",
                                    name
                                ),
                                span: stmt.span,
                            });
                        } else {
                            val_ty
                        };
                        let c_name = if mod_prefix.is_empty() {
                            format!("lo_g_{}", sanitize_ident(name))
                        } else {
                            format!("lo_g_{}_{}", mod_prefix, sanitize_ident(name))
                        };
                        self.global_types.insert(name.clone(), final_ty.clone());
                        self.global_c_names.insert(name.clone(), c_name.clone());
                        self.globals.push(GlobalConst {
                            c_name: c_name.clone(),
                            module_prefix: mod_prefix.to_string(),
                            lo_name: name.clone(),
                            ty: final_ty.clone(),
                            value: value.clone(),
                            expr_types: c_ctx.expr_types,
                        });
                        if *is_pub {
                            exports.values.insert(
                                name.clone(),
                                ModuleSym::Const {
                                    c_name,
                                    ty: final_ty,
                                },
                            );
                        }
                    }
                },
                other => {
                    if is_root {
                        root_stmts.push(Stmt {
                            kind: other.clone(),
                            span: stmt.span,
                        });
                    }
                }
            }
        }

        // Pass 3: Eagerly typecheck concrete non-generic functions & entry point `main`
        if is_root {
            if self.func_templates.contains_key("main") {
                let (c_name, _) =
                    self.instantiate_template("main", None, &[], &[], Span::default())?;
                self.main_c_name = Some(c_name);
            } else if !root_stmts.is_empty() {
                // Script-style top-level executable statements -> synthesize `__script_main`
                let synth_func = FuncDef {
                    generics: Vec::new(),
                    params: Vec::new(),
                    ret_ty: Some(TypeExpr::Unit),
                    body: Block {
                        stmts: root_stmts,
                        span: Span::default(),
                    },
                    span: Span::default(),
                };
                self.func_templates.insert(
                    "__script_main".to_string(),
                    FuncTemplate {
                        key_name: "__script_main".to_string(),
                        receiver_type: None,
                        is_mutating_method: false,
                        func: synth_func,
                        module_prefix: String::new(),
                    },
                );
                let (c_name, _) =
                    self.instantiate_template("__script_main", None, &[], &[], Span::default())?;
                self.main_c_name = Some(c_name);
            }

            // Also eagerly check all concrete (annotated) functions/methods even if uncalled so compile_fail tests catch errors inside them!
            let keys: Vec<String> = self.func_templates.keys().cloned().collect();
            for key in keys {
                let tpl = self.func_templates[&key].clone();
                if tpl.func.generics.is_empty() && tpl.func.params.iter().all(|p| p.ty.is_some()) {
                    let empty = HashMap::new();
                    let recv_ok = match &tpl.receiver_type {
                        Some(rn) => !self.generic_structs.contains_key(rn),
                        None => true,
                    };
                    if recv_ok {
                        let mut ptys = Vec::new();
                        for p in &tpl.func.params {
                            ptys.push(self.resolve_type_expr(
                                p.ty.as_ref().unwrap(),
                                &empty,
                                p.span,
                            )?);
                        }
                        let recv_ty = tpl.receiver_type.as_ref().map(|rn| Type::Struct(rn.clone()));
                        let _ =
                            self.instantiate_template(&key, recv_ty, &ptys, &[], tpl.func.span)?;
                    }
                }
            }
        }

        Ok(exports)
    }

    fn load_import(
        &mut self,
        import_spec: &str,
        base_file: Option<&Path>,
        span: Span,
    ) -> Result<String, SemaError> {
        // Resolve path for `<"std/...">` or `<"./...">`
        let resolved_path: Option<PathBuf> = if let Some(rest) = import_spec.strip_prefix("std/") {
            let file_rel = if rest.ends_with(".lo") {
                rest.to_string()
            } else {
                format!("{}.lo", rest)
            };
            self.std_dir
                .as_ref()
                .map(|d| d.join(&file_rel))
                .filter(|p| p.exists())
        } else {
            let base_dir = base_file
                .and_then(|p| p.parent())
                .unwrap_or_else(|| Path::new("."));
            let candidate = if import_spec.ends_with(".lo") {
                base_dir.join(import_spec)
            } else {
                base_dir.join(format!("{}.lo", import_spec))
            };
            Some(candidate)
        };

        if let Some(path) = resolved_path {
            if !path.exists() {
                return Err(SemaError {
                    message: format!("imported module `{}` not found at `{}`", import_spec, path.display()),
                    span,
                });
            }
            let canon = path.canonicalize().unwrap_or(path);
            if let Some(existing_id) = self.loaded_modules.get(&canon) {
                return Ok(existing_id.clone());
            }
            if self.loading_stack.contains(&canon) {
                return Err(SemaError {
                    message: format!("circular module import detected for `{}`", import_spec),
                    span,
                });
            }
            let src = fs::read_to_string(&canon).map_err(|e| SemaError {
                message: format!("failed to read module `{}`: {}", canon.display(), e),
                span,
            })?;
            let mut mod_prog = Parser::parse_source(&src).map_err(|e| SemaError {
                message: format!("in `{}`: {}", import_spec, e.message),
                span: e.span,
            })?;
            ComptimeEvaluator::evaluate_program(&mut mod_prog).map_err(|e| SemaError {
                message: format!("in `{}`: {}", import_spec, e.message),
                span: e.span,
            })?;

            let mod_id = format!("mod{}", self.modules.len() + 1);
            self.loading_stack.push(canon.clone());
            // Save & restore module_aliases so imports are file-scoped
            let saved_aliases = self.module_aliases.clone();
            let saved_globals = self.global_types.clone();
            let saved_externs = self.extern_map.clone();

            let exports = self.process_module(&mod_prog, Some(&canon), &mod_id, false)?;

            self.module_scopes.insert(
                mod_id.clone(),
                ModuleScope {
                    aliases: self.module_aliases.clone(),
                    externs: self.extern_map.clone(),
                    globals: self.global_types.clone(),
                },
            );

            self.module_aliases = saved_aliases;
            self.global_types = saved_globals;
            self.extern_map = saved_externs;
            self.loading_stack.pop();

            self.loaded_modules.insert(canon, mod_id.clone());
            self.modules.insert(mod_id.clone(), exports);
            return Ok(mod_id);
        }

        // Builtin fallback ONLY if `std/<name>.lo` is not on disk (e.g. standalone unit tests)
        if import_spec.starts_with("std/") {
            let mod_id = format!("builtin_{}", sanitize_ident(import_spec));
            if !self.modules.contains_key(&mod_id) {
                self.modules.insert(
                    mod_id.clone(),
                    ModuleExports {
                        id: mod_id.clone(),
                        values: HashMap::new(),
                        types: HashMap::new(),
                    },
                );
            }
            return Ok(mod_id);
        }

        Err(SemaError {
            message: format!("cannot resolve import `{}`", import_spec),
            span,
        })
    }

    fn register_type_def(
        &mut self,
        is_pub: bool,
        name: &str,
        generics: &[GenericParam],
        def: &TypeDefKind,
        span: Span,
        exports: &mut ModuleExports,
    ) -> Result<(), SemaError> {
        match def {
            TypeDefKind::Struct(fields) => {
                if !generics.is_empty() {
                    self.generic_structs.insert(
                        name.to_string(),
                        GenericStructTemplate {
                            name: name.to_string(),
                            generics: generics.to_vec(),
                            fields: fields.clone(),
                        },
                    );
                    return Ok(());
                }
                let mut checked_fields = Vec::with_capacity(fields.len());
                let empty = HashMap::new();
                for f in fields {
                    let mut dummy_ctx = FnCheckCtx::new(None, empty.clone());
                    let decl_ty = match &f.ty {
                        Some(te) => Some(self.resolve_type_expr(te, &empty, f.span)?),
                        None => None,
                    };
                    let def_ty = match &f.default {
                        Some(de) => Some(self.check_expr(de, decl_ty.as_ref(), &mut dummy_ctx)?),
                        None => None,
                    };
                    let final_ty = match (decl_ty, def_ty) {
                        (Some(dt), Some(vt)) => {
                            if !vt.is_assignable_to(&dt) {
                                return Err(SemaError {
                                    message: format!(
                                        "default value of field `{}.{}` has type `{}`, expected `{}`",
                                        name, f.name, vt, dt
                                    ),
                                    span: f.span,
                                });
                            }
                            dt
                        }
                        (Some(dt), None) => dt,
                        (None, Some(vt)) => {
                            if vt == Type::Null {
                                return Err(SemaError {
                                    message: format!(
                                        "field `{}.{}` initialized to `~` requires explicit nullable type `[T?]`",
                                        name, f.name
                                    ),
                                    span: f.span,
                                });
                            }
                            vt
                        }
                        (None, None) => {
                            return Err(SemaError {
                                message: format!(
                                    "field `{}.{}` must specify a `[Type]` or default value",
                                    name, f.name
                                ),
                                span: f.span,
                            })
                        }
                    };
                    checked_fields.push(CheckedField {
                        name: f.name.clone(),
                        ty: final_ty,
                        default: f.default.clone(),
                    });
                }
                let type_id = self.alloc_type_id();
                self.struct_order.push(name.to_string());
                self.structs.insert(
                    name.to_string(),
                    CheckedStruct {
                        name: name.to_string(),
                        fields: checked_fields,
                        type_id,
                    },
                );
                if is_pub {
                    exports.types.insert(name.to_string(), name.to_string());
                }
            }
            TypeDefKind::Enum(variants) => {
                let empty = HashMap::new();
                let mut checked_vars = Vec::with_capacity(variants.len());
                for (idx, v) in variants.iter().enumerate() {
                    let payload = match &v.payload {
                        VariantPayload::Unit => CheckedVariantPayload::Unit,
                        VariantPayload::Tuple(tys) => {
                            let mut rtys = Vec::with_capacity(tys.len());
                            for te in tys {
                                rtys.push(self.resolve_type_expr(te, &empty, v.span)?);
                            }
                            CheckedVariantPayload::Tuple(rtys)
                        }
                        VariantPayload::Struct(fs) => {
                            let mut cfs = Vec::with_capacity(fs.len());
                            for f in fs {
                                let fty = if let Some(te) = &f.ty {
                                    self.resolve_type_expr(te, &empty, f.span)?
                                } else if let Some(de) = &f.default {
                                    let mut dctx = FnCheckCtx::new(None, empty.clone());
                                    self.check_expr(de, None, &mut dctx)?
                                } else {
                                    Type::I64
                                };
                                cfs.push(CheckedField {
                                    name: f.name.clone(),
                                    ty: fty,
                                    default: f.default.clone(),
                                });
                            }
                            CheckedVariantPayload::Struct(cfs)
                        }
                    };
                    self.variant_lookup
                        .insert(v.name.clone(), (name.to_string(), idx));
                    checked_vars.push(CheckedVariant {
                        name: v.name.clone(),
                        tag: idx as u32,
                        payload,
                    });
                }
                let type_id = self.alloc_type_id();
                self.enums.insert(
                    name.to_string(),
                    CheckedEnum {
                        name: name.to_string(),
                        variants: checked_vars,
                        type_id,
                    },
                );
            }
            TypeDefKind::Interface(methods) => {
                let empty = HashMap::new();
                let mut sigs = Vec::with_capacity(methods.len());
                for (mname, te) in methods {
                    let resolved = self.resolve_type_expr(te, &empty, span)?;
                    if let Type::Func { params, ret } = resolved {
                        sigs.push((mname.clone(), params, *ret));
                    }
                }
                self.interfaces.insert(
                    name.to_string(),
                    InterfaceSchema {
                        name: name.to_string(),
                        methods: sigs,
                    },
                );
            }
            TypeDefKind::Alias(te) => {
                let resolved = self.resolve_type_expr(te, &HashMap::new(), span)?;
                self.type_aliases.insert(name.to_string(), resolved);
            }
        }
        Ok(())
    }

    pub(crate) fn resolve_type_expr(
        &mut self,
        te: &TypeExpr,
        subst: &HashMap<String, Type>,
        span: Span,
    ) -> Result<Type, SemaError> {
        match te {
            TypeExpr::Unit => Ok(Type::Unit),
            TypeExpr::Named { name, generics } => {
                if let Some(sub_ty) = subst.get(name) {
                    return Ok(sub_ty.clone());
                }
                if generics.is_empty() {
                    match name.as_str() {
                        "bool" => return Ok(Type::Bool),
                        "i8" => return Ok(Type::I8),
                        "i16" => return Ok(Type::I16),
                        "i32" => return Ok(Type::I32),
                        "i64" => return Ok(Type::I64),
                        "u8" => return Ok(Type::U8),
                        "u16" => return Ok(Type::U16),
                        "u32" => return Ok(Type::U32),
                        "u64" => return Ok(Type::U64),
                        "usz" => return Ok(Type::Usz),
                        "f32" => return Ok(Type::F32),
                        "f64" => return Ok(Type::F64),
                        "char" => return Ok(Type::Char),
                        "str" => return Ok(Type::Str),
                        _ => {}
                    }
                    if let Some(alias) = self.type_aliases.get(name) {
                        return Ok(alias.clone());
                    }
                    if self.structs.contains_key(name) {
                        return Ok(Type::Struct(name.clone()));
                    }
                    if self.enums.contains_key(name) {
                        return Ok(Type::Enum(name.clone()));
                    }
                    if self.interfaces.contains_key(name) {
                        return Ok(Type::Interface(name.clone()));
                    }
                }
                if let Some(gtpl) = self.generic_structs.get(name).cloned() {
                    if generics.len() != gtpl.generics.len() {
                        return Err(SemaError {
                            message: format!(
                                "generic type `{}` expects {} type arguments, found {}",
                                name,
                                gtpl.generics.len(),
                                generics.len()
                            ),
                            span,
                        });
                    }
                    let mut g_args = Vec::with_capacity(generics.len());
                    for g in generics {
                        g_args.push(self.resolve_type_expr(g, subst, span)?);
                    }
                    let mono_name = self.instantiate_generic_struct(&gtpl, &g_args, span)?;
                    return Ok(Type::Struct(mono_name));
                }
                Err(SemaError {
                    message: format!("unknown type `{}`", name),
                    span,
                })
            }
            TypeExpr::Nullable(inner) => {
                let it = self.resolve_type_expr(inner, subst, span)?;
                Ok(Type::make_nullable(it))
            }
            TypeExpr::Array(inner) => {
                let it = self.resolve_type_expr(inner, subst, span)?;
                Ok(Type::Array(Box::new(it)))
            }
            TypeExpr::Channel(inner) => {
                let it = self.resolve_type_expr(inner, subst, span)?;
                Ok(Type::Channel(Box::new(it)))
            }
            TypeExpr::Set(inner) => {
                let it = self.resolve_type_expr(inner, subst, span)?;
                Ok(Type::Set(Box::new(it)))
            }
            TypeExpr::Map(k, v) => {
                let kt = self.resolve_type_expr(k, subst, span)?;
                let vt = self.resolve_type_expr(v, subst, span)?;
                Ok(Type::Map(Box::new(kt), Box::new(vt)))
            }
            TypeExpr::Func { params, ret } => {
                let mut ptys = Vec::with_capacity(params.len());
                for p in params {
                    ptys.push(self.resolve_type_expr(p, subst, span)?);
                }
                let rty = self.resolve_type_expr(ret, subst, span)?;
                Ok(Type::Func {
                    params: ptys,
                    ret: Box::new(rty),
                })
            }
            TypeExpr::Tuple(elems) => {
                let mut etys = Vec::with_capacity(elems.len());
                for e in elems {
                    etys.push(self.resolve_type_expr(e, subst, span)?);
                }
                Ok(Type::Tuple(etys))
            }
        }
    }

    fn instantiate_generic_struct(
        &mut self,
        gtpl: &GenericStructTemplate,
        args: &[Type],
        span: Span,
    ) -> Result<String, SemaError> {
        let mangled_args: Vec<String> = args.iter().map(|a| a.mangle()).collect();
        let mono_name = format!("{}__{}", gtpl.name, mangled_args.join("_"));
        if self.structs.contains_key(&mono_name) {
            return Ok(mono_name);
        }
        let mut subst = HashMap::new();
        for (gp, arg_ty) in gtpl.generics.iter().zip(args.iter()) {
            if let Some(bound_te) = &gp.bound {
                let bound_ty = self.resolve_type_expr(bound_te, &HashMap::new(), span)?;
                self.verify_interface_bound(arg_ty, &bound_ty, span)?;
            }
            subst.insert(gp.name.clone(), arg_ty.clone());
        }
        let mut cfields = Vec::with_capacity(gtpl.fields.len());
        for f in &gtpl.fields {
            let fty = if let Some(te) = &f.ty {
                self.resolve_type_expr(te, &subst, f.span)?
            } else if let Some(de) = &f.default {
                let mut dctx = FnCheckCtx::new(None, subst.clone());
                self.check_expr(de, None, &mut dctx)?
            } else {
                Type::I64
            };
            cfields.push(CheckedField {
                name: f.name.clone(),
                ty: fty,
                default: f.default.clone(),
            });
        }
        let type_id = self.alloc_type_id();
        self.struct_order.push(mono_name.clone());
        self.structs.insert(
            mono_name.clone(),
            CheckedStruct {
                name: mono_name.clone(),
                fields: cfields,
                type_id,
            },
        );
        Ok(mono_name)
    }

    fn verify_interface_bound(
        &mut self,
        concrete: &Type,
        bound: &Type,
        span: Span,
    ) -> Result<(), SemaError> {
        let Type::Interface(iface_name) = bound else {
            return Ok(());
        };
        let Some(iface) = self.interfaces.get(iface_name).cloned() else {
            return Ok(());
        };
        let Type::Struct(type_name) = concrete else {
            return Err(SemaError {
                message: format!(
                    "type `{}` does not implement structural interface `{}`",
                    concrete, iface_name
                ),
                span,
            });
        };
        for (req_method, req_params, req_ret) in &iface.methods {
            let Some(tpl_key) = self
                .method_templates
                .get(&(type_name.clone(), req_method.clone()))
                .cloned()
            else {
                return Err(SemaError {
                    message: format!(
                        "type `{}` is missing method `{}` required by interface `{}`",
                        type_name, req_method, iface_name
                    ),
                    span,
                });
            };
            let (_, actual_ret) = self.instantiate_template(
                &tpl_key,
                Some(Type::Struct(type_name.clone())),
                req_params,
                &[],
                span,
            )?;
            if !actual_ret.is_assignable_to(req_ret) {
                return Err(SemaError {
                    message: format!(
                        "method `{}.{}` returns `{}`, but interface `{}` requires `{}`",
                        type_name, req_method, actual_ret, iface_name, req_ret
                    ),
                    span,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn check_struct_or_variant_lit(
        &mut self,
        type_name: Option<&str>,
        generics: &[TypeExpr],
        spread: Option<&Expr>,
        fields: &[(String, Expr)],
        span: Span,
        ctx: &mut FnCheckCtx,
    ) -> Result<Type, SemaError> {
        // 1. Check struct-payload Enum Variant `Attack[target = 1, dmg = 50]`
        if let Some(tn) = type_name {
            if let Some((enum_name, v_idx)) = self.variant_lookup.get(tn).cloned() {
                let en = self.enums[&enum_name].clone();
                if let CheckedVariantPayload::Struct(vfields) = &en.variants[v_idx].payload {
                    for (fname, fexpr) in fields {
                        let vf = vfields.iter().find(|f| &f.name == fname).ok_or_else(|| {
                            SemaError {
                                message: format!(
                                    "variant `{}.{}` has no field `{}`",
                                    enum_name, tn, fname
                                ),
                                span: fexpr.span,
                            }
                        })?;
                        let vty = self.check_expr(fexpr, Some(&vf.ty), ctx)?;
                        if !vty.is_assignable_to(&vf.ty) {
                            return Err(SemaError {
                                message: format!(
                                    "field `{}` of `{}` expects `{}`, found `{}`",
                                    fname, tn, vf.ty, vty
                                ),
                                span: fexpr.span,
                            });
                        }
                    }
                    ctx.struct_targets
                        .insert(span_key(span), format!("{}::{}", enum_name, tn));
                    return Ok(Type::Enum(enum_name));
                }
            }
        }

        // 2. Named Struct (possibly generic `Cache<str, i64>[...]`)
        let resolved_struct_name = if let Some(tn) = type_name {
            let sty = self.resolve_type_expr(
                &TypeExpr::Named {
                    name: tn.to_string(),
                    generics: generics.to_vec(),
                },
                &ctx.type_subst,
                span,
            )?;
            match sty {
                Type::Struct(sn) => Some(sn),
                other => {
                    return Err(SemaError {
                        message: format!("`{}` is not a struct type", other),
                        span,
                    })
                }
            }
        } else if let Some(sp_expr) = spread {
            let base_ty = self.check_expr(sp_expr, None, ctx)?;
            match base_ty {
                Type::Struct(sn) => Some(sn),
                other => {
                    return Err(SemaError {
                        message: format!("spread `..` requires struct value, found `{}`", other),
                        span: sp_expr.span,
                    })
                }
            }
        } else {
            None
        };

        if let Some(sn) = resolved_struct_name {
            if let Some(sp_expr) = spread {
                let sp_ty = self.check_expr(sp_expr, Some(&Type::Struct(sn.clone())), ctx)?;
                if sp_ty != Type::Struct(sn.clone()) {
                    return Err(SemaError {
                        message: format!(
                            "spread base has type `{}`, expected `[{}]`",
                            sp_ty, sn
                        ),
                        span: sp_expr.span,
                    });
                }
            }
            let schema = self.structs.get(&sn).cloned().ok_or_else(|| SemaError {
                message: format!("unknown struct `{}`", sn),
                span,
            })?;
            let mut provided = HashSet::new();
            for (fname, fexpr) in fields {
                let sf = schema
                    .fields
                    .iter()
                    .find(|f| &f.name == fname)
                    .ok_or_else(|| SemaError {
                        message: format!("struct `{}` has no field `{}`", sn, fname),
                        span: fexpr.span,
                    })?;
                let vty = self.check_expr(fexpr, Some(&sf.ty), ctx)?;
                if !vty.is_assignable_to(&sf.ty) {
                    return Err(SemaError {
                        message: format!(
                            "field `{}.{}` expects `{}`, found `{}`",
                            sn, fname, sf.ty, vty
                        ),
                        span: fexpr.span,
                    });
                }
                provided.insert(fname.as_str());
            }
            if spread.is_none() {
                for sf in &schema.fields {
                    if !provided.contains(sf.name.as_str())
                        && sf.default.is_none()
                        && !sf.ty.is_nullable()
                    {
                        return Err(SemaError {
                            message: format!(
                                "missing required field `{}` in `{}` initialization",
                                sf.name, sn
                            ),
                            span,
                        });
                    }
                }
            }
            ctx.struct_targets.insert(span_key(span), sn.clone());
            return Ok(Type::Struct(sn));
        }

        // 3. Anonymous structural literal `[x = 12.5, y = -4.0]`
        let mut anon_fields = Vec::with_capacity(fields.len());
        let mut sig_parts = Vec::with_capacity(fields.len());
        for (fname, fexpr) in fields {
            let fty = self.check_expr(fexpr, None, ctx)?;
            if fty == Type::Null {
                return Err(SemaError {
                    message: format!("anonymous struct field `{}` cannot be bare `~`", fname),
                    span: fexpr.span,
                });
            }
            sig_parts.push(format!("{}_{}", sanitize_ident(fname), fty.mangle()));
            anon_fields.push(CheckedField {
                name: fname.clone(),
                ty: fty,
                default: None,
            });
        }
        let anon_name = format!("Anon__{}", sig_parts.join("__"));
        if !self.structs.contains_key(&anon_name) {
            let type_id = self.alloc_type_id();
            self.struct_order.push(anon_name.clone());
            self.structs.insert(
                anon_name.clone(),
                CheckedStruct {
                    name: anon_name.clone(),
                    fields: anon_fields,
                    type_id,
                },
            );
        }
        ctx.struct_targets.insert(span_key(span), anon_name.clone());
        Ok(Type::Struct(anon_name))
    }

    pub(crate) fn resolve_field_access(
        &self,
        ty: &Type,
        field: &str,
        span: Span,
    ) -> Result<Type, SemaError> {
        match ty {
            Type::Struct(sn) => {
                let st = self.structs.get(sn).ok_or_else(|| SemaError {
                    message: format!("unknown struct `{}`", sn),
                    span,
                })?;
                if let Some(f) = st.fields.iter().find(|f| f.name == field) {
                    return Ok(f.ty.clone());
                }
                Err(SemaError {
                    message: format!("struct `{}` has no field `{}`", sn, field),
                    span,
                })
            }
            Type::Str | Type::Array(_) | Type::Map(_, _) | Type::Set(_) if field == "len" => {
                Ok(Type::I64)
            }
            Type::ErrorAny => match field {
                "code" => Ok(Type::I64),
                "msg" | "reason" => Ok(Type::Str),
                _ => Err(SemaError {
                    message: format!("error value has no field `{}` (available: `code`, `msg`)", field),
                    span,
                }),
            },
            Type::Module(mod_id) => {
                if mod_id == "builtin_std_math" {
                    return match field {
                        "pi" | "e" | "tau" => Ok(Type::F64),
                        _ => Err(SemaError {
                            message: format!("unknown constant `{}` in `std/math`", field),
                            span,
                        }),
                    };
                }
                if let Some(m) = self.modules.get(mod_id) {
                    if let Some(ModuleSym::Const { ty, .. }) = m.values.get(field) {
                        return Ok(ty.clone());
                    }
                }
                Err(SemaError {
                    message: format!("module `{}` has no exported value `{}`", mod_id, field),
                    span,
                })
            }
            other => Err(SemaError {
                message: format!("cannot access field `.{}` on type `{}`", field, other),
                span,
            }),
        }
    }

    pub(crate) fn check_call(
        &mut self,
        callee: &Expr,
        call_generics: &[TypeExpr],
        args: &[Expr],
        safe: bool,
        span: Span,
        ctx: &mut FnCheckCtx,
    ) -> Result<Type, SemaError> {
        // 1. Channel constructor `[~i64]()`
        if let ExprKind::TypeVal(te) = &callee.kind {
            let resolved = self.resolve_type_expr(te, &ctx.type_subst, callee.span)?;
            if matches!(resolved, Type::Channel(_) | Type::Set(_)) {
                ctx.record_expr_type(callee.span, resolved.clone());
                return Ok(resolved);
            }
        }

        // 2. Tuple Enum Variant constructor `Run(3.5)`
        if let ExprKind::TypeRef { name, .. } = &callee.kind {
            if let Some((enum_name, v_idx)) = self.variant_lookup.get(name).cloned() {
                let en = self.enums[&enum_name].clone();
                if let CheckedVariantPayload::Tuple(expected_tys) = &en.variants[v_idx].payload {
                    if args.len() != expected_tys.len() {
                        return Err(SemaError {
                            message: format!(
                                "variant `{}` expects {} arguments, found {}",
                                name,
                                expected_tys.len(),
                                args.len()
                            ),
                            span,
                        });
                    }
                    for (a, et) in args.iter().zip(expected_tys.iter()) {
                        let aty = self.check_expr(a, Some(et), ctx)?;
                        if !aty.is_assignable_to(et) {
                            return Err(SemaError {
                                message: format!(
                                    "variant `{}` argument expects `{}`, found `{}`",
                                    name, et, aty
                                ),
                                span: a.span,
                            });
                        }
                    }
                    ctx.call_targets
                        .insert(span_key(span), format!("__enum_{}_{}", enum_name, name));
                    return Ok(Type::Enum(enum_name));
                }
            }
        }

        // 3. Built-in `print` / `println` / `assert` when not shadowed
        if let ExprKind::Ident(fname) = &callee.kind {
            if matches!(fname.as_str(), "print" | "println" | "assert")
                && ctx.lookup_var_raw(fname).is_none()
                && !self.func_templates.contains_key(fname)
                && !self.extern_map.contains_key(fname)
            {
                for a in args {
                    let _ = self.check_expr(a, None, ctx)?;
                }
                ctx.call_targets
                    .insert(span_key(span), format!("__builtin_{}", fname));
                return Ok(Type::Unit);
            }
        }

        // 4. Method calls & Module calls (`recv.method(args)` or `recv?.method(args)`)
        if let ExprKind::Field {
            expr: recv_expr,
            field,
            safe: field_safe,
        } = &callee.kind
        {
            // Check qualified enum variant `State.Run(3.5)`
            if let ExprKind::TypeRef { name: tname, .. } = &recv_expr.kind {
                if let Some(en) = self.enums.get(tname).cloned() {
                    if let Some(v) = en.variants.iter().find(|v| &v.name == field) {
                        if let CheckedVariantPayload::Tuple(expected_tys) = &v.payload {
                            for (a, et) in args.iter().zip(expected_tys.iter()) {
                                let aty = self.check_expr(a, Some(et), ctx)?;
                                if !aty.is_assignable_to(et) {
                                    return Err(SemaError {
                                        message: format!("expected `{}`, found `{}`", et, aty),
                                        span: a.span,
                                    });
                                }
                            }
                            ctx.call_targets.insert(
                                span_key(span),
                                format!("__enum_{}_{}", tname, field),
                            );
                            return Ok(Type::Enum(tname.clone()));
                        }
                    }
                }
            }

            let recv_ty = self.check_expr(recv_expr, None, ctx)?;

            // Module call: `math.sin(x)`, `io.println(...)`, etc.
            if let Type::Module(mod_id) = recv_ty {
                return self.check_module_call(&mod_id, field, call_generics, args, span, ctx);
            }

            if recv_ty.is_nullable() && !*field_safe && !safe {
                return Err(SemaError {
                    message: format!(
                        "cannot call `.{}()` on nullable receiver `{}`; use `?.{}()` or `!!`",
                        field, recv_ty, field
                    ),
                    span,
                });
            }

            let is_safe_chain = *field_safe || safe || recv_ty.is_nullable();
            let base_recv = recv_ty.unwrap_nullable().unwrap_or(&recv_ty).clone();

            // Check built-in collection/string/channel methods
            if let Some(ret_ty) =
                self.try_check_builtin_method(&base_recv, recv_expr, field, args, span, ctx)?
            {
                return Ok(if is_safe_chain && ret_ty != Type::Unit {
                    Type::make_nullable(ret_ty)
                } else {
                    ret_ty
                });
            }

            // User-defined Type.method or Type.method!
            if let Type::Struct(ref sn) = base_recv {
                let base_sn = sn.split("__").next().unwrap_or(sn).to_string();
                let lookup_key = if self.method_templates.contains_key(&(sn.clone(), field.clone()))
                {
                    Some((sn.clone(), field.clone()))
                } else if self
                    .method_templates
                    .contains_key(&(base_sn.clone(), field.clone()))
                {
                    Some((base_sn, field.clone()))
                } else {
                    None
                };

                if let Some(mkey) = lookup_key {
                    let tpl_key = self.method_templates[&mkey].clone();
                    let is_mut_method = self.func_templates[&tpl_key].is_mutating_method;
                    if is_mut_method {
                        self.verify_mutable_lvalue(recv_expr, ctx).map_err(|_| {
                            SemaError {
                                message: format!(
                                    "cannot call mutating method `.{}()` on an immutable value; bind receiver with `~=`",
                                    field
                                ),
                                span,
                            }
                        })?;
                    }
                    let mut arg_tys = Vec::with_capacity(args.len());
                    for a in args {
                        arg_tys.push(self.check_expr(a, None, ctx)?);
                    }
                    let mut g_args = Vec::new();
                    for g in call_generics {
                        g_args.push(self.resolve_type_expr(g, &ctx.type_subst, span)?);
                    }
                    let (c_name, ret_ty) = self.instantiate_template(
                        &tpl_key,
                        Some(base_recv),
                        &arg_tys,
                        &g_args,
                        span,
                    )?;
                    ctx.call_targets.insert(span_key(span), c_name);
                    return Ok(if is_safe_chain && ret_ty != Type::Unit {
                        Type::make_nullable(ret_ty)
                    } else {
                        ret_ty
                    });
                }
            }

            // Or calling a function-typed struct field `obj.callback(x)`
            let field_ty = self.resolve_field_access(&base_recv, field, span)?;
            if let Type::Func { params, ret } = field_ty {
                self.verify_call_args(&params, args, span, ctx)?;
                return Ok(if is_safe_chain {
                    Type::make_nullable(*ret)
                } else {
                    *ret
                });
            }

            return Err(SemaError {
                message: format!("type `{}` has no method `{}`", base_recv, field),
                span,
            });
        }

        // 5. Direct function / extern / template / `$` / closure call
        if let ExprKind::Ident(fname) = &callee.kind {
            if ctx.lookup_var_raw(fname).is_none() {
                if !ctx.module_prefix.is_empty() {
                    if let Some(ext) = self
                        .module_scopes
                        .get(&ctx.module_prefix)
                        .and_then(|s| s.externs.get(fname))
                        .cloned()
                    {
                        self.verify_call_args(&ext.params, args, span, ctx)?;
                        ctx.call_targets
                            .insert(span_key(span), format!("__extern_{}", ext.c_symbol));
                        return Ok(ext.ret);
                    }
                    let scoped_tpl = format!("{}__{}", ctx.module_prefix, fname);
                    if self.func_templates.contains_key(&scoped_tpl) {
                        let mut arg_tys = Vec::with_capacity(args.len());
                        for a in args {
                            arg_tys.push(self.check_expr(a, None, ctx)?);
                        }
                        let mut g_args = Vec::new();
                        for g in call_generics {
                            g_args.push(self.resolve_type_expr(g, &ctx.type_subst, span)?);
                        }
                        let (c_name, ret_ty) =
                            self.instantiate_template(&scoped_tpl, None, &arg_tys, &g_args, span)?;
                        ctx.call_targets.insert(span_key(span), c_name);
                        return Ok(ret_ty);
                    }
                }
                if let Some(ext) = self.extern_map.get(fname).cloned() {
                    self.verify_call_args(&ext.params, args, span, ctx)?;
                    ctx.call_targets
                        .insert(span_key(span), format!("__extern_{}", ext.c_symbol));
                    return Ok(ext.ret);
                }
                if self.func_templates.contains_key(fname) {
                    let mut arg_tys = Vec::with_capacity(args.len());
                    for a in args {
                        arg_tys.push(self.check_expr(a, None, ctx)?);
                    }
                    let mut g_args = Vec::new();
                    for g in call_generics {
                        g_args.push(self.resolve_type_expr(g, &ctx.type_subst, span)?);
                    }
                    let (c_name, ret_ty) =
                        self.instantiate_template(fname, None, &arg_tys, &g_args, span)?;
                    ctx.call_targets.insert(span_key(span), c_name);
                    return Ok(ret_ty);
                }
            }
        }

        let callee_ty = self.check_expr(callee, None, ctx)?;
        if callee_ty.is_nullable() && !safe {
            return Err(SemaError {
                message: format!(
                    "cannot invoke nullable function `{}` directly; use safe call `?()` or `!!`",
                    callee_ty
                ),
                span,
            });
        }
        let unwrapped_fn = callee_ty.unwrap_nullable().unwrap_or(&callee_ty).clone();
        match unwrapped_fn {
            Type::Func { params, ret } => {
                self.verify_call_args(&params, args, span, ctx)?;
                if safe || callee_ty.is_nullable() {
                    Ok(Type::make_nullable(*ret))
                } else {
                    Ok(*ret)
                }
            }
            other => Err(SemaError {
                message: format!("type `{}` is not callable", other),
                span,
            }),
        }
    }

    fn check_module_call(
        &mut self,
        mod_id: &str,
        func_name: &str,
        call_generics: &[TypeExpr],
        args: &[Expr],
        span: Span,
        ctx: &mut FnCheckCtx,
    ) -> Result<Type, SemaError> {
        let mut arg_tys = Vec::with_capacity(args.len());
        for a in args {
            arg_tys.push(self.check_expr(a, None, ctx)?);
        }

        // Builtin fallback if std/*.lo wasn't loaded from disk
        if mod_id == "builtin_std_math" {
            match func_name {
                "sin" | "cos" | "tan" | "sqrt" | "floor" | "ceil" | "round" | "abs" => {
                    ctx.call_targets
                        .insert(span_key(span), format!("__math_{}", func_name));
                    return Ok(Type::F64);
                }
                "min" | "max" | "pow" | "clamp" => {
                    ctx.call_targets
                        .insert(span_key(span), format!("__math_{}", func_name));
                    return Ok(Type::F64);
                }
                _ => {}
            }
        }

        let sym = self
            .modules
            .get(mod_id)
            .and_then(|m| m.values.get(func_name))
            .cloned()
            .ok_or_else(|| SemaError {
                message: format!("module has no exported function `{}`", func_name),
                span,
            })?;

        match sym {
            ModuleSym::ExternC(ext) => {
                self.verify_call_args(&ext.params, args, span, ctx)?;
                ctx.call_targets
                    .insert(span_key(span), format!("__extern_{}", ext.c_symbol));
                Ok(ext.ret)
            }
            ModuleSym::FuncTemplateKey(tpl_key) => {
                let mut g_args = Vec::new();
                for g in call_generics {
                    g_args.push(self.resolve_type_expr(g, &ctx.type_subst, span)?);
                }
                let (c_name, ret_ty) =
                    self.instantiate_template(&tpl_key, None, &arg_tys, &g_args, span)?;
                ctx.call_targets.insert(span_key(span), c_name);
                Ok(ret_ty)
            }
            ModuleSym::Const { ty, .. } => {
                if let Type::Func { params, ret } = ty {
                    self.verify_call_args(&params, args, span, ctx)?;
                    Ok(*ret)
                } else {
                    Err(SemaError {
                        message: format!("exported symbol `{}` is not callable", func_name),
                        span,
                    })
                }
            }
        }
    }

    fn try_check_builtin_method(
        &mut self,
        recv_ty: &Type,
        recv_expr: &Expr,
        method: &str,
        args: &[Expr],
        span: Span,
        ctx: &mut FnCheckCtx,
    ) -> Result<Option<Type>, SemaError> {
        let mut arg_tys = Vec::with_capacity(args.len());
        for a in args {
            arg_tys.push(self.check_expr(a, None, ctx)?);
        }

        match recv_ty {
            Type::Str => {
                let ret = match method {
                    "trim" | "upper" | "lower" => Some(Type::Str),
                    "contains" | "starts_with" | "ends_with" => Some(Type::Bool),
                    "split" => Some(Type::Array(Box::new(Type::Str))),
                    "to_i64" => Some(Type::make_nullable(Type::I64)),
                    "to_f64" => Some(Type::make_nullable(Type::F64)),
                    "slice" => Some(Type::Str),
                    _ => None,
                };
                if let Some(r) = ret {
                    ctx.call_targets
                        .insert(span_key(span), format!("__str_{}", method));
                    return Ok(Some(r));
                }
            }
            Type::Array(elem) => {
                let ret = match method {
                    "push!" => {
                        self.verify_mutable_lvalue(recv_expr, ctx)?;
                        if let Some(aty) = arg_tys.first() {
                            if !aty.is_assignable_to(elem) {
                                return Err(SemaError {
                                    message: format!("cannot push `{}` into `[{}..]`", aty, elem),
                                    span,
                                });
                            }
                        }
                        Some(Type::Unit)
                    }
                    "pop!" => {
                        self.verify_mutable_lvalue(recv_expr, ctx)?;
                        Some(Type::make_nullable((**elem).clone()))
                    }
                    "is_empty" | "has" => Some(Type::Bool),
                    _ => None,
                };
                if let Some(r) = ret {
                    ctx.call_targets.insert(
                        span_key(span),
                        format!("__arr_{}", sanitize_ident(method)),
                    );
                    return Ok(Some(r));
                }
            }
            Type::Set(elem) => {
                let ret = match method {
                    "has" => Some(Type::Bool),
                    "add!" | "remove!" => {
                        self.verify_mutable_lvalue(recv_expr, ctx)?;
                        if let Some(aty) = arg_tys.first() {
                            if !aty.is_assignable_to(elem) {
                                return Err(SemaError {
                                    message: format!(
                                        "set method `{}` expects `{}`, found `{}`",
                                        method, elem, aty
                                    ),
                                    span,
                                });
                            }
                        }
                        Some(Type::Unit)
                    }
                    _ => None,
                };
                if let Some(r) = ret {
                    ctx.call_targets.insert(
                        span_key(span),
                        format!("__set_{}", sanitize_ident(method)),
                    );
                    return Ok(Some(r));
                }
            }
            Type::Map(k, _) => {
                let ret = match method {
                    "has" => {
                        if let Some(aty) = arg_tys.first() {
                            if !aty.is_assignable_to(k) {
                                return Err(SemaError {
                                    message: format!("map key expects `{}`, found `{}`", k, aty),
                                    span,
                                });
                            }
                        }
                        Some(Type::Bool)
                    }
                    "remove!" => {
                        self.verify_mutable_lvalue(recv_expr, ctx)?;
                        Some(Type::Unit)
                    }
                    _ => None,
                };
                if let Some(r) = ret {
                    ctx.call_targets.insert(
                        span_key(span),
                        format!("__map_{}", sanitize_ident(method)),
                    );
                    return Ok(Some(r));
                }
            }
            Type::Channel(_) if method == "close" => {
                ctx.call_targets
                    .insert(span_key(span), "__chan_close".to_string());
                return Ok(Some(Type::Unit));
            }
            _ => {}
        }
        Ok(None)
    }

    fn verify_call_args(
        &mut self,
        expected_params: &[Type],
        args: &[Expr],
        span: Span,
        ctx: &mut FnCheckCtx,
    ) -> Result<(), SemaError> {
        if args.len() != expected_params.len() {
            return Err(SemaError {
                message: format!(
                    "expected {} arguments, found {}",
                    expected_params.len(),
                    args.len()
                ),
                span,
            });
        }
        for (a, exp) in args.iter().zip(expected_params.iter()) {
            let aty = self.check_expr(a, Some(exp), ctx)?;
            if !aty.is_assignable_to(exp) {
                return Err(SemaError {
                    message: format!("argument type mismatch: expected `{}`, found `{}`", exp, aty),
                    span: a.span,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn instantiate_template(
        &mut self,
        key: &str,
        receiver_ty: Option<Type>,
        arg_tys: &[Type],
        explicit_generics: &[Type],
        call_span: Span,
    ) -> Result<(String, Type), SemaError> {
        let tpl = self.func_templates.get(key).cloned().ok_or_else(|| SemaError {
            message: format!("unknown function `{}`", key),
            span: call_span,
        })?;

        // Infer or bind `<T [Bound]>` generic parameters
        let mut subst = HashMap::new();
        if !tpl.func.generics.is_empty() {
            if !explicit_generics.is_empty() {
                if explicit_generics.len() != tpl.func.generics.len() {
                    return Err(SemaError {
                        message: format!(
                            "expected {} generic type arguments, found {}",
                            tpl.func.generics.len(),
                            explicit_generics.len()
                        ),
                        span: call_span,
                    });
                }
                for (gp, gt) in tpl.func.generics.iter().zip(explicit_generics.iter()) {
                    subst.insert(gp.name.clone(), gt.clone());
                }
            } else {
                // Infer generic parameters by matching `Param.ty` against `arg_tys`
                for (p, aty) in tpl.func.params.iter().zip(arg_tys.iter()) {
                    if let Some(pte) = &p.ty {
                        Self::unify_generic_param(pte, aty, &mut subst);
                    }
                }
            }
            for gp in &tpl.func.generics {
                let concrete = subst.get(&gp.name).cloned().ok_or_else(|| SemaError {
                    message: format!("could not infer generic parameter `{}`", gp.name),
                    span: call_span,
                })?;
                if let Some(bound_te) = &gp.bound {
                    let bound_ty = self.resolve_type_expr(bound_te, &subst, call_span)?;
                    self.verify_interface_bound(&concrete, &bound_ty, call_span)?;
                }
            }
        }

        // Resolve final parameter types (supporting default parameter values!)
        if arg_tys.len() > tpl.func.params.len() {
            return Err(SemaError {
                message: format!(
                    "function `{}` expects at most {} arguments, found {}",
                    key,
                    tpl.func.params.len(),
                    arg_tys.len()
                ),
                span: call_span,
            });
        }
        let mut final_params: Vec<(String, Type, Option<Expr>)> =
            Vec::with_capacity(tpl.func.params.len());
        for (idx, p) in tpl.func.params.iter().enumerate() {
            let declared_pty = match &p.ty {
                Some(te) => Some(self.resolve_type_expr(te, &subst, p.span)?),
                None => None,
            };
            let actual_pty = if let Some(passed) = arg_tys.get(idx) {
                if let Some(ref decl) = declared_pty {
                    if !passed.is_assignable_to(decl) {
                        return Err(SemaError {
                            message: format!(
                                "argument `{}` of `{}` expects `{}`, found `{}`",
                                p.name, key, decl, passed
                            ),
                            span: call_span,
                        });
                    }
                    decl.clone()
                } else {
                    passed.clone()
                }
            } else if let Some(def_expr) = &p.default {
                let mut dctx = FnCheckCtx::new(None, subst.clone());
                let dty = self.check_expr(def_expr, declared_pty.as_ref(), &mut dctx)?;
                declared_pty.unwrap_or(dty)
            } else {
                return Err(SemaError {
                    message: format!("missing required argument `{}` in call to `{}`", p.name, key),
                    span: call_span,
                });
            };
            final_params.push((p.name.clone(), actual_pty, p.default.clone()));
        }

        let mut mangle_parts = vec![sanitize_ident(&tpl.key_name)];
        if let Some(ref rt) = receiver_ty {
            mangle_parts.push(rt.mangle());
        }
        for (_, pt, _) in &final_params {
            mangle_parts.push(pt.mangle());
        }
        let c_name = format!("lo_fn_{}", mangle_parts.join("__"));

        if let Some(cached_ret) = self.mono_cache.get(&c_name) {
            return Ok((c_name, cached_ret.clone()));
        }
        if let Some(active_ret) = self.active_mono.get(&c_name) {
            return Ok((c_name, active_ret.clone()));
        }

        let declared_ret = match &tpl.func.ret_ty {
            Some(rte) => Some(self.resolve_type_expr(rte, &subst, tpl.func.span)?),
            None => None,
        };

        // Pre-seed `active_mono` for recursive functions (`fact(n - 1)` or `$(n - 1)`)
        let provisional_ret = declared_ret
            .clone()
            .or_else(|| final_params.first().map(|(_, t, _)| t.clone()))
            .unwrap_or(Type::I64);
        self.active_mono
            .insert(c_name.clone(), provisional_ret.clone());

        let mut fctx = FnCheckCtx::new(declared_ret.clone(), subst);
        fctx.module_prefix = tpl.module_prefix.clone();
        let param_only_tys: Vec<Type> = final_params.iter().map(|(_, t, _)| t.clone()).collect();
        let self_fn_sig = Type::Func {
            params: param_only_tys,
            ret: Box::new(provisional_ret),
        };
        fctx.self_fn_ty = Some(self_fn_sig);
        fctx.call_targets
            .insert((usize::MAX, usize::MAX), c_name.clone());

        if let Some(ref rty) = receiver_ty {
            fctx.push_current_item(rty.clone(), tpl.is_mutating_method);
        }
        for (pname, pty, _) in &final_params {
            fctx.define_var(pname, pty.clone(), false, tpl.func.span)?;
        }

        let body_ty = self.check_block(&tpl.func.body, &mut fctx)?;
        self.active_mono.remove(&c_name);

        let final_ret = if fctx.is_generator {
            Type::Generator(Box::new(fctx.yield_ty.clone().unwrap_or(Type::I64)))
        } else if let Some(decl) = declared_ret {
            if body_ty != Type::Never && !body_ty.is_assignable_to(&decl) {
                return Err(SemaError {
                    message: format!(
                        "function `{}` body evaluates to `{}`, expected return type `{}`",
                        key, body_ty, decl
                    ),
                    span: tpl.func.body.span,
                });
            }
            decl
        } else if let Some(early_ret) = fctx.inferred_ret {
            if body_ty == Type::Never {
                early_ret
            } else {
                Type::unify(&early_ret, &body_ty).unwrap_or(body_ty)
            }
        } else if body_ty == Type::Never {
            Type::Unit
        } else {
            body_ty
        };

        self.mono_cache.insert(c_name.clone(), final_ret.clone());
        self.mono_funcs.push(MonoFunc {
            c_name: c_name.clone(),
            module_prefix: tpl.module_prefix.clone(),
            receiver: receiver_ty.map(|rt| (rt, tpl.is_mutating_method)),
            params: final_params,
            ret_ty: final_ret.clone(),
            is_generator: fctx.is_generator,
            yield_ty: fctx.yield_ty,
            body: tpl.func.body,
            expr_types: fctx.expr_types,
            call_targets: fctx.call_targets,
            struct_targets: fctx.struct_targets,
        });

        Ok((c_name, final_ret))
    }

    fn unify_generic_param(te: &TypeExpr, actual: &Type, subst: &mut HashMap<String, Type>) {
        match (te, actual) {
            (TypeExpr::Named { name, generics }, _)
                if generics.is_empty() && name.chars().next().unwrap().is_ascii_uppercase() =>
            {
                subst.entry(name.clone()).or_insert_with(|| actual.clone());
            }
            (TypeExpr::Array(inner_te), Type::Array(inner_act)) => {
                Self::unify_generic_param(inner_te, inner_act, subst);
            }
            (TypeExpr::Nullable(inner_te), Type::Nullable(inner_act)) => {
                Self::unify_generic_param(inner_te, inner_act, subst);
            }
            (TypeExpr::Set(inner_te), Type::Set(inner_act)) => {
                Self::unify_generic_param(inner_te, inner_act, subst);
            }
            (TypeExpr::Map(kte, vte), Type::Map(kact, vact)) => {
                Self::unify_generic_param(kte, kact, subst);
                Self::unify_generic_param(vte, vact, subst);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_null_safety_rejects_null_in_non_null_and_unwrapped_access() {
        // 1. Assigning `~` to `[str]` must fail
        let err1 = TypeChecker::check_source(
            r#"
            +main = {
              name [str] = ~
            }
            "#,
        )
        .unwrap_err();
        assert!(err1.message.contains("cannot assign"));

        // 2. Accessing `.len` on `[str?]` without `?.` or smart cast must fail
        let err2 = TypeChecker::check_source(
            r#"
            +main = {
              nick [str?] = "Alex"
              n = nick.len
            }
            "#,
        )
        .unwrap_err();
        assert!(err2.message.contains("nullable"));
    }

    #[test]
    fn test_kotlin_smart_casts_and_elvis_and_safe_calls() {
        let src = r#"
        process = (x [i64?], nick [str?]) [i64] {
          ? (nick != ~ & nick.len > 2) {
            print(nick.upper())
          }
          ? (x == ~) { ^ 0 }
          valid = x + 10
          fallback = nick?.trim()?.len ?? 5
          valid + fallback
        }
        +main = {
          a = process(32, "Alex")
          b = process(~, ~)
        }
        "#;
        let checked = TypeChecker::check_source(src).expect("smart casts and elvis should pass");
        assert!(checked.main_c_name.is_some());
    }

    #[test]
    fn test_immutability_and_parallel_race_protection() {
        // 1. `~=` at top-level forbidden
        assert!(TypeChecker::check_source("score ~= 0").is_err());

        // 2. Mutating `=` binding forbidden
        assert!(TypeChecker::check_source(
            r#"
            +main = {
              x = 10
              x <- 20
            }
            "#
        )
        .is_err());

        // 3. Mutating outer `~=` binding inside `~>>` parallel pipeline forbidden
        let race_err = TypeChecker::check_source(
            r#"
            +main = {
              sum ~= 0
              [1, 2, 3] ~>> {
                sum +<- @
                @ * 2
              }
            }
            "#,
        )
        .unwrap_err();
        assert!(race_err.message.contains("parallel"));
    }

    #[test]
    fn test_polymorphic_inference_interfaces_and_generators() {
        let src = r#"
        Serializable = [
          to_bytes = () [u8..]
        ]
        Packet = [id = 1]
        Packet.to_bytes = () [u8..] { [1 [u8] .. 4] }

        save_all = <T [Serializable]> (items [T..]) {
          items ~> { @.to_bytes() }
        }

        add = (a, b) { a + b }

        fibonacci = (limit) {
          [a, b] ~= [0, 1]
          @ (a < limit) {
            ^~ a
            [a, b] <- [b, a + b]
          }
        }

        +main = {
          r_int = add(10, 20)
          r_flt = add(1.5, 2.5)
          pkts = [Packet[id = 1], Packet[id = 2]]
          bytes = save_all(pkts)
          fibonacci(100) ~> { print(@) }
        }
        "#;
        let checked = TypeChecker::check_source(src).expect("generics, interfaces, generators should typecheck");
        assert!(checked.funcs.iter().any(|f| f.is_generator));
    }
}
