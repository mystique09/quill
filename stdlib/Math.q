-- Numeric helpers. `sqrt`, `floor`, and `ceil` are native builtins available
-- in every program (see the prelude); everything here — including `round`,
-- which is built from `floor`/`ceil` — is ordinary Quill. `pow` and `gcd`
-- assume non-negative arguments.
module Std.Math
  ( abs, pow, min, max, clamp, even, odd, gcd, round ) where

abs :: Int -> Int
abs x
  | x >= 0    = x
  | otherwise = negate x

pow :: Int -> Int -> Int
pow _ 0 = 1
pow b n = b * pow b (n - 1)

min :: (Ord a) => a -> a -> a
min x y = if x <= y then x else y

max :: (Ord a) => a -> a -> a
max x y = if x >= y then x else y

clamp :: (Ord a) => a -> a -> a -> a
clamp lo hi x = min hi (max lo x)

even :: Int -> Bool
even n = n % 2 == 0

odd :: Int -> Bool
odd n = n % 2 /= 0

gcd :: Int -> Int -> Int
gcd a 0 = a
gcd a b = gcd b (a % b)

-- Round to the nearest Int, ties away from zero (like Rust's `f64::round`).
round :: Float -> Int
round x = if x >= 0.0 then floor (x + 0.5) else ceil (x - 0.5)
