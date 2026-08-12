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
