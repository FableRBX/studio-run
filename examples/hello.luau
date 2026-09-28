-- studio-run --script examples/hello.luau -- one two
print("Hello from", game.Name)
print("Arguments:", ...)

warn("Warnings show up in yellow")

local descendants = #game:GetDescendants()
print(`The place has {descendants} descendants`)
