-- Higher-order functions, lambdas, and list literals.

double x = x * 2

evens xs = filter (\x -> x % 2 == 0) xs

summate xs = foldl (\acc x -> acc + x) 0 xs

main = println
  ( show (map double [1, 2, 3, 4, 5])
    ++ " " ++ show (evens [1, 2, 3, 4, 5, 6, 7, 8])
    ++ " " ++ show (summate [1, 2, 3, 4, 5]) )
