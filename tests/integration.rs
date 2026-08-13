use quill::run;

fn run_ok(src: &str) -> String {
    run(src).unwrap_or_else(|d| {
        let msg = d
            .iter()
            .map(|x| x.message.clone())
            .collect::<Vec<_>>()
            .join("; ");
        panic!("unexpected error: {msg}")
    })
}

fn run_err(src: &str) -> String {
    match run(src) {
        Ok(_) => panic!("expected an error, but program succeeded"),
        Err(d) => d.iter().map(|x| x.message.clone()).collect::<Vec<_>>().join("; "),
    }
}

#[test]
fn arithmetic() {
    assert_eq!(run_ok("main = println (show (1 + 2 * 3))"), "7\n");
}

#[test]
fn recursion_and_guards() {
    let src = "fact 0 = 1\nfact n = n * fact (n - 1)\nmain = println (show (fact 5))\n";
    assert_eq!(run_ok(src), "120\n");
}

#[test]
fn adts_and_pattern_matching() {
    let src = r#"
data Option a = None | Some a
get default None = default
get default (Some x) = x
main = println (show (get 0 (Some 41) + 1))
"#;
    assert_eq!(run_ok(src), "42\n");
}

#[test]
fn records() {
    let src = r#"
data P = P { x :: Int, y :: Int }
main = println (show (P { x = 1, y = 2 }.x + P { x = 3, y = 4 }.y))
"#;
    assert_eq!(run_ok(src), "5\n");
}

#[test]
fn typeclasses() {
    let src = r#"
data Shape = Circle Float
class Area a where area :: a -> Float
instance Area Shape where area (Circle r) = 3.0 * r
main = println (show (area (Circle 2.0)))
"#;
    assert_eq!(run_ok(src), "6.0\n");
}

#[test]
fn type_error_rejected() {
    let e = run_err("main = println (show (1 + \"x\"))");
    assert!(e.contains("type mismatch"), "got: {e}");
}

#[test]
fn move_semantics() {
    // Values are shareable (immutability + ARC); using a value twice is fine.
    let src = "main = let s = \"hi\" in let t = s in println (s ++ t)";
    assert_eq!(run_ok(src), "hihi\n");
}

#[test]
fn immutability() {
    let e = run_err("main = let x = 5 in x := 6");
    assert!(e.contains("immutable"), "got: {e}");
}

#[test]
fn no_instance_rejected() {
    // `++` requires an `Append` instance; `Int` has none.
    let src = "main = println (show (1 ++ 2))";
    let r = run(src);
    assert!(r.is_err());
}

#[test]
fn show_is_structural() {
    // `show` formats any value (functions included).
    let src = "main = println (show (Some 42))";
    let src = format!("use Std.Option\n{src}");
    assert_eq!(run_ok(&src), "Some 42\n");
}

#[test]
fn higher_order() {
    let src = "main = println (show (map (\\x -> x * x) [1, 2, 3]))";
    assert_eq!(run_ok(src), "[1, 4, 9]\n");
}

#[test]
fn mutable_and_borrow() {
    let src = r#"
main = let mut n = 0 in let bump = n := n + 1 in let _ = bump in n
"#;
    assert_eq!(run_ok(src), "1\n");
}

#[test]
fn stdlib_list() {
    let src = r#"
main = println (show (reverse [1,2,3]) ++ " " ++ show (sum [1,2,3,4]) ++ " " ++ show (take 2 [5,6,7,8]))
"#;
    assert_eq!(run_ok(src), "[3, 2, 1] 10 [5, 6]\n");
}

#[test]
fn qualified_import() {
    let src = r#"
use Std.Option as O
use Std.Function as F
main = println (show (O.fromMaybe 0 (O.Some 5)) ++ " " ++ show (F.compose (\x -> x + 1) (\x -> x * 2) 10))
"#;
    assert_eq!(run_ok(src), "5 21\n");
}

