-- Standard list utilities. This module is the implicit prelude: its exports
-- are available unqualified in every program.
module Std.List
  ( reverse, sum, product, take, drop, elem, zip, concat
  , replicate, range, maximum, minimum, all, any
  , map, filter, foldl, foldr, head, tail, length ) where

map :: (a -> b) -> [a] -> [b]
map f []     = []
map f (x:xs) = f x : map f xs

filter :: (a -> Bool) -> [a] -> [a]
filter p []     = []
filter p (x:xs) = if p x then x : filter p xs else filter p xs

foldl :: (b -> a -> b) -> b -> [a] -> b
foldl f z []     = z
foldl f z (x:xs) = foldl f (f z x) xs

foldr :: (a -> b -> b) -> b -> [a] -> b
foldr f z []     = z
foldr f z (x:xs) = f x (foldr f z xs)

head :: [a] -> a
head (x:_) = x
head []    = error "head: empty list"

tail :: [a] -> [a]
tail (_:xs) = xs
tail []     = error "tail: empty list"

length :: [a] -> Int
length []     = 0
length (_:xs) = 1 + length xs

reverse :: [a] -> [a]
reverse xs = foldl (\acc x -> x : acc) [] xs

sum :: [Int] -> Int
sum xs = foldl (\acc x -> acc + x) 0 xs

product :: [Int] -> Int
product xs = foldl (\acc x -> acc * x) 1 xs

take :: Int -> [a] -> [a]
take n xs = takeFrom n xs

takeFrom :: Int -> [a] -> [a]
takeFrom 0 _      = []
takeFrom _ []     = []
takeFrom n (x:xs) = x : takeFrom (n - 1) xs

drop :: Int -> [a] -> [a]
drop n xs = dropFrom n xs

dropFrom :: Int -> [a] -> [a]
dropFrom 0 xs     = xs
dropFrom _ []     = []
dropFrom n (_:xs) = dropFrom (n - 1) xs

elem :: (Eq a) => a -> [a] -> Bool
elem x xs = foldl (\acc y -> acc || (x == y)) False xs

zip :: [a] -> [b] -> [(a, b)]
zip xs ys = zipGo xs ys

zipGo :: [a] -> [b] -> [(a, b)]
zipGo [] _          = []
zipGo _ []          = []
zipGo (x:xs) (y:ys) = (x, y) : zipGo xs ys

concat :: [[a]] -> [a]
concat xs = foldl (\acc x -> acc ++ x) [] xs

replicate :: Int -> a -> [a]
replicate n x = replicateGo n x

replicateGo :: Int -> a -> [a]
replicateGo 0 _ = []
replicateGo n x = x : replicateGo (n - 1) x

range :: Int -> Int -> [Int]
range a b = rangeGo a b

rangeGo :: Int -> Int -> [Int]
rangeGo a b = if a > b then [] else a : rangeGo (a + 1) b

maximum :: (Ord a) => [a] -> a
maximum (x:xs) = foldl (\acc y -> if y > acc then y else acc) x xs
maximum []     = error "maximum: empty list"

minimum :: (Ord a) => [a] -> a
minimum (x:xs) = foldl (\acc y -> if y < acc then y else acc) x xs
minimum []     = error "minimum: empty list"

all :: (a -> Bool) -> [a] -> Bool
all p xs = foldl (\acc x -> acc && p x) True xs

any :: (a -> Bool) -> [a] -> Bool
any p xs = foldl (\acc x -> acc || p x) False xs
