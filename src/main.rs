//! La3: a lexer, parser, checker, and tree-walking interpreter for Laila Lang.
//!
//! Usage:
//!   la3 run   <file.la3>   parse, check, and execute (calls `main`)
//!   la3 check <file.la3>   parse and report undefined-name errors
//!   la3 ast   <file.la3>   parse and print the AST
//!   la3 tokens <file.la3>  print the token stream (debugging)
//!   la3 types <file.la3>   print the inferred type of every expression (debugging)
//!   la3 build <file.la3>   compile to a native binary (WIP, see COMPILER_PLAN.md)

mod ast;
mod checker;
mod diag;
mod interp;
mod lexer;
mod parser;
mod typeck;

use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: la3 <run|check|build|ast|tokens|types> <file.la3>");
        exit(2);
    }
    let cmd = args[1].as_str();
    let path = &args[2];
    let src = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("la3: cannot read {}: {}", path, e);
        exit(2);
    });

    match cmd {
        "tokens" => match lexer::Lexer::new(&src).tokenize() {
            Ok(toks) => {
                for t in toks {
                    println!("{:>4}:{:<3} {:?}", t.pos.line, t.pos.col, t.tok);
                }
            }
            Err(d) => fail(&d, path, &src),
        },
        "ast" => match parser::parse(&src) {
            Ok(prog) => println!("{:#?}", prog),
            Err(d) => fail(&d, path, &src),
        },
        "types" => {
            // Debug view: the inferred type of every expression node. Surfaces
            // the type table the compiler back-end will consume (Phase 1.1).
            let prog = match parser::parse(&src) {
                Ok(p) => p,
                Err(d) => fail(&d, path, &src),
            };
            let table = typeck::check_types(&prog);
            print!("{}", table.dump());
            if !table.errors.is_empty() {
                for d in &table.errors {
                    eprintln!("{}\n", d.render(path, &src));
                }
                eprintln!("{} type error(s)", table.errors.len());
                exit(1);
            }
        }
        "check" => {
            let prog = match parser::parse(&src) {
                Ok(p) => p,
                Err(d) => fail(&d, path, &src),
            };
            let errs = checker::check(&prog);
            if errs.is_empty() {
                println!("ok: no errors in {}", path);
            } else {
                for d in &errs {
                    eprintln!("{}\n", d.render(path, &src));
                }
                eprintln!("{} error(s)", errs.len());
                exit(1);
            }
        }
        "run" => {
            let prog = match parser::parse(&src) {
                Ok(p) => p,
                Err(d) => fail(&d, path, &src),
            };
            let errs = checker::check(&prog);
            if !errs.is_empty() {
                for d in &errs {
                    eprintln!("{}\n", d.render(path, &src));
                }
                exit(1);
            }
            let mut interp = interp::Interp::new();
            // Arguments after the file path are exposed via `os.args()`. A lone
            // `--` separator (e.g. `la3 run f.la3 -- a b`) is dropped.
            let mut prog_args = &args[3..];
            if prog_args.first().map(|s| s.as_str()) == Some("--") {
                prog_args = &prog_args[1..];
            }
            interp.set_args(prog_args.to_vec());
            if let Err(d) = interp.run(&prog) {
                fail(&d, path, &src);
            }
        }
        "build" => {
            // Front-end is shared with the interpreter: parse, then run the
            // checker. Codegen itself lands in Phase 4 (see COMPILER_PLAN.md);
            // for now `build` proves the front-end accepts the program and
            // reports that the LLVM backend is not wired yet.
            let prog = match parser::parse(&src) {
                Ok(p) => p,
                Err(d) => fail(&d, path, &src),
            };
            let errs = checker::check(&prog);
            if !errs.is_empty() {
                for d in &errs {
                    eprintln!("{}\n", d.render(path, &src));
                }
                exit(1);
            }
            eprintln!(
                "la3: front-end OK for {}; native codegen is not implemented yet \
                 (LLVM backend lands in Phase 4 — see COMPILER_PLAN.md)",
                path
            );
            exit(3);
        }
        other => {
            eprintln!("la3: unknown command '{}'", other);
            exit(2);
        }
    }
}

fn fail(d: &diag::Diagnostic, path: &str, src: &str) -> ! {
    eprintln!("{}", d.render(path, src));
    exit(1);
}
