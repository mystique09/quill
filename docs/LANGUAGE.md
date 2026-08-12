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
argument. The standard library predefines `Num`, `Eq`, `Ord`, `Show`, and
`Append`, with instances for the built-in types.

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
- **Moves.** Non-`Copy` values (strings, lists, ADTs, tuples) move on use.
  Using a moved value is a compile-time error:

  ```haskell
  let s = "hello"
      t = s     -- moves s; s is now dead
  in println s  -- error: use of moved value `s`
  ```

- **Copy types.** `Int`, `Float`, `Bool`, `Char`, functions, and `&T`
  references copy freely.
- **`clone e`** makes an explicit deep copy.
- **Borrows.** `&e` borrows immutably, `&mut e` borrows mutably (requires a
  `mut` binding). Dereference with `*r`.
- **Assignment.** `x := e` requires `x` to be declared `mut`.

```haskell
main = let mut total = 0
           add x = total := total + x
       in let _ = add 5 in let _ = add 7 in println (show total)  -- 12
```

## The prelude

`+ - * / % negate`, `== /= < <= > >=`, `&& || not`, `:`, `++`,
`head tail length`, `map filter foldl foldr`, `show println error sqrt`,
`otherwise`.

## `main`

If a top-level binding named `main` exists it is evaluated and, when it yields a
non-unit value, that value is printed. `println` writes directly to stdout.

## Known simplifications

- `let` and `where` are non-recursive (top-level functions *are* recursive).
- Borrow checking is lexical and conservative; aliasing rules are best-effort.
- Type classes are single-parameter and use dynamic dispatch on the first
  argument's runtime type.
- No `do`-notation, list comprehensions, operator sections, or modules.
- Records are not open for extension; there is no record update syntax.
