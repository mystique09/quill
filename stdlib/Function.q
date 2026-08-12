-- Function combinators.
module Std.Function
  ( id, compose, flip, constFn ) where

id :: a -> a
id x = x

compose :: (b -> c) -> (a -> b) -> a -> c
compose f g x = f (g x)

flip :: (a -> b -> c) -> b -> a -> c
flip f x y = f y x

constFn :: a -> b -> a
constFn x _ = x
