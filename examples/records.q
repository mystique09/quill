-- Records: Rust-style structs with Haskell-style construction and access.

data Person = Person { name :: String, age :: Int }

birthday :: Person -> Person
birthday p = Person { name = p.name, age = p.age + 1 }

-- Record patterns.
greet :: Person -> String
greet (Person { name = n, age = a }) = "Hi " ++ n ++ ", you are " ++ show a

main = let ada = Person { name = "Ada", age = 36 }
       in println (greet (clone ada) ++ " -> " ++ greet (birthday (clone ada)))
