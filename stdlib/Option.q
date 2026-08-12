-- The Option type and helpers (Rust's Option, Haskell's Maybe).
module Std.Option
  ( Option, maybe, fromMaybe, isJust, isNothing, mapMaybe ) where

data Option a = None | Some a

maybe :: b -> (a -> b) -> Option a -> b
maybe default f None     = default
maybe default f (Some x) = f x

fromMaybe :: a -> Option a -> a
fromMaybe default None     = default
fromMaybe default (Some x) = x

isJust :: Option a -> Bool
isJust None     = False
isJust (Some _) = True

isNothing :: Option a -> Bool
isNothing None     = True
isNothing (Some _) = False

mapMaybe :: (a -> b) -> Option a -> Option b
mapMaybe f None     = None
mapMaybe f (Some x) = Some (f x)
