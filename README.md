# Aero Shell
Ash is a super simple shell in rust
```
➜ lucbook ~/Documents/GitHub/AeroShell ➜
```
The command to start it is `ash` (or `aero`)
## Install
*Debian, Fedora and Arch is supported by automated installer*
```bash
curl -sSL https://install.nebuff.xyz/aeroshell | bash
```
## Build from source
Requires a Rust toolchain ([rustup.rs](https://rustup.rs)).
```bash
cargo build --release
./target/release/aeroshell
```
## Setting Ash as your default shell

Once installed via a package or `cargo build --release`, run:

```bash
ash --set-default
```

This registers the absolute path of the running `ash` binary in `/etc/shells`
(sudo if needed) and runs `chsh -s <path>` to make it your login shell. Log out
and back in to land in Ash

```bash
ash -c 'ls --show-hidden'
```

To go back:

```bash
chsh -s /bin/bash   # or any shell in /etc/shells
```

## Usage

Built-in commands (also listed by `help`):

| Command | What it does |
|---|---|
| `ls [--show-hidden] [dir]` | List files in columns; dotfiles hidden unless `--show-hidden` |
| `cd [dir]` | Change directory (`-` = previous, `~` = home) |
| `pwd` | Print working directory |
| `history [n]` | Show recent commands (last n); Ctrl-R searches them interactively |
| `export VAR=val` | Set a session environment variable |
| `unset VAR` | Remove a session environment variable |
| `alias <subcommand>` | Manage aliases: `create <alias> <command…>`, `ls`, `rm <alias>` |
| `type <cmd>` / `which <cmd>` | Show how a command resolves |
| `aepkg <subcommand>` | Manage packages (`list`, `install`, `remove`, `reload`, …) |
| `ver` | Show Ash version |
| `help` | Command list |
| `exit` / `quit` | Exit |

CLI: `ash -c '<cmd>'`, `ash <file> [args…]` (a non-option argument is read
as a script), `ash -l` (login: source `/etc/profile` and `~/.profile` first),
`ash --help`, `ash --version`, `ash --set-default`.

One-liners work like a POSIX shell: parameter and command substitution
(`$VAR`, `${VAR}`, `$(cmd)`, `` `cmd` ``), pipelines (`|`), sequencing (`&&`,
`||`, `;`) and redirection (`<`, `>`, `>>`, `<<`, `2>`, `2>>`, `2>&1`, `&>`)
all apply per command, and builtins or package commands can be pipeline
stages. Heredoc bodies are expanded unless the delimiter is quoted:

```bash
cargo build && ./target/release/aeroshell --help
ls -la > /tmp/out.txt && cat < /tmp/out.txt
false && echo skipped || echo ran
history 20 | tail
cat <<EOF
heredocs expand $HOME and $(uname -s)
EOF
cat <<'EOF'
but this one stays literal: $HOME
EOF
```

### History search
`Ctrl-R` opens a searchable, navigable history picker. It starts with whatever
is already on the prompt, so `git ch` followed by `Ctrl-R` jumps straight to
the matching commands:
```
 Ash history search                                   3/48
> git ch
────────────────────────────────────────────────────────
> git checkout main
  git checkout -b feature
  git cherry-pick HEAD~1
  1/3   ↑↓ move · enter edit · esc cancel
```
## Configuration

Everything lives in `~/.aeroshell/`:

- `config.json` — settings, colors, aliases
- `history.txt` — persistent command history (up/down arrows recall, Ctrl-R searches)

Handy session variables (`export`) survive per session; aliases are persistent
in `config.json`, e.g

```bash
alias create ll ls -la
alias create gs git status
alias ls
# gs -> git status
alias rm ll
```

The prompt template is editable in `config.json` (`prompt_template`), with
placeholders `{username} {hostname} {pwd} {short_pwd} {time} {date} {battery}
{git}` and any color name:

```
"<magenta>➜</magenta> <cyan>{short_pwd}</cyan> <blue>{git}</blue> <green>➜</green> "
```

Standard ANSI names (`red green blue yellow magenta cyan white bold italic
underline dim highlight`, plus extras like `orange purple teal`) all work in
the template.

## Packages

Ash packages add Lua commands, completions, colors, aliases, prompt pieces and
hooks. Install one from a local directory and use its commands right away:

```bash
aepkg install examples/packages/hello
aepkg install /user/repo                         # GitHub shorthand
aepkg install https://github.com/user/repo        # or a .tar.gz / .zip link
hello Neo --loud
aepkg list
```

See [docs/packages.md](docs/packages.md) for the manifest format and Lua API.

## Data directories

AeroShell keeps all its state in `~/.aeroshell/`:

- `config.json` — settings, colors, aliases
- `history.txt` — command history
- `packages/` — installed package trees and `index.json`

Delete the directory to reset Ash to defaults.

## Contributing

Issues and PRs welcome, target the `main` branch. Keep changes small and
matching the existing style, `cargo build` with no warnings
Contributing to issues are also welcome, Take issues that **do not** have a assignee already
```bash
git clone https://github.com/Aero-Shell/Shell.git AeroShell
cd AeroShell
cargo build
cargo run
```

Code layout:

- `src/main.rs` — REPL, argument parsing, prompt, login-shell handling
- `src/shell/` — the shell itself: `lexer.rs` and `parser.rs` (tokens, AST),
  `expand.rs` (parameters, command substitution, arithmetic, splitting, globs),
  `exec.rs` (control flow, pipelines, redirections, `exec`), `special.rs`
  (POSIX special builtins), `mod.rs` (shell state)
- `tests/cli.rs` — end-to-end tests that run the real binary: `-c`, script
  files and `-l`
- `src/history_search.rs` — searchable history picker (Ctrl-R / Up)
- `src/commands/builtins.rs` — built-in commands
- `src/commands/aepkg.rs` — package manager command
- `src/config.rs` — config, themes, prompt rendering
- `src/packages/` — manifests, installed index, package loading, Lua bridge
- `src/registry.rs` — command registry

## License

[MIT](LICENSE)
