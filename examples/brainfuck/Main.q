-- A Brainfuck interpreter written in Quill.
--
-- `parse` turns a Brainfuck source string into the command ADT, and `run`
-- executes it. The tape is a zipper: the cell under the head plus the list
-- of cells to its left (stored reversed) and to its right. Cells are bytes
-- (0..255), and output is built up as a String (`.` = chr + snoc) and
-- printed at the end.
--
-- Brainfuck's `,` (read a byte) is omitted: Quill has no stdin, so it is a
-- runtime error.

data Cmd = Inc | Dec | MoveR | MoveL | Out | Loop [Cmd]

-- Parse Brainfuck source into commands. Any other character is a comment.
parse :: [Char] -> [Cmd]
parse []       = []
parse ('>':cs) = MoveR : parse cs
parse ('<':cs) = MoveL : parse cs
parse ('+':cs) = Inc : parse cs
parse ('-':cs) = Dec : parse cs
parse ('.':cs) = Out : parse cs
parse (',':_)  = error "Brainfuck `,` needs stdin, which Quill does not have"
parse (']':_)  = error "unbalanced `]`"
parse ('[':cs) =
    case split 1 cs of
      (body, rest) -> Loop (parse body) : parse rest
parse (_:cs)   = parse cs

-- Split the inside of a `[ ... ]` at bracket depth 1: the body (without the
-- brackets) and the source that follows the matching `]`.
split :: Int -> [Char] -> ([Char], [Char])
split _ []       = ([], [])
split n ('[':cs) = case split (n + 1) cs of (b, a) -> ('[' : b, a)
split 1 (']':cs) = ([], cs)
split n (']':cs) = case split (n - 1) cs of (b, a) -> (']' : b, a)
split n (c:cs)   = case split n cs of (b, a) -> (c : b, a)

-- Run one program step: consume the command, move/update the tape, and
-- recurse. A `Loop body` repeats `body` until the head cell is zero, by
-- re-queuing `body ++ [Loop body]` ahead of the rest of the program.
run :: [Cmd] -> [Int] -> Int -> [Int] -> String -> String
run []          l c r out = out
run (Inc:cs)    l c r out = run cs l (wrap (c + 1)) r out
run (Dec:cs)    l c r out = run cs l (wrap (c - 1)) r out
run (MoveR:cs)  l c r out = run cs (c : l) (headOr0 r) (tailOr r) out
run (MoveL:cs)  l c r out = run cs (tailOr l) (headOr0 l) (c : r) out
run (Out:cs)    l c r out = run cs l c r (snoc out (chr c))
run (Loop b:cs) l c r out =
    if c == 0 then run cs l c r out
    else run (b ++ [Loop b] ++ cs) l c r out

-- Keep cell values in 0..255.
wrap :: Int -> Int
wrap n = let m = n % 256 in if m < 0 then m + 256 else m

headOr0 :: [Int] -> Int
headOr0 []    = 0
headOr0 (x:_) = x

tailOr :: [Int] -> [Int]
tailOr []     = []
tailOr (_:xs) = xs

-- Turn a string into a list of characters.
chars :: String -> [Char]
chars ""     = []
chars (c:cs) = c : chars cs

brainfuck :: String -> String
brainfuck prog = run (parse (chars prog)) [] 0 [] ""

-- "Hello World!"
hello = "++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---.+++++++..+++.>>.<-.<.+++.------.--------.>>+."

main = println (brainfuck hello)
