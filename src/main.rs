use std::io::{self, Write};

use quill::error::render;
use quill::run;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let path = &args[1];
        match std::fs::read_to_string(path) {
            Ok(src) => {
                run_and_report(&src);
            }
            Err(e) => {
                eprintln!("error: could not read `{path}`: {e}");
                std::process::exit(1);
            }
        }
    } else {
        repl();
    }
}

fn run_and_report(src: &str) {
    match run(src) {
        Ok(output) => {
            print!("{output}");
            io::stdout().flush().ok();
        }
        Err(diags) => {
            for d in &diags {
                eprint!("{}", render(src, d));
            }
            std::process::exit(1);
        }
    }
}

fn repl() {
    println!("Quill v0.1.0 — Haskell syntax, Rust power. Ctrl-D to exit.");
    let mut defs = String::new();
    let mut buf = String::new();
    loop {
        print!("> ");
        io::stdout().flush().ok();
        buf.clear();
        let n = io::stdin().read_line(&mut buf).ok();
        if n.is_none_or(|n| n == 0) {
            println!();
            break;
        }
        let line = buf.trim_end();
        if line.is_empty() {
            continue;
        }
        // Heuristic: a line with a top-level `=` (but no `in`) is a
        // definition; otherwise treat it as an expression to evaluate.
        let is_def = line.contains('=')
            && !line.contains(" in ")
            && !line.starts_with("let ");
        let src = if is_def {
            defs.push_str(line);
            defs.push('\n');
            defs.clone()
        } else {
            format!("{defs}main = {line}\n")
        };
        match run(&src) {
            Ok(output) => {
                if is_def {
                    // Definitions produce no output unless they are `main`.
                }
                print!("{output}");
                io::stdout().flush().ok();
            }
            Err(diags) => {
                if is_def {
                    // Roll back the failed definition.
                    defs.truncate(defs.len() - line.len() - 1);
                }
                for d in &diags {
                    eprint!("{}", render(&src, d));
                }
            }
        }
    }
}
