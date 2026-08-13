-- Helpers for tuple values (pairs mainly). Pure Quill: tuples are
-- destructured with patterns and built with `(a, b)`.
module Std.Tuple
  ( fst, snd, swap, curry, uncurry ) where

fst :: (a, b) -> a
fst (x, _) = x

snd :: (a, b) -> b
snd (_, y) = y

swap :: (a, b) -> (b, a)
swap (x, y) = (y, x)

curry :: ((a, b) -> c) -> a -> b -> c
curry f x y = f (x, y)

uncurry :: (a -> b -> c) -> (a, b) -> c
uncurry f (x, y) = f x y
