-- Multi-file programs: `use` imports another module, and `as` gives
-- qualified access to it.

use Shapes
use Shapes as S

main = println
  ( show (area (Square 3.0))
    ++ " " ++ show (S.area (S.Rectangle 2.0 3.0)) )
