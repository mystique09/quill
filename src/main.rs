use std::io::{self, Write};
use std::path::{Path, PathBuf};

use quill::error::render;
use quill::{run, run_file};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "build" {
        build(&args);
        return;
    }
    if args.len() > 1 {
        let path = Path::new(&args[1]);
        let src = std::fs::read_to_string(path).unwrap_or_else(|e| {
            eprintln!("error: could not read `{}`: {e}", path.display());
            std::process::exit(1);
        });
        match run_file(path) {
            Ok(output) => {
                print!("{output}");
                io::stdout().flush().ok();
            }
            Err(diags) => {
                for d in &diags {
                    eprint!("{}", render(&src, d));
                }
                std::process::exit(1);
            }
        }
    } else {
        repl();
    }
}

/// `quill build file.q` — compile to a native executable.
fn build(args: &[String]) {
    let src_path = args.get(2).map(Path::new).unwrap_or_else(|| {
        eprintln!("usage: quill build <file.q> [output]");
        std::process::exit(1);
    });
    let out_path: PathBuf = match args.get(3) {
        Some(p) => PathBuf::from(p),
        None => Path::new(src_path).with_extension(""),
    };    let src = std::fs::read_to_string(src_path).unwrap_or_else(|e| {
        eprintln!("error: could not read `{}`: {e}", src_path.display());
        std::process::exit(1);
    });
    match quill::compile_file(src_path) {
        Ok(object) => {
            let obj_path = out_path.with_extension("o");
            if let Err(e) = std::fs::write(&obj_path, &object) {
                eprintln!("error: could not write `{}`: {e}", obj_path.display());
                std::process::exit(1);
            }
            let status = std::process::Command::new("cc")
                .arg(&obj_path)
                .arg("-o")
                .arg(&out_path)
                .status();
            match status {
                Ok(s) if s.success() => {
                    let _ = std::fs::remove_file(&obj_path);
                    println!("built {}", out_path.display());
                }
                Ok(s) => {
                    eprintln!("error: linker exited with {s}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: could not invoke `cc` to link: {e}");
                    std::process::exit(1);
                }
            }
        }
        Err(diags) => {
            for d in &diags {
                eprint!("{}", render(&src, d));
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
        // Heuristic: a declaration (`use`, `data`, `class`, …) or a line with
        // a top-level `=` (but no `in`) is a definition; otherwise treat it
        // as an expression to evaluate.
        let trimmed = line.trim_start();
        let is_decl_kw = ["use ", "module ", "data ", "type ", "class ", "instance "]
            .iter()
            .any(|k| trimmed.starts_with(k));
        let is_def = is_decl_kw
            || (line.contains('=')
                && !line.contains(" in ")
                && !line.starts_with("let "));
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
