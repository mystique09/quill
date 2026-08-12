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
    let e = run_err("main = let s = \"hi\" in let t = s in println s");
    assert!(e.contains("moved value"), "got: {e}");
}

#[test]
fn immutability() {
    let e = run_err("main = let x = 5 in x := 6");
    assert!(e.contains("immutable"), "got: {e}");
}

#[test]
fn no_instance_rejected() {
    let src = "main = println (show (\\f -> f))\n";
    // `show` on a function has no instance.
    let r = run(src);
    assert!(r.is_err());
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
