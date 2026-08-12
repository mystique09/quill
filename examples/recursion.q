-- Recursion and pattern matching, Haskell style.

factorial :: Int -> Int
factorial 0 = 1
factorial n = n * factorial (n - 1)

fib :: Int -> Int
fib n
  | n <= 1    = n
  | otherwise = fib (n - 1) + fib (n - 2)

main = println (show (factorial 10) ++ " " ++ show (fib 20))
