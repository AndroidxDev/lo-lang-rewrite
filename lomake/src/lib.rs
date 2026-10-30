use loc::codegen::{compile_c_to_binary, emit_c_program};
use loc::comptime::ComptimeEvaluator;
use loc::parser::{BindTarget, Expr, ExprKind, Parser, Program, Stmt, StmtKind};
use loc::sema::TypeChecker;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub name: String,
    pub version: String,
    pub entry: String,
    pub opt_level: u8,
    pub c_libs: Vec<String>,
    pub test_dir: String,
    pub std_path: Option<String>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            name: "app".into(),
            version: "0.1.0".into(),
            entry: "src/main.lo".into(),
            opt_level: 2,
            c_libs: vec!["m".into(), "pthread".into()],
            test_dir: "tests".into(),
            std_path: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BuildCache {
    pub modules: BTreeMap<String, String>,
    pub targets: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct BuildReport {
    pub binary_path: PathBuf,
    pub cache_hit: bool,
    pub module_count: usize,
    pub merkle_hash: String,
}

#[derive(Debug, Clone)]
pub struct TestSummary {
    pub passed: usize,
    pub failed: usize,
    pub results: Vec<(String, bool, String)>,
}

/// Compute a fast, deterministic 128-bit digest formatted as 32 hex chars.
pub fn content_hash_hex(bytes: &[u8]) -> String {
    let mut h1: u64 = 0xcbf29ce484222325;
    let mut h2: u64 = 0x84222325cbf29ce4;
    for (idx, &b) in bytes.iter().enumerate() {
        h1 ^= b as u64;
        h1 = h1.wrapping_mul(0x100000001b3);
        h2 ^= (b as u64).wrapping_add((idx as u64).wrapping_mul(0x9e3779b97f4a7c15));
        h2 = h2.rotate_left(13).wrapping_mul(0xff51afd7ed558ccd);
    }
    h1 ^= h1 >> 33;
    h2 ^= h2 >> 29;
    format!("{:016x}{:016x}", h1, h2)
}

/// Parse `Lomake.lo` written in genuine `lo` syntax.
pub fn parse_lomake_manifest(src: &str) -> Result<ProjectConfig, String> {
    let mut prog = Parser::parse_source(src)
        .map_err(|e| format!("Lomake.lo syntax error: {}", e))?;
    ComptimeEvaluator::evaluate_program(&mut prog)
        .map_err(|e| format!("Lomake.lo comptime error: {}", e))?;

    for stmt in &prog.items {
        if let StmtKind::Bind {
            target: BindTarget::Single { name, .. },
            value,
            ..
        } = &stmt.kind
        {
            if name == "project" {
                let ExprKind::StructLit { fields, .. } = &value.kind else {
                    return Err(
                        "`project` in Lomake.lo must be a struct record `[ name = ..., ... ]`"
                            .into(),
                    );
                };
                let mut cfg = ProjectConfig::default();
                let mut has_name = false;
                for (fname, fexpr) in fields {
                    match fname.as_str() {
                        "name" => {
                            cfg.name = expect_str(fexpr, "name")?;
                            has_name = true;
                        }
                        "version" => {
                            cfg.version = expect_str(fexpr, "version")?;
                        }
                        "entry" => {
                            cfg.entry = expect_str(fexpr, "entry")?;
                        }
                        "opt_level" => {
                            let lvl = expect_int(fexpr, "opt_level")?;
                            if !(0..=3).contains(&lvl) {
                                return Err(format!(
                                    "`opt_level` must be between 0 and 3, found {}",
                                    lvl
                                ));
                            }
                            cfg.opt_level = lvl as u8;
                        }
                        "c_libs" => {
                            cfg.c_libs = expect_str_list(fexpr, "c_libs")?;
                        }
                        "test_dir" => {
                            cfg.test_dir = expect_str(fexpr, "test_dir")?;
                        }
                        "std_path" => {
                            cfg.std_path = Some(expect_str(fexpr, "std_path")?);
                        }
                        other => {
                            return Err(format!("unknown field `{}` in Lomake.lo `project`", other));
                        }
                    }
                }
                if !has_name {
                    return Err("`project` in Lomake.lo is missing required field `name`".into());
                }
                return Ok(cfg);
            }
        }
    }

    Err("Lomake.lo must define `project = [ name = \"...\", ... ]`".into())
}

fn expect_str(e: &Expr, field: &str) -> Result<String, String> {
    match &e.kind {
        ExprKind::Str(s) => Ok(s.clone()),
        _ => Err(format!("field `{}` in Lomake.lo must be a string literal", field)),
    }
}

fn expect_int(e: &Expr, field: &str) -> Result<i64, String> {
    match &e.kind {
        ExprKind::Int(n) => Ok(*n),
        _ => Err(format!("field `{}` in Lomake.lo must be an integer", field)),
    }
}

fn expect_str_list(e: &Expr, field: &str) -> Result<Vec<String>, String> {
    match &e.kind {
        ExprKind::Unit => Ok(Vec::new()),
        ExprKind::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(expect_str(item, field)?);
            }
            Ok(out)
        }
        _ => Err(format!(
            "field `{}` in Lomake.lo must be an array of strings `[\"m\", ...]`",
            field
        )),
    }
}

