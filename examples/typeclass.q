-- Type classes are Quill's traits.  `class` declares a trait; `instance`
-- implements it for a type.  Method dispatch happens on the runtime type.

data Shape = Circle Float | Square Float | Rectangle Float Float

class Area a where
  area :: a -> Float

instance Area Shape where
  area (Circle r)            = 3.14159 * r * r
  area (Square s)            = s * s
  area (Rectangle w h)       = w * h

class Describe a where
  describe :: a -> String

instance Describe Shape where
  describe (Circle r)      = "a circle of radius " ++ show r
  describe (Square s)      = "a square of side " ++ show s
  describe (Rectangle w h) = "a " ++ show w ++ "x" ++ show h ++ " rectangle"

report :: Shape -> String
report s = describe (clone s) ++ " with area " ++ show (area (clone s))

main = println (report (Circle 2.0) ++ "; " ++ report (Rectangle 3.0 4.0))
