-- A user-defined module: geometric shapes.
module Shapes (Shape, Circle, Square, Rectangle, area) where

data Shape = Circle Float | Square Float | Rectangle Float Float

area :: Shape -> Float
area (Circle r)      = 3.14159 * r * r
area (Square s)      = s * s
area (Rectangle w h) = w * h
