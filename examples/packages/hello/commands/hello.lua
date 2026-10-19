-- Command entries return a function (or define a global `run`). Arguments are a
-- table with `cmd` plus the numeric argv sequence; the return value is the exit
-- code.
return function(args)
    local name = args[1] or "world"
    local loud = false

    for _, arg in ipairs(args) do
        if arg == "--loud" then
            loud = true
        end
    end

    local message = "Hello, " .. name .. "!"
    if loud then
        message = string.upper(message)
    end
    print(message)

    if #args == 0 then
        print("usage: hello [name] [--loud]")
    end
    return 0
end
