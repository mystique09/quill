# The Quill Language

Quill blends Haskell's syntax with Rust's semantics. This document is the
reference for the implemented language.

## Lexical structure

- Comments: `--` to end of line, `{- ... -}` (nestable) for blocks.
- Identifiers: `lowercase` or `snake_case` (variables), `Uppercase` (type/constructor names).
- Operators: `+ - * / % == /= < <= > >= && || : ++ . & := ^ $ !`.
- Keywords: `let in if then else case of data type class instance where module use mut clone`.
- Layout: indentation is significant, exactly like Haskell. Blocks after
  `where`, `let`, `of` are delimited by indentation, and you can always use
  explicit `{ ... ; ... }` instead.

## Values and types

| Type | Example literals |
|------|------------------|
| `Int` | `42`, `0x10` |
| `Float` | `3.14`, `1.0` |
| `Bool` | `True`, `False` |
| `Char` | `'a'`, `'\n'` |
| `String` | `"hello"` |
| `[a]` | `[1, 2, 3]`, `[]`, `1 : 2 : []` |
| `(a, b)` | `(1, "two")` |
| `a -> b` | functions |
| `&T`, `&mut T` | references |

Type variables are lowercase names (`a`, `b`, …). Function types are
right-associative: `a -> b -> c` is `a -> (b -> c)`.

## Declarations

### Type signatures

```haskell
id :: a -> a
add :: Int -> Int -> Int
```

Signatures are *checked* against the inferred type.

### Functions

Functions are equations with pattern matching and optional guards:

```haskell
fib :: Int -> Int
fib 0 = 0
fib 1 = 1
fib n = fib (n - 1) + fib (n - 2)

abs :: Int -> Int
abs x
  | x >= 0    = x
  | otherwise = -x
```

`otherwise` is a predefined constant equal to `True`.

### `where` clauses

```haskell
roots :: Float -> Float -> (Float, Float)
roots a b = (r1, r2)
  where
    r1 = -b + sqrt b
    r2 = -b - sqrt b
```

### Algebraic data types

```haskell
data Option a = None | Some a
data Result e a = Err e | Ok a
data Tree a = Leaf | Node a (Tree a) (Tree a)
```

### Records

```haskell
data Person = Person { name :: String, age :: Int }
```

- Construct: `Person { name = "Ada", age = 36 }`
- Project: `p.name`
- Match: `f (Person { name = n }) = n`

### Type aliases

```haskell
type Point = (Float, Float)
```

### Type classes (traits)

```haskell
class Eq a where
  eq :: a -> a -> Bool

instance Eq Int where
  eq x y = x == y
```

Single-parameter classes only. Method dispatch happens at the type of the first
argument. The standard library predefines `Num` and `Append` (for arithmetic
operators and `++`, with instances for the built-in types).

## Modules

Quill programs can span multiple files.

- `use Shapes` imports all of `Shapes`'s exports unqualified.
- `use Shapes as S` imports them qualified: `S.area`, `S.Circle`.
- `module Shapes (Shape, Circle, area) where` declares a module and its export
  list. Listing a data type exports its constructors too.

