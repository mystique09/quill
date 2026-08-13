-- Console input: `Std.IO.readLine` yields `None` at end of input, so loops
-- over stdin terminate cleanly. Try: echo "hi" | quill examples/io.q
use Std.IO as IO
use Std.Option

echoLine :: Option String -> String
echoLine None     = "bye"
echoLine (Some l) = "you said: " ++ l

main = println (echoLine (IO.readLine ()))
