-- The newer standard library modules, written entirely in Quill:
-- `Std.String` (character work via the `chr`/`ord`/`snoc` builtins),
-- `Std.Tuple`, and `Std.Math`.

use Std.String as S
use Std.Tuple as T
use Std.Math as M

main = println
  ( S.toUpper "hElLo"
    ++ "\n" ++ S.join ", " (S.words "one  two\tthree")
    ++ "\n" ++ show (S.split ',' "a,,b,c")
    ++ "\n" ++ S.trim "  padded  "
    ++ "\n" ++ show (S.startsWith "he" "hello")
    ++ "\n" ++ show (S.endsWith "lo" "hello")
    ++ "\n" ++ show (S.contains "ell" "hello")
    ++ "\n" ++ S.fromChars ['a', 'b', 'c']
    ++ "\n" ++ show (T.swap (1, 2))
    ++ "\n" ++ show (T.curry (\(x, y) -> x + y) 20 22)
    ++ "\n" ++ show (M.pow 2 8)
    ++ "\n" ++ show (M.gcd 12 18)
    ++ "\n" ++ show (M.round 2.5)
    ++ "\n" ++ show (ord 'A') )
