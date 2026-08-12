-- The standard library, written entirely in Quill.  `Std.List` is imported
-- implicitly; the others are imported with `use`.

use Std.Option
use Std.Result as R
use Std.Function as F

even :: Int -> Bool
even n = n % 2 == 0

main = let squares   = map (\x -> x * x) [1, 2, 3, 4]
           evens     = filter even [1, 2, 3, 4, 5, 6]
           total     = foldl (\acc x -> acc + x) 0 squares
           maybeAge  = fromMaybe 0 (Some 36)
           ok        = R.unwrap 0 (R.Ok 41)
           err       = R.unwrap 0 (R.Err "boom")
           incremented = F.compose (\x -> x + 1) (\x -> x * 10) 4
       in println
            ( show squares ++ " " ++ show evens ++ " " ++ show total
              ++ " " ++ show maybeAge ++ " " ++ show ok ++ " " ++ show err
              ++ " " ++ show incremented )
