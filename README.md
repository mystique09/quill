# Quill

**Haskell syntax. Rust power.**

Quill is a small, self-contained programming language written in Rust. It takes the
elegant, expression-oriented syntax of Haskell and pairs it with the power features
that make Rust great:

- **Static types with full type inference** — a Hindley–Milner engine with
  algebraic data types, polymorphism, and type classes.
- **Algebraic data types & pattern matching** — Rust's `enum`s, Haskell's `data`.
- **Traits (type classes)** — `class` / `instance`, with overloaded operators.
- **A module system** — multi-file programs with `use`, qualified imports, and
  export lists.
- **A standard library written in Quill itself** — `Std.List`, `Std.Option`,
  `Std.Result`, `Std.Function`.
- **Immutability by default** — like Rust, bindings are immutable unless declared `mut`,
  with `&`/`&mut` borrows. Memory is managed by automatic reference counting.
- **No `null`** — use `Option` and `Result` instead.

Quill is a tree-walking interpreter with a native compiler backend (Cranelift).
The interpreter runs `.q` files or a REPL; `quill build` compiles a program to a
native executable.

## Quick start

```console
$ cargo run -- examples/hello.q
Hello, world!

$ cargo run -- build examples/recursion.q quill-fact   # native binary
$ cargo run              # start the REPL
```

## A taste of Quill

```haskell
-- Type signatures use `::` (and are checked, not just comments).
factorial :: Int -> Int
factorial 0 = 1
factorial n = n * factorial (n - 1)

-- Guards, like Haskell.
fib :: Int -> Int
fib n
  | n <= 1    = n
  | otherwise = fib (n - 1) + fib (n - 2)

-- Algebraic data types (Rust enums).
data Option a = None | Some a

-- Records (Rust structs).
data Person = Person { name :: String, age :: Int }

-- Traits / type classes.
class Area a where
  area :: a -> Float

-- Modules: import the standard library (written in Quill itself).
use Std.Option as O

-- Immutability by default; `mut` for mutation.
main = let mut n = 0 in let _ = n := n + 1 in println (show (O.fromMaybe 0 (O.Some 41) + n))
```

Run the examples:

```console
$ cargo run -- examples/hello.q
$ cargo run -- examples/recursion.q
$ cargo run -- examples/stdlib.q
$ cargo run -- examples/modules/Main.q    # multi-file program
```

See [docs/LANGUAGE.md](docs/LANGUAGE.md) for the full language reference.

## Project layout

| Path | Contents |
|------|----------|
| `src/lexer.rs`    | Tokenizer with Haskell-style indentation layout |
| `src/parser.rs`   | Recursive-descent parser → AST |
| `src/ast.rs`      | Syntax tree definitions |
| `src/types.rs`    | Type representation |
| `src/infer.rs`    | Hindley–Milner type inference + type classes |
| `src/modules.rs`  | Module loader (imports, export lists, name resolution) |
| `src/compile.rs`  | Native codegen via Cranelift (`quill build`) |
| `src/ownership.rs`| Mutability checker |
| `src/eval.rs`     | Tree-walking interpreter |
| `src/error.rs`    | rustc-style diagnostics |
| `stdlib/`         | The standard library, written in Quill |

## Status

Quill is a toy language: an interpreter, not an optimizing compiler. It is a
demonstration of how Haskell's syntax and Rust's semantics can be combined in a
small, readable codebase. Known simplifications are documented in
[docs/LANGUAGE.md](docs/LANGUAGE.md).
