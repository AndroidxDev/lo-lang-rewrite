use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use loc::codegen::{compile_c_to_binary, emit_c_program};
use loc::comptime::ComptimeEvaluator;
use loc::lexer::Lexer;
use loc::parser::{format_program, Parser};
use loc::sema::TypeChecker;

fn print_usage() {
    eprintln!(
        "loc v1.0.0 — The lo language native compiler\n\n\
         Usage:\n  \
           loc build <file.lo> [-o <out>] [-O0..-O3] [--std-path <dir>] [-l <lib>]\n  \
           loc run   <file.lo> [-O0..-O3] [--std-path <dir>] [-l <lib>] [-- <args...>]\n  \
           loc check <file.lo> [--std-path <dir>]\n  \
           loc emit-c <file.lo> [--std-path <dir>]\n  \
           loc lex   <file.lo>\n  \
           loc ast   <file.lo>"
    );
}

struct CliOpts {
    input: PathBuf,
    output: Option<PathBuf>,
    opt_level: u8,
    std_path: Option<PathBuf>,
    c_libs: Vec<String>,
    run_args: Vec<String>,
}

fn parse_opts(args: &[String]) -> Result<CliOpts, String> {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut opt_level: u8 = 2;
    let mut std_path: Option<PathBuf> = None;
    let mut c_libs: Vec<String> = Vec::new();
    let mut run_args: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            run_args.extend_from_slice(&args[i + 1..]);
            break;
        } else if a == "-o" {
            i += 1;
            let val = args
                .get(i)
                .ok_or_else(|| "missing value after `-o`".to_string())?;
            output = Some(PathBuf::from(val));
        } else if a == "--std-path" {
            i += 1;
            let val = args
                .get(i)
                .ok_or_else(|| "missing value after `--std-path`".to_string())?;
            std_path = Some(PathBuf::from(val));
        } else if a == "-l" {
            i += 1;
            let val = args
                .get(i)
                .ok_or_else(|| "missing value after `-l`".to_string())?;
            c_libs.push(val.clone());
        } else if let Some(lib) = a.strip_prefix("-l") {
            c_libs.push(lib.to_string());
        } else if let Some(lvl) = a.strip_prefix("-O") {
            opt_level = lvl.parse::<u8>().unwrap_or(2).min(3);
        } else if a.starts_with('-') {
            return Err(format!("unknown flag `{}`", a));
        } else if input.is_none() {
            input = Some(PathBuf::from(a));
        } else {
            return Err(format!("unexpected positional argument `{}`", a));
        }
        i += 1;
    }

    let input = input.ok_or_else(|| "missing input `<file.lo>`".to_string())?;
    Ok(CliOpts {
        input,
        output,
        opt_level,
        std_path,
        c_libs,
        run_args,
    })
}

fn default_out_path(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a.out");
    PathBuf::from(stem)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    let cmd = args[1].as_str();
    match cmd {
        "-h" | "--help" | "help" => {
            print_usage();
        }
        "-V" | "--version" | "version" => {
            println!("loc 1.0.0");
        }
        "lex" => {
            let Some(path) = args.get(2) else {
                print_usage();
                process::exit(1);
            };
            let src = fs::read_to_string(path).unwrap_or_else(|e| {
                eprintln!("error reading `{}`: {}", path, e);
                process::exit(1);
            });
            match Lexer::tokenize(&src) {
                Ok(tokens) => {
                    for tok in tokens {
                        println!("{}:{} {:?}", tok.span.line, tok.span.col, tok.kind);
                    }
                }
                Err(e) => {
                    eprintln!("{}", e);
                    process::exit(1);
                }
            }
        }
        "ast" => {
            let Some(path) = args.get(2) else {
                print_usage();
                process::exit(1);
            };
            let src = fs::read_to_string(path).unwrap_or_else(|e| {
                eprintln!("error reading `{}`: {}", path, e);
                process::exit(1);
            });
            match Parser::parse_source(&src) {
                Ok(mut prog) => {
                    let _ = ComptimeEvaluator::evaluate_program(&mut prog);
                    print!("{}", format_program(&prog));
                }
                Err(e) => {
                    eprintln!("{}", e);
                    process::exit(1);
                }
            }
        }
        "check" => {
            let opts = parse_opts(&args[2..]).unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                process::exit(1);
            });
            if let Err(e) = TypeChecker::check_file(&opts.input, opts.std_path) {
                eprintln!("{}", e);
                process::exit(1);
            }
        }
        "emit-c" => {
            let opts = parse_opts(&args[2..]).unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                process::exit(1);
            });
            let checked = TypeChecker::check_file(&opts.input, opts.std_path).unwrap_or_else(|e| {
                eprintln!("{}", e);
                process::exit(1);
            });
            let c_code = emit_c_program(&checked);
            if let Some(out_file) = opts.output {
                fs::write(&out_file, c_code).unwrap_or_else(|e| {
                    eprintln!("error writing `{}`: {}", out_file.display(), e);
                    process::exit(1);
                });
            } else {
                print!("{}", c_code);
            }
        }
        "build" => {
            let opts = parse_opts(&args[2..]).unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                process::exit(1);
            });
            let checked = TypeChecker::check_file(&opts.input, opts.std_path).unwrap_or_else(|e| {
                eprintln!("{}", e);
                process::exit(1);
            });
            let c_code = emit_c_program(&checked);
            let out_bin = opts.output.unwrap_or_else(|| default_out_path(&opts.input));
            if let Err(e) = compile_c_to_binary(&c_code, &out_bin, opts.opt_level, &opts.c_libs) {
                eprintln!("{}", e);
                process::exit(1);
            }
        }
        "run" => {
            let opts = parse_opts(&args[2..]).unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                process::exit(1);
            });
            let checked = TypeChecker::check_file(&opts.input, opts.std_path).unwrap_or_else(|e| {
                eprintln!("{}", e);
                process::exit(1);
            });
            let c_code = emit_c_program(&checked);
            let tmp_bin = env::temp_dir().join(format!("lo_run_{}", process::id()));
            if let Err(e) = compile_c_to_binary(&c_code, &tmp_bin, opts.opt_level, &opts.c_libs) {
                eprintln!("{}", e);
                process::exit(1);
            }
            let status = Command::new(&tmp_bin)
                .args(&opts.run_args)
                .status()
                .unwrap_or_else(|e| {
                    let _ = fs::remove_file(&tmp_bin);
                    eprintln!("failed to execute compiled binary: {}", e);
                    process::exit(1);
                });
            let _ = fs::remove_file(&tmp_bin);
            process::exit(status.code().unwrap_or(1));
        }
        other if other.ends_with(".lo") => {
            let opts = parse_opts(&args[1..]).unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                process::exit(1);
            });
            let checked = TypeChecker::check_file(&opts.input, opts.std_path).unwrap_or_else(|e| {
                eprintln!("{}", e);
                process::exit(1);
            });
            let c_code = emit_c_program(&checked);
            let out_bin = opts.output.unwrap_or_else(|| default_out_path(&opts.input));
            if let Err(e) = compile_c_to_binary(&c_code, &out_bin, opts.opt_level, &opts.c_libs) {
                eprintln!("{}", e);
                process::exit(1);
            }
        }
        other => {
            eprintln!("Unknown subcommand `{}`\n", other);
            print_usage();
            process::exit(1);
        }
    }
}
