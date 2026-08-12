-- Compilable by the native backend (`quill build examples/native.q`).
--
-- The native compiler supports Int, Bool, arithmetic and comparison,
-- `if`/`case`, recursion, and algebraic data types with pattern matching
-- (including nested and wildcard patterns).

factorial :: Int -> Int
factorial 0 = 1
factorial n = n * factorial (n - 1)

data List a = Nil | Cons a (List a)

length :: List a -> Int
length Nil = 0
length (Cons _ t) = 1 + length t

main = factorial 5 + length (Cons 1 (Cons 2 (Cons 3 Nil)))
