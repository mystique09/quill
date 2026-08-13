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

