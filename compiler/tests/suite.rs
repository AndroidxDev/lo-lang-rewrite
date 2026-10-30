use loc::codegen::{compile_c_to_binary, emit_c_program};
use loc::lexer::{Lexer, TokenKind};
use loc::parser::{format_program, Parser};
use loc::sema::TypeChecker;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn collect_lo_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("failed to read `{}`: {}", dir.display(), e))
        .filter_map(|entry| {
            let p = entry.ok()?.path();
            if p.extension().and_then(|s| s.to_str()) == Some("lo") {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "expected at least one `.lo` test file in `{}`",
        dir.display()
    );
    files
}

fn format_tokens_snapshot(src: &str) -> String {
    let tokens = Lexer::tokenize(src).expect("lexer should succeed on valid fixture");
    let mut out = String::new();
    for tok in tokens {
        if matches!(tok.kind, TokenKind::Newline | TokenKind::Eof) {
            continue;
        }
        out.push_str(&format!("{:?}\n", tok.kind));
    }
    out
}

#[test]
fn lexer_tests_suite() {
    let dir = repo_root().join("tests/lexer_tests");
    let update = env::var("UPDATE_GOLDEN").as_deref() == Ok("1");

    for lo_file in collect_lo_files(&dir) {
        let src = fs::read_to_string(&lo_file).unwrap();
        let actual = format_tokens_snapshot(&src);
        let snap_file = lo_file.with_extension("tokens");

        if update {
            fs::write(&snap_file, &actual).unwrap();
        } else {
            let expected = fs::read_to_string(&snap_file).unwrap_or_else(|_| {
                panic!(
                    "missing golden token snapshot `{}` (run with UPDATE_GOLDEN=1 to generate)",
                    snap_file.display()
                )
            });
            assert_eq!(
                actual,
                expected,
                "lexer golden mismatch for `{}`",
                lo_file.display()
            );
        }
    }
}

#[test]
fn parser_golden_suite() {
    let dir = repo_root().join("tests/parser_golden");
    let update = env::var("UPDATE_GOLDEN").as_deref() == Ok("1");

    for lo_file in collect_lo_files(&dir) {
        let src = fs::read_to_string(&lo_file).unwrap();
        let prog = Parser::parse_source(&src)
            .unwrap_or_else(|e| panic!("parser failed on `{}`: {}", lo_file.display(), e));
        let actual = format_program(&prog);
        let ast_file = lo_file.with_extension("ast");

        if update {
            fs::write(&ast_file, &actual).unwrap();
        } else {
            let expected = fs::read_to_string(&ast_file).unwrap_or_else(|_| {
                panic!(
                    "missing golden AST snapshot `{}` (run with UPDATE_GOLDEN=1 to generate)",
                    ast_file.display()
                )
            });
            assert_eq!(
                actual,
                expected,
                "AST golden mismatch for `{}`",
                lo_file.display()
            );
        }
    }
}

#[test]
fn compile_fail_suite() {
    let root = repo_root();
    let dir = root.join("tests/compile_fail");
    let std_dir = root.join("std");

    for lo_file in collect_lo_files(&dir) {
        let src = fs::read_to_string(&lo_file).unwrap();
        let first_line = src.lines().next().unwrap_or("");
        let expected_substr = first_line
            .strip_prefix("# ERROR:")
            .map(str::trim)
            .unwrap_or_else(|| {
                panic!(
                    "`{}` must start with `# ERROR: <expected diagnostic substring>`",
                    lo_file.display()
                )
            });

        match TypeChecker::check_file(&lo_file, Some(std_dir.clone())) {
            Ok(_) => panic!(
                "compile_fail test `{}` unexpectedly compiled without errors! Expected error containing `{}`",
                lo_file.display(),
                expected_substr
            ),
            Err(err) => {
                let diag = err.to_string();
                assert!(
                    diag.contains(expected_substr),
                    "compile_fail diagnostic mismatch in `{}`:\n  expected substring: `{}`\n  actual diagnostic:  `{}`",
                    lo_file.display(),
                    expected_substr,
                    diag
                );
            }
        }
    }
}

#[test]
fn e2e_native_execution_suite() {
    let root = repo_root();
    let dir = root.join("tests/e2e");
    let std_dir = root.join("std");
    let out_dir = env::temp_dir().join(format!("lo_e2e_suite_{}", std::process::id()));
    fs::create_dir_all(&out_dir).unwrap();

    for lo_file in collect_lo_files(&dir) {
        let stem = lo_file.file_stem().unwrap().to_string_lossy();
        let stdout_file = lo_file.with_extension("stdout");
        let expected_stdout = fs::read_to_string(&stdout_file).unwrap_or_else(|_| {
            panic!("missing expected `.stdout` file `{}`", stdout_file.display())
        });

        let checked = TypeChecker::check_file(&lo_file, Some(std_dir.clone()))
            .unwrap_or_else(|e| panic!("typecheck failed on `{}`: {}", lo_file.display(), e));
        let c_code = emit_c_program(&checked);

        let bin_path = out_dir.join(format!("e2e_{}", stem));
        compile_c_to_binary(&c_code, &bin_path, 1, &["m".into(), "pthread".into()])
            .unwrap_or_else(|e| panic!("native C11 build failed on `{}`: {}", lo_file.display(), e));

        let output = Command::new(&bin_path)
            .current_dir(&out_dir)
            .output()
            .unwrap_or_else(|e| panic!("failed to execute `{}`: {}", bin_path.display(), e));

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "e2e binary `{}` exited with {:?}.\nstderr:\n{}",
            lo_file.display(),
            output.status.code(),
            stderr
        );

        let actual_stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
        let expected_norm = expected_stdout.replace("\r\n", "\n");
        assert_eq!(
            actual_stdout,
            expected_norm,
            "stdout mismatch in e2e test `{}`",
            lo_file.display()
        );
    }

    let _ = fs::remove_dir_all(&out_dir);
}
