# Aero Shell

Ash is a super fast unix shell written in Rust

```
➜ lucbook ~/Documents/GitHub/AeroShell ➜
```

The command to start it is `ash` (or `aero`)

## Install

One-liner (Fedora/RHEL, Debian/Ubuntu, Arch/SteamOS are autodetected):

```bash
curl -sSL https://install.nebuff.xyz/ | bash
```

Or with a package manager directly:

```bash
# Fedora / RHEL
sudo dnf install https://repo.nebuff.xyz/rpm/newest/aero-1.0.2-1.fc44.x86_64.rpm   # or via repo file:
curl -sSL https://repo.nebuff.xyz/aero.repo | sudo tee /etc/yum.repos.d/aero.repo
sudo dnf install aero

# Debian / Ubuntu
curl -sSL https://repo.nebuff.xyz/aero.asc | sudo tee /usr/share/keyrings/aero.asc
curl -sSL https://repo.nebuff.xyz/aero.sources | sudo tee /etc/apt/sources.list.d/aero.sources
sudo apt update && sudo apt install aero

# Arch / SteamOS
curl -sSL https://repo.nebuff.xyz/aero.pacman.conf.snippet | sudo tee -a /etc/pacman.conf
sudo pacman -Sy aero
```

Packages install `/usr/bin/ash` and a `/usr/bin/aero` symlink

No package manager? (containers, older devices):

```bash
curl -sSL https://repo.nebuff.xyz/releases/aero-1.0.2-linux-x86_64.tar.gz -o /tmp/aero.tgz
mkdir -p ~/.local/bin && tar -xzf /tmp/aero.tgz -C ~/.local/bin --strip-components=2
~/.local/bin/ash   # add ~/.local/bin to PATH if needed; 'aero' also works
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
| `history [n]` | Show recent commands (last n) |
| `export VAR=val` | Set a session environment variable |
| `unset VAR` | Remove a session environment variable |
| `alias create <alias> <command…>` | Create a persistent alias |
| `alias ls` | List aliases |
| `alias rm <alias>` | Remove an alias |
| `type <cmd>` / `which <cmd>` | Show how a command resolves |
| `ver` | Show Ash version |
| `help` | Command list |
| `exit` / `quit` | Exit |

CLI flags: `ash --help`, `ash --version`, `ash --set-default`, `ash -c '<cmd>'`.

## Configuration

Everything lives in `~/.aeroshell/`:

- `config.json` — settings, colors, aliases
- `history.txt` — persistent command history (up/down arrows recall)

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

## Data directories

AeroShell keeps all its state in `~/.aeroshell/`:

- `config.json` — settings, colors, aliases
- `history.txt` — command history

Delete the directory to reset Ash to defaults.

## Contributing

Issues and PRs welcome, target the `main` branch. Keep changes small and
matching the existing style, `cargo build` with no warnings

```bash
git clone https://github.com/Aero-Shell/Shell.git AeroShell
cd AeroShell
cargo build
cargo run
```

Code layout:

- `src/main.rs` — REPL, arg parsing, pipelines, login-shell handling
- `src/commands/builtins.rs` — built-in commands
- `src/config.rs` — config, themes, prompt rendering
- `src/registry.rs` — command registry

## License

[MIT](LICENSE)
