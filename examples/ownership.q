-- Rust-style ownership: values move by default, `clone` duplicates,
-- `&` borrows, and mutation requires `mut`.

main = let greeting = "hello"
           copied   = clone greeting   -- explicit duplication
           borrowed = &greeting        -- immutable borrow (no move)
       in println (copied ++ " " ++ *borrowed)

counter = let mut n = 0
              bump x = n := n + x
          in let _ = bump 3 in let _ = bump 4 in n

report = println ("counter = " ++ show counter)
