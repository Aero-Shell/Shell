# Ash packages

Packages extend Ash with Lua commands, completions, colors, aliases, prompt
contributions and hooks. A package is a directory containing `ash-pkg.toml`:

```
hello/
├── ash-pkg.toml
├── init.lua
├── commands/
│   └── hello.lua
└── completions/
    └── hello.lua
```

## Managing packages

```bash
aepkg install ./hello       # local directory (or local .tar.gz/.zip)
aepkg install /user/repo    # GitHub shorthand
aepkg install https://github.com/user/repo
aepkg install /user/repo/tree/main/packages/hello
aepkg install https://github.com/user/repo/tree/main/packages/hello
aepkg install https://gitlab.com/user/repo.git#packages/hello
aepkg install https://example.com/hello.tar.gz
aepkg list                  # installed packages and their commands
aepkg info hello            # manifest details and contributions
aepkg disable hello         # keep installed, stop loading it
aepkg enable hello
aepkg remove hello
aepkg reload                # reload enabled packages (after editing sources)
```

Remote sources:

- **GitHub shorthand** `/owner/repo` (and `/owner/repo/tree/<ref>/<path>`) is
  resolved to `https://github.com/owner/repo`. An existing local path always
  wins, so absolute paths still install from disk.
- **Git URLs** (`https://…`, `git@…`, `ssh://…`, `file://…`) are cloned with
  `git clone --depth 1`. Requires `git` on `PATH`.
- **GitHub tree URLs** (`/tree/<branch>/<path>`) and a `#<path>` suffix install a
  package from a subdirectory of a repository.
- **Archives** (`.tar.gz`, `.tgz`, `.tar`, `.zip`, and GitHub `/archive/` URLs)
  are downloaded and extracted. Downloads are capped at 64 MiB; extraction at
  256 MiB. Symlinks and special files in archives are skipped.

When the source has no manifest at its root, Ash looks for exactly one
`ash-pkg.toml` below it. If several are found, use a subdirectory URL or
`#<path>`. Git metadata and symlinks are not copied into the package store.

Installed packages live in `~/.aeroshell/packages/` (`store/` plus `index.json`).
Manifest contributions are applied to the in-memory config at load time;
installing, enabling or disabling a package does not rewrite `config.json`.
Values written explicitly by the Lua API (`colors.set`, `prompt.set_template`)
or by the user do persist.

Command names may not replace built-ins (`ls`, `cd`, `aepkg`, …). Higher
`priority` packages load later and win when two packages define the same
command.

## Manifest

```toml
name = "hello"                 # letters, digits, '-', '_', '.' only
version = "1.0.0"
language = "lua"               # only Lua packages are supported
description = "Greeting command"
priority = 10                  # optional, default 0

[[commands]]
name = "hello"                 # command shown in the shell
entry = "commands/hello.lua"

[[completions]]
command = "hello"
definition = "completions/hello.lua"

[init]
entrypoint = "init.lua"        # optional, runs once at load

[contributions.aliases.aliases]
hi = "hello"

[contributions.colors.colors]
accent = "\u001b[35m"

[contributions.config.settings]
username = "Aero-User"         # color, username, time_format,
                               # active_theme, prompt_template

[contributions.prompt]
template = "<green>➜</green> <blue>{pwd}</blue> {{hello_user}} "

[[contributions.prompt.pieces]]
name = "hello_user"
template = "hello {username}"
```

Every entry path is relative to the package root; absolute paths and `..` are
rejected.

## Command entries

A command entry is Lua that runs at load time and must return a function (or
define a global `run`). It is called with one table: `cmd` holds the command
name, and the numeric part is `argv`:

```lua
return function(args)
    local name = args[1] or "world"
    print("Hello, " .. name .. "!")
    return 0  -- exit code; nil counts as 0
end
```

Command entries are sandboxed in their own environment but can still read the
API globals below. Package commands are built-ins, so they cannot be used as a
stage in a pipeline yet.

## Lua API

| API | Description |
|---|---|
| `ash.version` | Running Ash version |
| `ash.config()` | Table of the active config (`username`, `colors`, …) |
| `ash.theme(name)` | Set and persist the active theme name |
| `ash.dir()` | Root of the package being loaded/executed |
| `ash.package_path(sub)` | Package file path during load/execution |
| `os.execute(cmd)` | Run a shell command, returns its exit code |
| `os.getenv/setenv/removeenv` | Session environment |
| `os.read_file/write_file/read_dir` | File helpers |
| `colors.get(name)` | ANSI code for a color |
| `colors.set(name, code)` | Set and persist a color |
| `colors.reset()` | Restore default colors |
| `prompt.set_template(template)` | Set and persist the prompt template |
| `prompt.color(name, code)` | Alias for `colors.set` |
| `prompt.add_piece(name, template)` | Register `{{name}}` for prompt templates |
| `completion.add(command, { … })` | Argument candidates for Tab completion and inline preview |
| `env.get/set/unset` | Session environment |
| `hooks.register(name, fn)` | Register a hook (see below) |

Registered candidates are also previewed inline in the dim suggestion color
while you type the argument: the rest of the first matching candidate is shown
and can be accepted with `→`, `End`, or `Ctrl-F`. Tab completion still lists
every match.
Only the in-memory candidate list is consulted, so typing never triggers
package code or filesystem work.

Prompt placeholders are shared with the shell: `{username} {hostname} {pwd}
{short_pwd} {time} {date} {battery} {git}`, plus `<color>…</color>` tags. Pieces
are expanded once as `{{name}}`.

### Hooks

- `prompt` — runs before each prompt, no arguments.
- `command` — runs before a command; receives `{ cmd, [1] = arg, … }`.
- `command_done` — runs after a command; same table plus `status`.

Hooks run in the main thread and may use the whole API. A failing hook prints a
warning and does not stop the command.

## Example

A working package is in [`examples/packages/hello`](../examples/packages/hello):

```bash
aepkg install examples/packages/hello
hello Neo --loud
hi
```
