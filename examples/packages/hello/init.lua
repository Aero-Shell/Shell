-- Runs once when the package loads. Packages can register hooks here, tweak
-- colors, or contribute prompt pieces.
prompt.add_piece("hello_username", "hello " .. (ash.config().username or "user"))
