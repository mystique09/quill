-- Console input. `_readLine` is implemented natively (it yields `None` at
-- end of input); everything else is ordinary Quill.
module Std.IO
  ( readLine, getLines, readAll ) where

use Std.Option
use Std.List

readLine :: () -> Option String
readLine _ = _readLine ()

getLines :: () -> [String]
getLines _ = case readLine () of
    None   -> []
    Some l -> l : getLines ()

-- The whole input, one line per `\n` (a final newline is added even if the
-- input did not end with one).
readAll :: () -> String
readAll _ = foldl (\acc l -> acc ++ l ++ "\n") "" (getLines ())