pub fn load_project_config(project_dir: &Path) -> Result<(ProjectConfig, String), String> {
    let manifest_path = project_dir.join("Lomake.lo");
    let raw = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("failed to read `{}`: {}", manifest_path.display(), e))?;
    let cfg = parse_lomake_manifest(&raw)?;
    let manifest_hash = content_hash_hex(raw.as_bytes());
    Ok((cfg, manifest_hash))
}

pub fn resolve_std_dir(project_dir: &Path, cfg: &ProjectConfig) -> Option<PathBuf> {
    if let Some(ref sp) = cfg.std_path {
        let p = project_dir.join(sp);
        if p.exists() {
            return Some(p);
        }
    }
    let local_std = project_dir.join("std");
    if local_std.exists() {
        return Some(local_std);
    }
    let parent_std = project_dir.join("../std");
    if parent_std.exists() {
        return Some(parent_std);
    }
    let parent2_std = project_dir.join("../../std");
    if parent2_std.exists() {
        return Some(parent2_std);
    }
    TypeChecker::default_std_dir()
}

/// Walk the transitive import graph (`<"./...">` and `<"std/...">`) starting at `entry_file`,
/// returning a sorted map of canonical module path -> content hash.
pub fn resolve_import_graph(
    entry_file: &Path,
    std_dir: Option<&Path>,
) -> Result<BTreeMap<PathBuf, String>, String> {
    let mut visited: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut active_stack: Vec<PathBuf> = Vec::new();
    visit_module(entry_file, std_dir, &mut visited, &mut active_stack)?;
    Ok(visited)
}

fn visit_module(
    file_path: &Path,
    std_dir: Option<&Path>,
    visited: &mut BTreeMap<PathBuf, String>,
    active_stack: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if !file_path.exists() {
        return Err(format!("module file not found: `{}`", file_path.display()));
    }
    let canon = file_path
        .canonicalize()
        .unwrap_or_else(|_| file_path.to_path_buf());
    if visited.contains_key(&canon) {
        return Ok(());
    }
    if active_stack.contains(&canon) {
        return Err(format!(
            "circular import detected involving `{}`",
            canon.display()
        ));
    }

    let src = fs::read_to_string(&canon)
        .map_err(|e| format!("failed to read `{}`: {}", canon.display(), e))?;
    let hash = content_hash_hex(src.as_bytes());
    visited.insert(canon.clone(), hash);

    let prog = Parser::parse_source(&src)
        .map_err(|e| format!("in `{}`: {}", canon.display(), e))?;

    let mut imports = Vec::new();
    collect_imports_from_program(&prog, &mut imports);

    active_stack.push(canon.clone());
    let base_dir = canon.parent().unwrap_or_else(|| Path::new("."));
    for spec in imports {
        if let Some(dep_path) = resolve_import_spec(&spec, base_dir, std_dir)? {
            visit_module(&dep_path, std_dir, visited, active_stack)?;
        }
    }
    active_stack.pop();
    Ok(())
}

