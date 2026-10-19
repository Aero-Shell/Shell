use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use crate::config;
use crate::registry;

// Previous directory for `cd -`.
static PREVIOUS_DIR: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));

// ls [--show-hidden] [dir]
pub fn ls(args: &[&str]) -> Result<()> {
    let mut show_hidden = false;
    let mut path: &str = ".";
    for arg in args {
        if *arg == "--show-hidden" || *arg == "-a" || *arg == "--all" {
            show_hidden = true;
        } else if path == "." && !arg.starts_with('-') {
            path = arg;
        } else if !arg.starts_with('-') {
            path = arg;
        } else {
            registry::print_colored(&format!("ls: unknown option: {arg}"), "error");
            return Ok(());
        }
    }
    let iter = std::fs::read_dir(path).with_context(|| format!("ls: cannot read {path}"))?;
    let mut items: Vec<(String, String, String, bool)> = Vec::new();
    for entry in iter.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let full = entry.path();
        let is_dir = entry
            .file_type()
            .map(|ft| ft.is_dir())
            .unwrap_or_else(|_| full.is_dir());
        let is_exec = full
            .metadata()
            .map(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && (m.permissions().mode() & 0o111 != 0)
            })
            .unwrap_or(false);
        let is_script = name.ends_with(".sh") || is_exec;
        let key = if is_dir {
            "ls_dir"
        } else if is_script {
            "ls_script"
        } else if name.starts_with('.') {
            "ls_hidden"
        } else {
            "ls_file"
        };
        // Quote names with spaces so they survive re-tokenizing.
        let displayed = if name.contains([' ', '"', '\'', '\\']) {
            shlex::try_quote(&name)
                .map(|q| q.into_owned())
                .unwrap_or(name.clone())
        } else {
            name.clone()
        };
        items.push((name.clone(), displayed, key.to_string(), is_dir));
    }
    // Case-insensitive sort on the raw name, not the quoted display.
    items.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    if items.is_empty() {
        return Ok(());
    }

    let max_len = items.iter().map(|(_, n, _, _)| n.len()).max().unwrap_or(0);
    let col_width = max_len + 2;
    let term_width = terminal_width().max(col_width).max(20);
    let num_cols = (term_width / col_width).max(1);
    let num_rows = (items.len() + num_cols - 1) / num_cols;

    // Column-major order, like `ls`.
    for row in 0..num_rows {
        let mut line = String::new();
        for col in 0..num_cols {
            let idx = col * num_rows + row;
            if idx >= items.len() {
                continue;
            }
            let (_, name, key, _) = &items[idx];
            let is_last_col = col == num_cols - 1 || (col + 1) * num_rows + row >= items.len();
            let padded = if is_last_col {
                name.clone()
            } else {
                format!("{name:<col_width$}")
            };
            line.push_str(&crate::config::colorize(&padded, key));
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}

fn terminal_width() -> usize {
    if let Ok(cols) = std::env::var("COLUMNS")
        && let Ok(n) = cols.parse::<usize>()
        && n > 0
    {
        return n;
    }
    #[cfg(unix)]
    {
        let mut ws = unsafe { std::mem::zeroed::<libc::winsize>() };
        if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    80
}

// cd [dir] — no args = home, '-' = previous, ~/x = under home.
pub fn cd(args: &[&str]) -> Result<()> {
    if args.len() > 1 {
        registry::print_colored("cd: too many arguments", "error");
        return Ok(());
    }

    if args.is_empty() {
        let home = dirs::home_dir().with_context(|| "cd: cannot resolve home")?;
        switch_dir(&home, false)?;
        return Ok(());
    }

    if args[0] == "-" {
        let prev = PREVIOUS_DIR.lock().expect("prev dir lock poisoned").clone();
        match prev {
            Some(dir) => switch_dir(&dir, true)?,
            None => registry::print_colored("cd: no previous directory", "warning"),
        }
        return Ok(());
    }

    switch_dir(&expand_tilde(args[0]), false)
}

fn switch_dir(target: &Path, print_target: bool) -> Result<()> {
    if !target.is_dir() {
        registry::print_colored(
            &format!("cd: no such directory: {}", target.display()),
            "error",
        );
        return Ok(());
    }
    let previous = std::env::current_dir().ok();
    match std::env::set_current_dir(target) {
        Ok(()) => {
            *PREVIOUS_DIR.lock().expect("prev dir lock poisoned") = previous;
            if print_target {
                registry::print_colored(&target.to_string_lossy(), "data_value");
            }
            Ok(())
        }
        Err(e) => {
            registry::print_colored(&format!("cd: {}: {e}", target.display()), "error");
            Ok(())
        }
    }
}

// mkdir <dir>
pub fn mkdir(args: &[&str]) -> Result<()> {
    if args.is_empty() {
        registry::print_colored("mkdir: missing operand", "error");
        return Ok(());
    }
    std::fs::create_dir_all(args[0]).with_context(|| format!("mkdir: {}", args[0]))?;
    registry::print_colored(&format!("Created directory {}", args[0]), "success");
    Ok(())
}

pub fn pwd(_args: &[&str]) -> Result<()> {
    let cwd = std::env::current_dir().with_context(|| "pwd: cannot get cwd")?;
    registry::print_colored(&cwd.to_string_lossy(), "data_value");
    Ok(())
}

pub fn exit(_args: &[&str]) -> Result<()> {
    registry::print_colored("Exiting Ash...", "warning");
    std::process::exit(0);
}

pub fn clear(_args: &[&str]) -> Result<()> {
    print!("\x1b[2J\x1b[H");
    Ok(())
}

pub fn ver(_args: &[&str]) -> Result<()> {
    registry::print_colored(&format!("Ash Version: {}", config::aero_version()), "info");
    Ok(())
}

pub fn help(_args: &[&str]) -> Result<()> {
    registry::print_colored("Ash Help - Available Commands:", "header");

    let help_text = [
        (
            "ls [--show-hidden] [dir]",
            "List files in columns (dotfiles hidden unless --show-hidden)",
        ),
        ("cd [dir]", "Change directory ('-' = previous, '~' = home)"),
        ("pwd", "Print working directory"),
        ("history [n]", "Show recent commands (last n)"),
        ("export VAR=val", "Set a session environment variable"),
        ("unset VAR", "Remove a session environment variable"),
        (
            "alias create <alias> <command…>",
            "Create a persistent command alias",
        ),
        ("alias ls", "List aliases"),
        ("alias rm <alias>", "Remove an alias"),
        ("type <cmd>", "Show how a command resolves (also: which)"),
        (
            "aepkg <subcommand>",
            "Manage packages (list, install, remove, reload)",
        ),
        ("ver", "Show Ash version"),
        ("exit / quit", "Exit Ash"),
    ];

    for (cmd, desc) in help_text {
        println!("  {cmd:<34} - {desc}");
    }

    Ok(())
}

// history [n] — show recent commands from the persistent history file.
pub fn history(args: &[&str]) -> Result<()> {
    let path = config::history_path()?;
    // `#` lines in rustyline history files are metadata; treat as comments.
    let entries: Vec<String> = match std::fs::read_to_string(&path) {
        Ok(text) => text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
        Err(_) => {
            registry::print_colored("history: empty (no history saved yet)", "warning");
            return Ok(());
        }
    };

    let count = match args.first() {
        Some(n) => match n.parse::<usize>() {
            Ok(m) if m > 0 => m,
            _ => {
                registry::print_colored("history: argument must be a positive number", "error");
                return Ok(());
            }
        },
        None => entries.len(),
    };

    for (i, entry) in entries.iter().enumerate() {
        let n = i + 1;
        if n > entries.len().saturating_sub(count) {
            println!("{n:>5}  {entry}");
        }
    }
    Ok(())
}

// export VAR=value; with no `=`, prints the current value.
pub fn export(args: &[&str]) -> Result<()> {
    for arg in args {
        match arg.split_once('=') {
            Some((name, _value)) if !is_valid_name(name) => {
                registry::print_colored(&format!("export: invalid name: {name}"), "error");
            }
            Some((name, value)) => {
                unsafe { std::env::set_var(name, value) };
            }
            None => match std::env::var(arg) {
                Ok(value) => println!("{arg}={value}"),
                Err(_) => registry::print_colored(&format!("export: not set: {arg}"), "warning"),
            },
        }
    }
    Ok(())
}

// unset VAR
pub fn unset(args: &[&str]) -> Result<()> {
    for name in args {
        if !is_valid_name(name) {
            registry::print_colored(&format!("unset: invalid name: {name}"), "error");
            continue;
        }
        unsafe { std::env::remove_var(name) };
    }
    Ok(())
}

// alias create <alias> <command…>, alias ls, alias rm <alias>.
// Aliases live in config.json so they survive restarts.
pub fn alias(args: &[&str]) -> Result<()> {
    match args {
        ["ls"] => {
            let mut aliases: Vec<_> = registry::active_config()?.aliases.into_iter().collect();
            aliases.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, command) in aliases {
                println!("{name} -> {command}");
            }
        }
        ["create", name, command] => {
            if !is_valid_name(name) {
                registry::print_colored(&format!("alias: invalid name: {name}"), "error");
                return Ok(());
            }
            // Keep the command's literal spaces intact: the argument is one
            // single string (e.g. `alias create aerotest 'bash /home/.../test.sh'`),
            // so parse it back to its real tokens before quoting.
            // `shlex::split` returns `&[&str]`; convert to owned `String`
            // so `try_quote` can borrow it and we keep the owned output.
            // `shlex::split` returns `Option<Vec<String>>` of owned `String`
            // tokens, so we don't need to re-encode the separator ourselves.
            let command = shlex::split(command)
                .unwrap_or_default()
                .into_iter()
                .map(|part| {
                    shlex::try_quote(&part)
                        .map(|quoted| quoted.into_owned())
                        .unwrap_or_else(|_| part)
                })
                .collect::<Vec<_>>()
                .join(" ");
            modify_config(|cfg| {
                cfg.aliases.insert((*name).into(), command.clone());
            })?;
            registry::print_colored(&format!("Created alias {name} -> {command}"), "success");
        }
        ["rm", name] => {
            let exists = registry::active_config()?.aliases.contains_key(*name);
            if exists {
                modify_config(|cfg| {
                    cfg.aliases.remove(*name);
                })?;
                registry::print_colored(&format!("Removed alias {name}"), "success");
            } else {
                registry::print_colored(&format!("alias: not found: {name}"), "warning");
            }
        }
        ["rm", ..] => {
            registry::print_colored("Usage: alias rm <alias>", "warning");
        }
        ["create", ..] => {
            registry::print_colored(
                "Usage: alias create <alias> <command to execute>",
                "warning",
            );
        }
        [] => {
            registry::print_colored(
                "Usage: alias create <alias> <command>, alias ls, alias rm <alias>",
                "info",
            );
        }
        [subcommand, ..] => {
            registry::print_colored(&format!("alias: unknown subcommand: {subcommand}"), "error");
        }
    }
    Ok(())
}

// type / which <cmd> — builtin vs PATH binary.
pub fn type_cmd(args: &[&str]) -> Result<()> {
    if args.is_empty() {
        registry::print_colored("type: missing command name", "warning");
        return Ok(());
    }
    for name in args {
        if registry::resolve(name).is_some() {
            println!("{name} is a shell builtin");
        } else if let Some(path) = which_cmd(name) {
            println!("{name} is {path}");
        } else {
            registry::print_colored(&format!("type: no such command: {name}"), "error");
        }
    }
    Ok(())
}

// Resolve a command name to a PATH binary.
pub fn which_cmd(name: &str) -> Option<String> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        if path.is_file() {
            return Some(path.to_string_lossy().into_owned());
        }
        return None;
    }
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let candidate = PathBuf::from(dir).join(name);
        if candidate.is_file() && candidate.exists() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

