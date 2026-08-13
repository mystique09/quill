-- String utilities. Strings destructure with cons patterns (`(c:cs)`) and
-- are built back up with `snoc`; character arithmetic goes through the
-- builtins `ord` and `chr`, so this module is ordinary Quill all the way.
module Std.String
  ( toUpper, toLower, lines, words, split, join, trim
  , startsWith, endsWith, contains, fromChars, toChars, repeat ) where

use Std.List

toUpper :: String -> String
toUpper s = mapChars upperChar s

toLower :: String -> String
toLower s = mapChars lowerChar s

mapChars :: (Char -> Char) -> String -> String
mapChars f s = mapCharsGo f s ""

mapCharsGo :: (Char -> Char) -> String -> String -> String
mapCharsGo f "" acc     = acc
mapCharsGo f (c:cs) acc = mapCharsGo f cs (snoc acc (f c))

upperChar :: Char -> Char
upperChar c = if c >= 'a' && c <= 'z' then chr (ord c - 32) else c

lowerChar :: Char -> Char
lowerChar c = if c >= 'A' && c <= 'Z' then chr (ord c + 32) else c

lines :: String -> [String]
lines s = dropFinal (split '\n' s)

dropFinal :: [String] -> [String]
dropFinal []     = []
dropFinal [""]   = []
dropFinal (x:xs) = x : dropFinal xs

words :: String -> [String]
words s = filter (\w -> not (isEmpty w)) (split ' ' (squashSpaces s))

squashSpaces :: String -> String
squashSpaces s = squashGo s ""

squashGo :: String -> String -> String
squashGo "" acc     = acc
squashGo (c:cs) acc = squashGo cs (snoc acc (if isSpace c then ' ' else c))

isSpace :: Char -> Bool
isSpace c = c == ' ' || c == '\t' || c == '\n' || c == '\r'

isEmpty :: String -> Bool
isEmpty "" = True
isEmpty _  = False

split :: Char -> String -> [String]
split sep s = splitGo sep s ""

splitGo :: Char -> String -> String -> [String]
splitGo sep "" acc         = [acc]
splitGo sep (c:cs) acc
  | c == sep  = acc : splitGo sep cs ""
  | otherwise = splitGo sep cs (snoc acc c)

join :: String -> [String] -> String
join _ []       = ""
join sep (x:xs) = x ++ joinGo sep xs

joinGo :: String -> [String] -> String
joinGo _ []       = ""
joinGo sep (x:xs) = sep ++ x ++ joinGo sep xs

trim :: String -> String
trim s = trimLeft (trimRight s)

trimLeft :: String -> String
trimLeft "" = ""
trimLeft (c:cs) = if isSpace c then trimLeft cs else snoc "" c ++ cs

trimRight :: String -> String
trimRight s = reverseStr (trimLeft (reverseStr s))

startsWith :: String -> String -> Bool
startsWith "" _          = True
startsWith _ ""          = False
startsWith (c:cs) (d:ds) = c == d && startsWith cs ds

endsWith :: String -> String -> Bool
endsWith suffix s = startsWith (reverseStr suffix) (reverseStr s)

reverseStr :: String -> String
reverseStr "" = ""
reverseStr (c:cs) = snoc (reverseStr cs) c

contains :: String -> String -> Bool
contains needle hay =
  if startsWith needle hay then True
  else if isEmpty hay then False
  else contains needle (tailStr hay)

tailStr :: String -> String
tailStr "" = ""
tailStr (c:cs) = cs

fromChars :: [Char] -> String
fromChars cs = foldl (\acc c -> snoc acc c) "" cs

toChars :: String -> [Char]
toChars ""     = []
toChars (c:cs) = c : toChars cs

repeat :: String -> Int -> String
repeat s n = if n <= 0 then "" else s ++ repeat s (n - 1)