fn resolve_import_spec(
    spec: &str,
    base_dir: &Path,
    std_dir: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    if let Some(rest) = spec.strip_prefix("std/") {
        let rel = if rest.ends_with(".lo") {
            rest.to_string()
        } else {
            format!("{}.lo", rest)
        };
        if let Some(sd) = std_dir {
            let candidate = sd.join(rel);
            if candidate.exists() {
                return Ok(Some(candidate));
            }
        }
        // Builtin fallback if std file not yet on disk
        return Ok(None);
    }

    let candidate = if spec.ends_with(".lo") {
        base_dir.join(spec)
    } else {
        base_dir.join(format!("{}.lo", spec))
    };
    if !candidate.exists() {
        return Err(format!(
            "imported module `{}` not found at `{}`",
            spec,
            candidate.display()
        ));
    }
    Ok(Some(candidate))
}

fn collect_imports_from_program(prog: &Program, out: &mut Vec<String>) {
    for stmt in &prog.items {
        collect_imports_from_stmt(stmt, out);
    }
}

fn collect_imports_from_stmt(stmt: &Stmt, out: &mut Vec<String>) {
    match &stmt.kind {
        StmtKind::Bind { value, .. } | StmtKind::Mutate { value, .. } | StmtKind::Expr(value) => {
            collect_imports_from_expr(value, out);
        }
        StmtKind::Defer { body, .. } => {
            for s in &body.stmts {
                collect_imports_from_stmt(s, out);
            }
        }
        StmtKind::MethodDef { func, .. } => {
            for s in &func.body.stmts {
                collect_imports_from_stmt(s, out);
            }
        }
        _ => {}
    }
}

fn collect_imports_from_expr(expr: &Expr, out: &mut Vec<String>) {
    match &expr.kind {
        ExprKind::Import(spec) => out.push(spec.clone()),
        ExprKind::Func(f) => {
            for s in &f.body.stmts {
                collect_imports_from_stmt(s, out);
            }
        }
        ExprKind::BlockExpr(b) | ExprKind::AsyncSpawn(b) | ExprKind::Comptime(b) => {
            for s in &b.stmts {
                collect_imports_from_stmt(s, out);
            }
        }
        _ => {}
    }
}

fn load_cache(cache_file: &Path) -> BuildCache {
    let Ok(text) = fs::read_to_string(cache_file) else {
        return BuildCache::default();
    };
    let mut cache = BuildCache::default();
    let mut section = "";
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "[modules]" {
            section = "modules";
        } else if trimmed == "[targets]" {
            section = "targets";
        } else if let Some((k, v)) = trimmed.split_once('=') {
            let key = k.trim().to_string();
            let val = v.trim().to_string();
            if section == "modules" {
                cache.modules.insert(key, val);
            } else if section == "targets" {
                cache.targets.insert(key, val);
            }
        }
    }
    cache
}

fn save_cache(cache_file: &Path, cache: &BuildCache) -> Result<(), String> {
    if let Some(parent) = cache_file.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create `{}`: {}", parent.display(), e))?;
    }
    let mut buf = String::from("# lomake incremental build cache\n[modules]\n");
    for (k, v) in &cache.modules {
        buf.push_str(&format!("{} = {}\n", k, v));
    }
    buf.push_str("\n[targets]\n");
    for (k, v) in &cache.targets {
        buf.push_str(&format!("{} = {}\n", k, v));
    }
    fs::write(cache_file, buf)
        .map_err(|e| format!("failed to write `{}`: {}", cache_file.display(), e))
}

fn compute_target_merkle(
    manifest_hash: &str,
    cfg: &ProjectConfig,
    mod_hashes: &BTreeMap<PathBuf, String>,
) -> String {
    let mut combined = format!(
        "manifest={};opt={};libs={};",
        manifest_hash,
        cfg.opt_level,
        cfg.c_libs.join(",")
    );
    for (path, h) in mod_hashes {
        combined.push_str(&format!("{}={};", path.display(), h));
    }
    content_hash_hex(combined.as_bytes())
}