#[test]
fn unqualified_import() {
    let src = r#"
use Std.Option
main = println (show (mapMaybe (\x -> x * 2) (Some 21)))
"#;
    assert_eq!(run_ok(src), "Some 42\n");
}

#[test]
fn missing_module() {
    let r = run("use No.Such.Module\nmain = println \"hi\"");
    assert!(r.is_err());
    let msg = r.unwrap_err().into_iter().map(|d| d.message).collect::<Vec<_>>().join("; ");
    assert!(msg.contains("not found"), "got: {msg}");
}

#[test]
fn native_compile() {
    // The native backend compiles a small program to object code without
    // errors. (Linking and running is exercised manually via `quill build`.)
    let dir = std::env::temp_dir().join("quill_native_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fact.q");
    std::fs::write(
        &path,
        "factorial 0 = 1\nfactorial n = n * factorial (n - 1)\nmain = factorial 5\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    assert!(object.len() > 100, "object too small: {} bytes", object.len());
    // The generated object references the compiled function symbol.
    let has_symbol = object
        .windows(b"q_factorial".len())
        .any(|w| w == b"q_factorial");
    assert!(has_symbol, "object missing `q_factorial` symbol");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_compile_adts() {
    // The native backend also compiles ADTs, `case`, and Bool patterns to
    // object code without errors.
    let dir = std::env::temp_dir().join("quill_native_adt_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("adt.q");
    std::fs::write(
        &path,
        "data Option a = None | Some a\n\
         unwrap (Some x) = x\n\
         unwrap None = 0\n\
         flag True = 10\n\
         flag False = 20\n\
         main = unwrap (Some 42) + flag True\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    assert!(object.len() > 100, "object too small: {} bytes", object.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_compile_lists() {
    // List literals, cons, append, and list patterns compile to object code.
    let dir = std::env::temp_dir().join("quill_native_list_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("list.q");
    std::fs::write(
        &path,
        "sum [] = 0\n\
         sum (x:xs) = x + sum xs\n\
         len [] = 0\n\
         len (x:xs) = 1 + len xs\n\
         main = sum ([1, 2] ++ [3, 4]) + len [5]\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    assert!(object.len() > 100, "object too small: {} bytes", object.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_compile_closures() {
    // Lambdas, higher-order functions, and partial application compile.
    let dir = std::env::temp_dir().join("quill_native_closure_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("closure.q");
    std::fs::write(
        &path,
        "add x y = x + y\n\
         inc = add 1\n\
         map f [] = []\n\
         map f (x:xs) = f x : map f xs\n\
         sum [] = 0\n\
         sum (x:xs) = x + sum xs\n\
         main = sum (map inc [1, 2]) + sum (map (\\x -> x * 2) [3])\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    assert!(object.len() > 100, "object too small: {} bytes", object.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_compile_capturing_closures() {
    // Lambdas that capture enclosing variables (closure conversion) compile.
    let dir = std::env::temp_dir().join("quill_native_capture_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("capture.q");
    std::fs::write(
        &path,
        "makeAdder n = \\x -> x + n\n\
         apply2 f x y = f x y\n\
         main = (makeAdder 10) 5 + apply2 (\\x -> \\y -> x + y) 3 4 + (let z = 7 in (\\w -> w + z) 1)\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    assert!(object.len() > 100, "object too small: {} bytes", object.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_compile_strings() {
    // String literals, `++`, and `println` compile to object code.
    let dir = std::env::temp_dir().join("quill_native_str_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("str.q");
    std::fs::write(
        &path,
        "greet name = \"Hello, \" ++ name ++ \"!\"\nmain = println (greet \"Ada\")\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    assert!(object.len() > 100, "object too small: {} bytes", object.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_compile_show_floats_tuples() {
    // Floats, tuples, and structural `show` compile to object code.
    let dir = std::env::temp_dir().join("quill_native_show_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("show.q");
    std::fs::write(
        &path,
        "data Shape = Circle Float\n\
         pair x y = (x, y)\n\
         main = println (show (Circle 2.0) ++ \" \" ++ show (pair 1 2))\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    assert!(object.len() > 100, "object too small: {} bytes", object.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_compile_records() {
    // Record construction, projection, and patterns compile to object code.
    let dir = std::env::temp_dir().join("quill_native_record_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("record.q");
    std::fs::write(
        &path,
        "data P = P { x :: Int, y :: Int }\n\
         bump :: P -> P\n\
         bump p = P { x = p.x + 1, y = p.y }\n\
         sumXY :: P -> Int\n\
         sumXY (P { x = a, y = b }) = a + b\n\
         main = println (show (sumXY (bump (P { x = 40, y = 1 }))))\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    assert!(object.len() > 100, "object too small: {} bytes", object.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_compile_typeclasses() {
    // Class methods dispatch on the receiver's runtime tag.
    let dir = std::env::temp_dir().join("quill_native_class_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("class.q");
    std::fs::write(
        &path,
        "data Shape = Circle Float | Square Float\n\
         class Area a where area :: a -> Float\n\
         instance Area Shape where\n\
         \x20 area (Circle r) = 3.0 * r\n\
         \x20 area (Square s) = s * s\n\
         main = println (show (area (Circle 2.0) + area (Square 3.0)))\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    assert!(object.len() > 100, "object too small: {} bytes", object.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_compile_mutation() {
    // `mut` bindings, `:=`, and closures that mutate their captures compile.
    let dir = std::env::temp_dir().join("quill_native_mut_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mut.q");
    std::fs::write(
        &path,
        "counter = let mut n = 0\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20bump x = n := n + x\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20in let _ = bump 3 in let _ = bump 4 in n\n\
         main = println (show counter)\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    assert!(object.len() > 100, "object too small: {} bytes", object.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_compile_prelude() {
    // The implicit Std.List prelude resolves in native builds.
    let dir = std::env::temp_dir().join("quill_native_prelude_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("prelude.q");
    std::fs::write(
        &path,
        "double x = x * 2\n\
         main = println (show (map double [1, 2, 3]))\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    assert!(object.len() > 100, "object too small: {} bytes", object.len());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Link and run a compiled program, asserting its stdout. This exercises the
/// runtime (reference counting, boxing, `show`, dispatch) end to end; the
/// other native tests only check object emission. Skipped when no C compiler
/// is available.
#[test]
fn native_link_and_run() {
    let dir = std::env::temp_dir().join("quill_native_run_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("run.q");
    std::fs::write(
        &path,
        "data Shape = Circle Float | Square Float\n\
         class Area a where area :: a -> Float\n\
         instance Area Shape where\n\
         \x20 area (Circle r) = 3.0 * r\n\
         \x20 area (Square s) = s * s\n\
         data P = P { x :: Int, y :: Int }\n\
         bump :: P -> P\n\
         bump p = P { x = p.x + 1, y = p.y }\n\
         counter = let mut n = 0\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20bumpC x = n := n + x\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20in let _ = bumpC 3 in let _ = bumpC 4 in n\n\
         main = println\n\
         \x20 ( show (area (Circle 2.0)) ++ \" \" ++ show (area (Square 3.0))\n\
         \x20   ++ \" \" ++ show (bump (P { x = 40, y = 1 })).x\n\
         \x20   ++ \" \" ++ show counter ++ \" \" ++ show (map (\\x -> x * 2) [1, 2, 3])\n\
         \x20   ++ \" \" ++ show (1, \"two\") )\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    let obj_path = dir.join("run.o");
    std::fs::write(&obj_path, &object).unwrap();
    let rt_path = dir.join("rt.c");
    std::fs::write(&rt_path, include_str!("../runtime/quill-rt.c")).unwrap();
    let bin_path = dir.join("run");
    let status = std::process::Command::new("cc")
        .arg(&obj_path)
        .arg(&rt_path)
        .arg("-o")
        .arg(&bin_path)
        .status()
        .unwrap();
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return; // no C toolchain: skip the runtime assertions
    }
    let output = std::process::Command::new(&bin_path).output().unwrap();
    assert!(
        output.status.success(),
        "compiled program failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "6.0 9.0 41 7 [2, 4, 6] (1, two)\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn brainfuck_example() {
    // The Brainfuck interpreter example parses a real Brainfuck string and
    // prints "Hello World!" (on a large stack, since the interpreter
    // recurses per step).
    // Use the example's interpreter functions, but pin `main` to the fast
    // Hello World program (the checked-in main runs Mandelbrot, which takes
    // minutes in the tree-walking interpreter).
    let example = include_str!("../examples/brainfuck/Main.q");
    let src = example.split("main = ").next().unwrap().to_string()
        + "main = println (brainfuck hello)\n";
    let output = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(move || run(&src))
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(output.unwrap(), "Hello World!\n");
}

#[test]
fn stdlib_string() {
    let src = r#"
use Std.String as S
main = println
  ( S.toUpper "hElLo" ++ " " ++ show (S.lines "a\nb\nc") ++ " "
    ++ show (S.words "foo  bar\tbaz") ++ " " ++ show (S.split ',' "a,,b,c") ++ " "
    ++ S.join "-" ["x", "y"] ++ " " ++ S.trim "  hi  " ++ " "
    ++ show (S.startsWith "he" "hello") ++ " " ++ show (S.endsWith "lo" "hello") ++ " "
    ++ show (S.contains "ell" "hello") ++ " " ++ S.fromChars ['a', 'b'] ++ " "
    ++ show (S.toChars "ab") ++ " " ++ S.repeat "ab" 2 )
"#;
    assert_eq!(
        run_ok(src),
        "HELLO [a, b, c] [foo, bar, baz] [a, , b, c] x-y hi true true true ab ['a', 'b'] abab\n"
    );
}

#[test]
fn stdlib_tuple_math() {
    let src = r#"
use Std.Tuple as T
use Std.Math as M
main = println
  ( show (T.fst (1, 2)) ++ " " ++ show (T.snd (1, 2)) ++ " " ++ show (T.swap (1, 2)) ++ " "
    ++ show (T.uncurry (\x y -> x + y) (2, 3)) ++ " " ++ show (M.abs (-5)) ++ " "
    ++ show (M.pow 2 8) ++ " " ++ show (M.min 3 7) ++ " " ++ show (M.max 3 7) ++ " "
    ++ show (M.clamp 0 10 42) ++ " " ++ show (M.even 4) ++ " " ++ show (M.odd 4) ++ " "
    ++ show (M.gcd 12 18) ++ " " ++ show (M.round 2.5) ++ " " ++ show (floor 2.7)
    ++ " " ++ show (ceil 2.1) ++ " " ++ show (ord 'A') )
"#;
    assert_eq!(
        run_ok(src),
        "1 2 (2, 1) 5 5 256 3 7 10 true false 6 3 2 3 65\n"
    );
}

#[test]
fn stdlib_io_typechecks() {
    // Std.IO's readLine is typed `() -> Option String` via a native helper;
    // this checks the module loads and typechecks (stdin is not exercised).
    let src = r#"
use Std.IO as IO
use Std.Option
main = println (case IO.readLine () of { None -> "no input"; Some l -> l })
"#;
    quill::check(src).unwrap();
}

#[test]
fn char_escapes() {
    // Regression: escapes must work in both char and string literals (the
    // lexer used to skip the backslash twice, breaking '\t' and friends).
    let src = r#"
main = println (show ('\n' == '\n') ++ " " ++ show ("a\nb" == "a" ++ "\n" ++ "b") ++ " " ++ show (ord '\t'))
"#;
    assert_eq!(run_ok(src), "true true 9\n");
}

#[test]
fn native_compile_stdlib_string() {
    // Std.String exercises the compiled backend's string cons patterns,
    // `chr`/`ord`/`snoc`, and `++`.
    let dir = std::env::temp_dir().join("quill_native_stdstr_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.q");
    std::fs::write(
        &path,
        "use Std.String as S\n\
         use Std.Math as M\n\
         main = println\n\
         \x20 ( S.toUpper \"hi\" ++ S.trim \" x \" ++ show (S.split ',' \"a,b\")\n\
         \x20   ++ \" \" ++ show (M.round 2.5) ++ \" \" ++ show (floor 2.7) ++ \" \" ++ show (ord 'A') )\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    assert!(
        object.len() > 100,
        "object too small: {} bytes",
        object.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Link and run a compiled program that uses the new stdlib modules and
/// reads a line from piped stdin. Skipped when no C compiler is available.
#[test]
fn native_link_and_run_stdlib() {
    let dir = std::env::temp_dir().join("quill_native_stdlib_run_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("run.q");
    std::fs::write(
        &path,
        "use Std.IO as IO\n\
         use Std.Option\n\
         use Std.String as S\n\
         use Std.Math as M\n\
         showLine :: Option String -> String\n\
         showLine None = \"none\"\n\
         showLine (Some l) = l\n\
         main = println\n\
         \x20 ( S.toUpper (showLine (IO.readLine ())) ++ \" \" ++ show (S.lines \"a\\nb\")\n\
         \x20   ++ \" \" ++ show (M.round 2.5) ++ \" \" ++ show (floor 2.7) ++ \" \" ++ show (ord 'A') )\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    let obj_path = dir.join("run.o");
    std::fs::write(&obj_path, &object).unwrap();
    let rt_path = dir.join("rt.c");
    std::fs::write(&rt_path, include_str!("../runtime/quill-rt.c")).unwrap();
    let bin_path = dir.join("run");
    let status = std::process::Command::new("cc")
        .arg(&obj_path)
        .arg(&rt_path)
        .arg("-o")
        .arg(&bin_path)
        .status()
        .unwrap();
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return; // no C toolchain: skip the runtime assertions
    }
    // Feed one line on stdin and read the process's stdout.
    let mut child = std::process::Command::new(&bin_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), b"hello world\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "compiled program failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "HELLO WORLD [a, b] 3 2 65\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn readline_intrinsic_is_typed() {
    // `_readLine` is internal to Std.IO: it is never bound for user code (the
    // intrinsic lives under the module-qualified flat name `Std.IO._readLine`,
    // which source identifiers cannot spell), so direct use is a compile-time
    // error — never a runtime crash or garbage.
    let e = run_err("main = println (show (_readLine () + 1))");
    assert!(e.contains("unbound name"), "got: {e}");
    let e = run_err("use Std.IO\nmain = println (show (_readLine () + 1))");
    assert!(e.contains("unbound name"), "got: {e}");
    // A user's own `_readLine` definition is a perfectly ordinary function.
    let src = "_readLine _ = 42\nmain = println (show (_readLine ()))";
    assert_eq!(run_ok(src), "42\n");
}

#[test]
fn readline_intrinsic_isolated_from_user_defs() {
    // Even when the importing program defines `_readLine`, Std.IO's readLine
    // must keep calling the intrinsic (typed `() -> Option String`), not the
    // user's function. (stdin is not exercised here; this is typecheck-only.)
    let src = r#"
use Std.IO as IO
use Std.Option
_readLine _ = 42
main = println (case IO.readLine () of { None -> "none"; Some l -> l })
"#;
    quill::check(src).unwrap();
}

/// Compiled programs preserve embedded NUL bytes in stdin lines (the C
/// runtime reports the line length instead of relying on strlen). Observed
/// through `toChars`, since printing the raw string would stop at the NUL.
/// Skipped when no C compiler is available.
#[test]
fn native_readline_nul() {
    let dir = std::env::temp_dir().join("quill_native_nul_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nul.q");
    std::fs::write(
        &path,
        "use Std.IO as IO\n\
         use Std.Option\n\
         use Std.String as S\n\
         line :: Option String -> String\n\
         line None = \"\"\n\
         line (Some l) = l\n\
         main = println (show (length (S.toChars (line (IO.readLine ())))))\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    let obj_path = dir.join("nul.o");
    std::fs::write(&obj_path, &object).unwrap();
    let rt_path = dir.join("rt.c");
    std::fs::write(&rt_path, include_str!("../runtime/quill-rt.c")).unwrap();
    let bin_path = dir.join("nul");
    let status = std::process::Command::new("cc")
        .arg(&obj_path)
        .arg(&rt_path)
        .arg("-o")
        .arg(&bin_path)
        .status()
        .unwrap();
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return; // no C toolchain: skip the runtime assertions
    }
    let mut child = std::process::Command::new(&bin_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), b"a\0b\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "compiled program failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "3\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtin_shadowed_by_user_definition() {
    // A top-level definition with a builtin's name replaces the prelude
    // entry in the interpreter (the native half is native_builtin_shadowing).
    let src = r#"
_readLine _ = 42
show _ = 42
main = println (show (_readLine () + show 1))
"#;
    assert_eq!(run_ok(src), "42\n");
}

/// Compiled programs also let user definitions shadow builtins of the same
/// name — the result is returned as the exit code (and no stdin is read,
/// since the user's `_readLine` replaces the intrinsic). Skipped when no C
/// compiler is available.
#[test]
fn native_builtin_shadowing() {
    let dir = std::env::temp_dir().join("quill_native_shadow_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("shadow.q");
    std::fs::write(
        &path,
        "_readLine _ = 40\n\
         show _ = 2\n\
         main = _readLine () + show 1\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    let obj_path = dir.join("shadow.o");
    std::fs::write(&obj_path, &object).unwrap();
    let rt_path = dir.join("rt.c");
    std::fs::write(&rt_path, include_str!("../runtime/quill-rt.c")).unwrap();
    let bin_path = dir.join("shadow");
    let status = std::process::Command::new("cc")
        .arg(&obj_path)
        .arg(&rt_path)
        .arg("-o")
        .arg(&bin_path)
        .status()
        .unwrap();
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return; // no C toolchain: skip the runtime assertions
    }
    let output = std::process::Command::new(&bin_path).output().unwrap();
    // `main` returns the value, which becomes the process exit code: 42
    // proves both shadowing definitions ran (40 + 2).
    assert_eq!(
        output.status.code(),
        Some(42),
        "expected exit code 42, got {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Compiled `println` formats any value structurally (Int, Bool, Char,
/// lists, tuples) instead of trapping on non-strings — parity with the
/// interpreter. Skipped when no C compiler is available.
#[test]
fn native_println_any_value() {
    let dir = std::env::temp_dir().join("quill_native_println_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("p.q");
    std::fs::write(
        &path,
        "f x = x + 2\n\
         main = let _ = println (f 40) in\n\
         \x20\x20\x20\x20\x20\x20\x20let _ = println True in\n\
         \x20\x20\x20\x20\x20\x20\x20let _ = println [1, 2] in\n\
         \x20\x20\x20\x20\x20\x20\x20let _ = println (1, \"a\") in\n\
         \x20\x20\x20\x20\x20\x20\x20println 'x'\n",
    )
    .unwrap();
    let object = quill::compile_file(&path).unwrap();
    let obj_path = dir.join("p.o");
    std::fs::write(&obj_path, &object).unwrap();
    let rt_path = dir.join("rt.c");
    std::fs::write(&rt_path, include_str!("../runtime/quill-rt.c")).unwrap();
    let bin_path = dir.join("p");
    let status = std::process::Command::new("cc")
        .arg(&obj_path)
        .arg(&rt_path)
        .arg("-o")
        .arg(&bin_path)
        .status()
        .unwrap();
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return; // no C toolchain: skip the runtime assertions
    }
    let output = std::process::Command::new(&bin_path).output().unwrap();
    assert!(
        output.status.success(),
        "compiled program failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "42\ntrue\n[1, 2]\n(1, a)\n'x'\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
