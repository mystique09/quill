# Quill

**Haskell syntax. Rust power.**

Quill is a small, self-contained programming language written in Rust. It takes the
elegant, expression-oriented syntax of Haskell and pairs it with the power features
that make Rust great:

- **Static types with full type inference** — a Hindley–Milner engine with
  algebraic data types, polymorphism, and type classes.
- **Algebraic data types & pattern matching** — Rust's `enum`s, Haskell's `data`.
- **Traits (type classes)** — `class` / `instance`, with overloaded operators.
- **Ownership** — values *move* by default, `clone` duplicates, `&` / `&mut`
  borrow, and mutation requires `mut`.
- **Immutability by default** — like Rust, bindings are immutable unless declared `mut`.
- **No `null`** — use `Option` and `Result`-style ADTs instead.

Quill is a tree-walking interpreter. It lexes, parses, type-checks, borrow-checks,
and then evaluates your program.

## Quick start

```console
$ cargo run -- examples/hello.q
Hello, world!

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
data List a   = Nil | Cons a (List a)

length :: List a -> Int
length Nil        = 0
length (Cons _ t) = 1 + length t

-- Records (Rust structs).
data Person = Person { name :: String, age :: Int }

greet :: Person -> String
greet p = "Hi " ++ p.name

-- Traits / type classes.
class Area a where
  area :: a -> Float

instance Area Shape where
  area (Circle r) = 3.14159 * r * r

-- Ownership: moves, clones, borrows, and mutation.
main = let s = "owned"
           t = clone s      -- explicit copy
           r = &s           -- borrow
           mut n = 0        -- mutable binding
       in n := n + 1
```

Run the examples:

```console
$ cargo run -- examples/recursion.q
$ cargo run -- examples/adt.q
$ cargo run -- examples/typeclass.q
$ cargo run -- examples/records.q
$ cargo run -- examples/ownership.q
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
| `src/ownership.rs`| Move / mutability / borrow checker |
| `src/eval.rs`     | Tree-walking interpreter |
| `src/error.rs`    | rustc-style diagnostics |

## Status

Quill is a toy language: an interpreter, not an optimizing compiler. It is a
demonstration of how Haskell's syntax and Rust's semantics can be combined in a
small, readable codebase. Known simplifications are documented in
[docs/LANGUAGE.md](docs/LANGUAGE.md).
