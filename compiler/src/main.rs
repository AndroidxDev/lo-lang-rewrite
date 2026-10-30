use std::env;
use std::fs;
use std::process;

use loc::lexer::Lexer;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 || args[1] != "lex" {
        eprintln!("Usage: loc lex <file.lo>");
        process::exit(1);
    }
    let path = &args[2];
    let src = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading `{}`: {}", path, e);
            process::exit(1);
        }
    };
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