`use` resolves a module by replacing `.` with `/` and appending `.q`, looking in
the current directory (or the importing file's directory for a file program).
The standard library modules — `Std.List`, `Std.Option`, `Std.Result`,
`Std.Function`, `Std.String`, `Std.Tuple`, `Std.Math`, `Std.IO` — are embedded
in the interpreter and need no files on disk.

`Std.List` is the *implicit prelude*: its exports (`map`, `filter`, `foldl`,
`foldr`, `head`, `tail`, `length`, `reverse`, `sum`, …) are available
unqualified in every program.

```haskell
use Std.Option as O

main = println (show (O.fromMaybe 0 (O.Some 41) + 1))
```

## Expressions

- Application by juxtaposition, e.g. `f x y` (left-associative).
- Lambdas: `\x y -> x + y`
- `let … in …`
- `if … then … else …`
- `case … of …`
- Operators, with Haskell-like precedence (`*` binds tighter than `+`, `++`
  is right-associative, etc.)
- Type ascription: `expr :: Type`
- Record field access: `expr.field`

## Ownership (the Rust part)

- **Immutability by default.** Bindings are immutable; declare `mut` to allow
  assignment with `:=`.
- **Values are shareable.** Immutable values may be used any number of times;
  memory is managed by automatic reference counting (a cycle collector is
  planned). This keeps functional code — like `filter` — natural:

  ```haskell
  filter p (x:xs) = if p x then x : filter p xs else filter p xs
  ```

- **`clone e`** makes an explicit deep copy (rarely needed now, but available).
- **Borrows.** `&e` borrows immutably, `&mut e` borrows mutably (requires a
  `mut` binding). Dereference with `*r`.
- **Assignment.** `x := e` requires `x` to be declared `mut`.

```haskell
main = let mut total = 0
           add x = total := total + x
       in let _ = add 5 in let _ = add 7 in println (show total)  -- 12
```

## The prelude

The primitive builtins: `+ - * / % negate`, `== /= < <= > >=`, `&& || not`,
`:`, `++`, `show println print error sqrt chr ord floor ceil snoc`,
`otherwise`.

`==`, `<`, and `show` are structural and work on any value; `+` requires the
`Num` class and `++` the `Append` class. The list functions (`map`, `filter`,
`foldl`, `foldr`, `head`, `tail`, `length`, `reverse`, `sum`, …) are ordinary
Quill code in `Std.List`, which is imported implicitly.

`print` is `println` without the trailing newline; in compiled binaries it
streams to stdout immediately (the tree-walking interpreter still buffers).
Strings support cons patterns: `(c:cs)` destructures a string into its first
character and the rest (when the pattern's type is `String`). `chr` maps a
byte (mod 256) to its `Char`, `ord` is its inverse (a character's code point
as an `Int`), and `snoc` appends a `Char` to a `String` — together they make
character-level I/O possible without `do`-notation. `floor` and `ceil` round
a `Float` to an `Int`; `Std.Math` builds the rest (`abs`, `pow`, `round`,
`gcd`, …) on top of them.

## Standard input

`Std.IO` provides `readLine :: () -> Option String` (yielding `None` at end
of input, so loops over stdin terminate), plus `getLines` and `readAll`:

```haskell
use Std.IO as IO
use Std.Option

echo :: Option String -> String
echo None     = "bye"
echo (Some l) = "you said: " ++ l

main = println (echo (IO.readLine ()))
```

Input works in both the interpreter and compiled binaries.

## `main`

If a top-level binding named `main` exists it is evaluated and, when it yields a
non-unit value, that value is printed. `println` writes directly to stdout.

## Compiling to native code

`quill build file.q [output]` compiles a program to a native executable using
Cranelift; `cargo run -- file.q` (or `quill file.q`) instead runs it on the
tree-walking interpreter. Both execute the same language, but they are
different engines — the interpreter is slow and buffers `print`, while
compiled binaries run fast and stream output. The compiler supports `Int`,
`Float`, `Bool`, arithmetic and
comparison (with tag-dispatched `Int`/`Float`), `if`, `case` expressions,
top-level function calls (uncurried), recursion, pattern matching over `Int`,
`Bool`, `Float`, and algebraic data types (including nested, tuple, record,
and wildcard patterns), `String` literals with `++` and `println`, lists (list
literals, `:` cons, `++` append, and list patterns), tuples, records
(construction, projection, and patterns), `let`/`where` local bindings
(including `mut` bindings and `:=`, implemented with shared cells so closures
mutate the same storage), lambdas (including those that capture enclosing
variables), higher-order functions, partial application, structural `show`,
type classes (method dispatch on the receiver's runtime constructor tag),
string cons patterns with `chr`/`ord`/`snoc`, `floor`/`ceil`, stdin via
`Std.IO`, and
the standard library (the implicit `Std.List` prelude and `use`d modules are
compiled into the object). The compiled `main` returns its result as the
process exit code.

Number formatting in `show` and line-at-a-time stdin reads are provided by a
small C runtime (`runtime/quill-rt.c`) compiled and linked by `quill build`.
Values are uniformly boxed (every value is a reference-counted heap object),
so polymorphic functions compile soundly, and releasing an object recursively
releases its fields.

## Known simplifications

- `let` and `where` are non-recursive (top-level functions *are* recursive).
- Memory management is reference counting without cycle collection (yet).
- Type classes are single-parameter and use dynamic dispatch on the first
  argument's runtime type.
- No `do`-notation, list comprehensions, or operator sections.
- Records are not open for extension; there is no record update syntax.
