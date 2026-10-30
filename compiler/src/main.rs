use std::env;
use std::fs;
use std::process;

use loc::lexer::Lexer;
use loc::parser::{format_program, Parser};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: loc <lex|ast> <file.lo>");
        process::exit(1);
    }
    let cmd = args[1].as_str();
    let path = &args[2];
    let src = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading `{}`: {}", path, e);
            process::exit(1);
        }
    };

    match cmd {
        "lex" => match Lexer::tokenize(&src) {
            Ok(tokens) => {
                for tok in tokens {
                    println!("{}:{} {:?}", tok.span.line, tok.span.col, tok.kind);
                }
            }
            Err(e) => {
                eprintln!("{}", e);
                process::exit(1);
            }
        },
        "ast" => match Parser::parse_source(&src) {
            Ok(prog) => print!("{}", format_program(&prog)),
            Err(e) => {
                eprintln!("{}", e);
                process::exit(1);
            }
        },
        other => {
            eprintln!("Unknown subcommand `{}`", other);
            process::exit(1);
        }
    }
}
