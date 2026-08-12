-- Algebraic data types: Rust's enums with Haskell's syntax.

data Option a = None | Some a

data List a = Nil | Cons a (List a)

-- Pattern-matching function clauses.
length :: List a -> Int
length Nil        = 0
length (Cons _ t) = 1 + length t

unwrap :: a -> Option a -> a
unwrap default None     = default
unwrap default (Some x) = x

-- `case` expressions and nested patterns.
describe :: Option Int -> String
describe x =
  case x of
    None    -> "nothing"
    Some 0  -> "zero"
    Some n  -> "some " ++ show n

main = println
  ( show (length (Cons 1 (Cons 2 (Cons 3 Nil))))
    ++ " " ++ describe None
    ++ " " ++ describe (Some 0)
    ++ " " ++ describe (Some 7) )