/// `lomake init <name>`: creates `<base_dir>/<name>` with `Lomake.lo`, `src/main.lo`, `tests/basic_test.lo`, `.gitignore`.
pub fn cmd_init(base_dir: &Path, name: &str) -> Result<PathBuf, String> {
    if name.trim().is_empty() || name.contains('/') || name.contains('\\') {
        return Err(format!("invalid project name `{}`", name));
    }
    let proj_dir = base_dir.join(name);
    if proj_dir.exists() {
        return Err(format!(
            "directory `{}` already exists",
            proj_dir.display()
        ));
    }

    fs::create_dir_all(proj_dir.join("src"))
        .map_err(|e| format!("failed to create `src/`: {}", e))?;
    fs::create_dir_all(proj_dir.join("tests"))
        .map_err(|e| format!("failed to create `tests/`: {}", e))?;

    let lomake_content = format!(
        "#[ Lomake.lo — конфигурация сборки проекта {name} ]#\n\
         project = [\n  \
           name = \"{name}\"\n  \
           version = \"0.1.0\"\n  \
           entry = \"src/main.lo\"\n  \
           opt_level = 3\n  \
           c_libs = [\"m\", \"pthread\"]\n\
         ]\n"
    );
    let main_content = format!(
        "#[ Точка входа проекта {name} ]#\n\
         +main = {{\n  \
           println(\"Привет из {name}!\")\n\
         }}\n"
    );
    let test_content = "#[ Базовый тест проекта ]#\n\
        add = (a [i64], b [i64]) [i64] { a + b }\n\n\
        +main = {\n  \
          assert(add(2, 3) == 5)\n  \
          println(\"basic_test ok\")\n\
        }\n";
    let gitignore_content = "build/\n";

    fs::write(proj_dir.join("Lomake.lo"), lomake_content)
        .map_err(|e| format!("failed to write `Lomake.lo`: {}", e))?;
    fs::write(proj_dir.join("src/main.lo"), main_content)
        .map_err(|e| format!("failed to write `src/main.lo`: {}", e))?;
    fs::write(proj_dir.join("tests/basic_test.lo"), test_content)
        .map_err(|e| format!("failed to write `tests/basic_test.lo`: {}", e))?;
    fs::write(proj_dir.join(".gitignore"), gitignore_content)
        .map_err(|e| format!("failed to write `.gitignore`: {}", e))?;

    Ok(proj_dir)
}

/// `lomake build`: resolves import graph, checks incremental module hash cache, compiles & links binary into `build/<name>`.
pub fn cmd_build(project_dir: &Path) -> Result<BuildReport, String> {
    let (cfg, manifest_hash) = load_project_config(project_dir)?;
    let entry_path = project_dir.join(&cfg.entry);
    if !entry_path.exists() {
        return Err(format!(
            "entry file `{}` does not exist",
            entry_path.display()
        ));
    }

    let std_dir = resolve_std_dir(project_dir, &cfg);
    let mod_hashes = resolve_import_graph(&entry_path, std_dir.as_deref())?;
    let merkle_hash = compute_target_merkle(&manifest_hash, &cfg, &mod_hashes);

    let build_dir = project_dir.join("build");
    let cache_file = build_dir.join(".lomake_cache");
    let out_bin = build_dir.join(&cfg.name);

    let mut cache = load_cache(&cache_file);
    let target_key = format!("bin:{}", cfg.name);

    if out_bin.exists() && cache.targets.get(&target_key) == Some(&merkle_hash) {
        return Ok(BuildReport {
            binary_path: out_bin,
            cache_hit: true,
            module_count: mod_hashes.len(),
            merkle_hash,
        });
    }

    fs::create_dir_all(&build_dir)
        .map_err(|e| format!("failed to create `build/`: {}", e))?;

    let checked = TypeChecker::check_file(&entry_path, std_dir)
        .map_err(|e| format!("{}", e))?;
    let c_code = emit_c_program(&checked);

    let c_cache_dir = build_dir.join(".cache");
    let _ = fs::create_dir_all(&c_cache_dir);
    let _ = fs::write(c_cache_dir.join(format!("{}.c", cfg.name)), &c_code);

    compile_c_to_binary(&c_code, &out_bin, cfg.opt_level, &cfg.c_libs)
        .map_err(|e| e.to_string())?;

    for (mpath, mhash) in &mod_hashes {
        cache
            .modules
            .insert(mpath.display().to_string(), mhash.clone());
    }
    cache.targets.insert(target_key, merkle_hash.clone());
    save_cache(&cache_file, &cache)?;

    Ok(BuildReport {
        binary_path: out_bin,
        cache_hit: false,
        module_count: mod_hashes.len(),
        merkle_hash,
    })
}

