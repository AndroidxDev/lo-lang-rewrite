use lomake::{cmd_build, cmd_clean, cmd_init, cmd_run, cmd_test};
use std::env;
use std::path::PathBuf;
use std::process;

fn print_usage() {
    eprintln!(
        "lomake v1.0.0 — Build system & project manager for the lo language\n\n\
         Usage:\n  \
           lomake init <name> [--dir <parent_dir>]\n  \
           lomake build [--dir <project_dir>]\n  \
           lomake run   [--dir <project_dir>] [-- <args...>]\n  \
           lomake test  [--dir <project_dir>]\n  \
           lomake clean [--dir <project_dir>]"
    );
}

fn parse_project_dir_and_args(args: &[String]) -> Result<(PathBuf, Vec<String>), String> {
    let mut dir = env::current_dir().map_err(|e| e.to_string())?;
    let mut passthrough = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            passthrough.extend_from_slice(&args[i + 1..]);
            break;
        } else if args[i] == "--dir" || args[i] == "-C" {
            i += 1;
            let val = args
                .get(i)
                .ok_or_else(|| "missing directory path after `--dir`".to_string())?;
            dir = PathBuf::from(val);
        } else if args[i] == "--manifest" {
            i += 1;
            let val = args
                .get(i)
                .ok_or_else(|| "missing file path after `--manifest`".to_string())?;
            let mp = PathBuf::from(val);
            dir = mp
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
        } else {
            return Err(format!("unexpected argument `{}`", args[i]));
        }
        i += 1;
    }
    Ok((dir, passthrough))
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    match args[1].as_str() {
        "-h" | "--help" | "help" => {
            print_usage();
        }
        "-V" | "--version" | "version" => {
            println!("lomake 1.0.0");
        }
        "init" => {
            let Some(name) = args.get(2) else {
                eprintln!("error: missing project `<name>` for `lomake init <name>`");
                process::exit(1);
            };
            let (base_dir, _) = parse_project_dir_and_args(&args[3..]).unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                process::exit(1);
            });
            match cmd_init(&base_dir, name) {
                Ok(path) => {
                    println!(
                        "[lomake] Initialized new lo project at `{}`",
                        path.display()
                    );
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    process::exit(1);
                }
            }
        }
        "build" => {
            let (proj_dir, _) = parse_project_dir_and_args(&args[2..]).unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                process::exit(1);
            });
            match cmd_build(&proj_dir) {
                Ok(rep) => {
                    if rep.cache_hit {
                        println!(
                            "[lomake] Up-to-date (cached): {} ({} modules, hash {})",
                            rep.binary_path.display(),
                            rep.module_count,
                            &rep.merkle_hash[..12]
                        );
                    } else {
                        println!(
                            "[lomake] Built {} ({} modules, hash {})",
                            rep.binary_path.display(),
                            rep.module_count,
                            &rep.merkle_hash[..12]
                        );
                    }
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    process::exit(1);
                }
            }
        }
        "run" => {
            let (proj_dir, run_args) = parse_project_dir_and_args(&args[2..]).unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                process::exit(1);
            });
            match cmd_run(&proj_dir, &run_args) {
                Ok(code) => process::exit(code),
                Err(e) => {
                    eprintln!("error: {}", e);
                    process::exit(1);
                }
            }
        }
        "test" => {
            let (proj_dir, _) = parse_project_dir_and_args(&args[2..]).unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                process::exit(1);
            });
            match cmd_test(&proj_dir) {
                Ok(summary) => {
                    for (name, ok, detail) in &summary.results {
                        if *ok {
                            println!("  [PASS] {}", name);
                        } else {
                            println!("  [FAIL] {} — {}", name, detail);
                        }
                    }
                    println!(
                        "[lomake] Tests: {} passed, {} failed, {} total",
                        summary.passed,
                        summary.failed,
                        summary.passed + summary.failed
                    );
                    if summary.failed > 0 {
                        process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    process::exit(1);
                }
            }
        }
        "clean" => {
            let (proj_dir, _) = parse_project_dir_and_args(&args[2..]).unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                process::exit(1);
            });
            match cmd_clean(&proj_dir) {
                Ok(true) => println!("[lomake] Cleaned `build/`"),
                Ok(false) => println!("[lomake] Nothing to clean"),
                Err(e) => {
                    eprintln!("error: {}", e);
                    process::exit(1);
                }
            }
        }
        other => {
            eprintln!("error: unknown command `{}`\n", other);
            print_usage();
            process::exit(1);
        }
    }
}