// Tab completion of the first word: built-ins + PATH executables.
pub fn command_candidates(word: &str) -> Vec<String> {
    if !word.is_empty() && word.contains('/') {
        return path_candidates(word);
    }

    let mut candidates: Vec<String> = Vec::new();
    candidates.extend(registry::names());
    candidates.extend(executable_names());
    candidates.retain(|name| name.starts_with(word));
    candidates.sort();
    candidates.dedup();
    candidates
}

// Tab completion of file paths.
pub fn path_candidates(word: &str) -> Vec<String> {
    let (dir_text, prefix) = match word.rfind('/') {
        Some(i) => (word[..=i].to_string(), word[i + 1..].to_string()),
        None => (String::new(), word.to_string()),
    };

    let search_dir = if dir_text.is_empty() {
        PathBuf::from(".")
    } else {
        expand_tilde(&dir_text)
    };

    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&search_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) {
                continue;
            }
            let suffix = if entry.path().is_dir() { "/" } else { "" };
            candidates.push(quote_if_needed(&format!("{dir_text}{name}{suffix}")));
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn executable_names() -> Vec<String> {
    let mut names = Vec::new();
    let Ok(path) = std::env::var("PATH") else {
        return names;
    };
    let mut seen = std::collections::HashSet::new();
    for dir in path.split(':') {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let is_exec = entry
                .metadata()
                .map(|m| {
                    use std::os::unix::fs::PermissionsExt;
                    m.is_file() && (m.permissions().mode() & 0o111 != 0)
                })
                .unwrap_or(false);
            if !is_exec {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if seen.insert(name.clone()) {
                names.push(name);
            }
        }
    }
    names
}

// Quote a completion candidate if it would break tokenization.
fn quote_if_needed(candidate: &str) -> String {
    if candidate.contains([' ', '"', '\'', '\\']) {
        shlex::try_quote(candidate)
            .map(|q| q.into_owned())
            .unwrap_or_else(|_| candidate.to_string())
    } else {
        candidate.to_string()
    }
}

// Expand a leading `~` or `~/…` to the home directory.
fn expand_tilde(word: &str) -> PathBuf {
    if word == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    if let Some(rest) = word.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(word)
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// Apply a config change and persist it.
fn modify_config(f: impl FnOnce(&mut config::Config)) -> anyhow::Result<()> {
    let mut cfg = registry::active_config()?;
    f(&mut cfg);
    config::save_config(&cfg)?;
    config::set_config(cfg);
    Ok(())
}