/// `lomake run`: builds (if needed) and runs `build/<name>`, returning exit status code.
pub fn cmd_run(project_dir: &Path, args: &[String]) -> Result<i32, String> {
    let report = cmd_build(project_dir)?;
    let status = Command::new(&report.binary_path)
        .current_dir(project_dir)
        .args(args)
        .status()
        .map_err(|e| format!("failed to execute `{}`: {}", report.binary_path.display(), e))?;
    Ok(status.code().unwrap_or(1))
}

fn discover_lo_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    let entries =
        fs::read_dir(dir).map_err(|e| format!("failed to read `{}`: {}", dir.display(), e))?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        paths.push(entry.path());
    }
    paths.sort();
    for p in paths {
        if p.is_dir() {
            discover_lo_files(&p, out)?;
        } else if p.extension().and_then(|s| s.to_str()) == Some("lo") {
            out.push(p);
        }
    }
    Ok(())
}

/// `lomake test`: discovers all `.lo` files in `tests/`, compiles (with cache) & executes each.
pub fn cmd_test(project_dir: &Path) -> Result<TestSummary, String> {
    let (cfg, manifest_hash) = load_project_config(project_dir)?;
    let test_root = project_dir.join(&cfg.test_dir);
    let mut test_files = Vec::new();
    discover_lo_files(&test_root, &mut test_files)?;

    let std_dir = resolve_std_dir(project_dir, &cfg);
    let build_dir = project_dir.join("build");
    let test_bin_dir = build_dir.join("tests");
    fs::create_dir_all(&test_bin_dir)
        .map_err(|e| format!("failed to create `build/tests/`: {}", e))?;

    let cache_file = build_dir.join(".lomake_cache");
    let mut cache = load_cache(&cache_file);

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut results = Vec::with_capacity(test_files.len());
    let mut used_names: HashSet<String> = HashSet::new();

    for tf in &test_files {
        let rel_display = tf
            .strip_prefix(project_dir)
            .unwrap_or(tf)
            .display()
            .to_string();

        let base_stem = tf
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("test")
            .to_string();
        let bin_name = if used_names.insert(base_stem.clone()) {
            base_stem
        } else {
            format!("{}_{}", base_stem, used_names.len())
        };
        let test_bin = test_bin_dir.join(&bin_name);
        let target_key = format!("test:{}", rel_display);

        let graph_res = resolve_import_graph(tf, std_dir.as_deref());
        let mod_hashes = match graph_res {
            Ok(m) => m,
            Err(e) => {
                failed += 1;
                results.push((rel_display, false, e));
                continue;
            }
        };

        let merkle = compute_target_merkle(&manifest_hash, &cfg, &mod_hashes);
        if !test_bin.exists() || cache.targets.get(&target_key) != Some(&merkle) {
            match TypeChecker::check_file(tf, std_dir.clone()) {
                Ok(checked) => {
                    let c_code = emit_c_program(&checked);
                    if let Err(e) = compile_c_to_binary(&c_code, &test_bin, 1, &cfg.c_libs) {
                        failed += 1;
                        results.push((rel_display, false, e.to_string()));
                        continue;
                    }
                    for (mp, mh) in &mod_hashes {
                        cache.modules.insert(mp.display().to_string(), mh.clone());
                    }
                    cache.targets.insert(target_key, merkle);
                }
                Err(e) => {
                    failed += 1;
                    results.push((rel_display, false, e.to_string()));
                    continue;
                }
            }
        }

        match Command::new(&test_bin).current_dir(project_dir).output() {
            Ok(out) if out.status.success() => {
                passed += 1;
                results.push((
                    rel_display,
                    true,
                    String::from_utf8_lossy(&out.stdout).trim().to_string(),
                ));
            }
            Ok(out) => {
                failed += 1;
                let msg = format!(
                    "exited with {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                results.push((rel_display, false, msg));
            }
            Err(e) => {
                failed += 1;
                results.push((rel_display, false, format!("execution error: {}", e)));
            }
        }
    }

    save_cache(&cache_file, &cache)?;
    Ok(TestSummary {
        passed,
        failed,
        results,
    })
}

/// `lomake clean`: deletes `build/`.
pub fn cmd_clean(project_dir: &Path) -> Result<bool, String> {
    let build_dir = project_dir.join("build");
    if build_dir.exists() {
        fs::remove_dir_all(&build_dir)
            .map_err(|e| format!("failed to remove `{}`: {}", build_dir.display(), e))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_lomake_manifest_from_plan_md() {
        let src = r#"
        project = [
          name = "my_app"
          version = "0.1.0"
          entry = "src/main.lo"
          opt_level = 3
          c_libs = ["m", "pthread"]
        ]
        "#;
        let cfg = parse_lomake_manifest(src).expect("valid Lomake.lo");
        assert_eq!(cfg.name, "my_app");
        assert_eq!(cfg.version, "0.1.0");
        assert_eq!(cfg.entry, "src/main.lo");
        assert_eq!(cfg.opt_level, 3);
        assert_eq!(cfg.c_libs, vec!["m", "pthread"]);
    }

    #[test]
    fn test_lomake_lifecycle_init_build_cache_test_clean() {
        let tmp_root =
            std::env::temp_dir().join(format!("lomake_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_root);
        fs::create_dir_all(&tmp_root).unwrap();

        // 1. lomake init demo_proj
        let proj_dir = cmd_init(&tmp_root, "demo_proj").expect("init succeeds");
        assert!(proj_dir.join("Lomake.lo").exists());
        assert!(proj_dir.join("src/main.lo").exists());
        assert!(proj_dir.join("tests/basic_test.lo").exists());

        // Add an imported local module `src/helper.lo` to verify transitive import hashing!
        fs::write(
            proj_dir.join("src/helper.lo"),
            "+triple = (x [i64]) [i64] { x * 3 }\n",
        )
        .unwrap();
        fs::write(
            proj_dir.join("src/main.lo"),
            "h = <\"./helper.lo\">\n+main = {\n  println(\"val={h.triple(7)}\")\n}\n",
        )
        .unwrap();

        // 2. First `lomake build` -> compiles 2 modules (cache_hit == false)
        let rep1 = cmd_build(&proj_dir).expect("first build succeeds");
        assert!(!rep1.cache_hit);
        assert_eq!(rep1.module_count, 2);
        assert!(rep1.binary_path.exists());

        // 3. Second `lomake build` with no changes -> instant cache hit!
        let rep2 = cmd_build(&proj_dir).expect("second build succeeds");
        assert!(rep2.cache_hit);
        assert_eq!(rep1.merkle_hash, rep2.merkle_hash);

        // 4. Modify transitive dependency `src/helper.lo` -> cache invalidates and rebuilds!
        fs::write(
            proj_dir.join("src/helper.lo"),
            "+triple = (x [i64]) [i64] { x * 4 }\n",
        )
        .unwrap();
        let rep3 = cmd_build(&proj_dir).expect("rebuild after module edit succeeds");
        assert!(!rep3.cache_hit);
        assert_ne!(rep2.merkle_hash, rep3.merkle_hash);

        // 5. Run binary & check exit code
        let code = cmd_run(&proj_dir, &[]).expect("run succeeds");
        assert_eq!(code, 0);

        // 6. `lomake test`
        let summary = cmd_test(&proj_dir).expect("test succeeds");
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.failed, 0);

        // 7. `lomake clean`
        let cleaned = cmd_clean(&proj_dir).expect("clean succeeds");
        assert!(cleaned);
        assert!(!proj_dir.join("build").exists());

        let _ = fs::remove_dir_all(&tmp_root);
    }
}
