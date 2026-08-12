-- The Result type for fallible computations (Rust's Result).
module Std.Result
  ( Result, unwrap, mapResult, andThen ) where

data Result e a = Err e | Ok a

unwrap :: a -> Result e a -> a
unwrap default (Err _) = default
unwrap default (Ok x)  = x

mapResult :: (a -> b) -> Result e a -> Result e b
mapResult f (Err e) = Err e
mapResult f (Ok x)  = Ok (f x)

andThen :: (a -> Result e b) -> Result e a -> Result e b
andThen f (Err e) = Err e
andThen f (Ok x)  = f x
