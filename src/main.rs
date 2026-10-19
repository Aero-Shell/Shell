use anyhow::Result;
use rustyline::highlight::Highlighter;
use rustyline::history::DefaultHistory;
use rustyline::{CompletionType, Config as RLConfig, Editor};
use shlex::split;
use std::borrow::Cow;
use std::io::{IsTerminal, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};

use aeroshell::{config, packages, registry};

mod commands;

use commands::aepkg as aepkg_cmd;
use commands::builtins;

// While a foreground external command runs, the terminal delivers Ctrl-C
// (SIGINT) to every process in the foreground process group — including the
// shell itself. With no disposition, that signal kills Ash outright
// ("Warning: Program '...aero' crashed."), so ignore the terminal signals for
// the duration of the command and restore the previous behavior on drop.
#[cfg(unix)]
struct InterruptGuard {
    previous: Vec<(i32, libc::sigaction)>,
}

#[cfg(unix)]
impl InterruptGuard {
    fn acquire() -> Self {
        let mut previous = Vec::new();
        for sig in [libc::SIGINT, libc::SIGQUIT] {
            unsafe {
                let mut ignore: libc::sigaction = std::mem::zeroed();
                ignore.sa_sigaction = libc::SIG_IGN;
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, &ignore, &mut old) == 0 {
                    previous.push((sig, old));
                }
            }
        }
        Self { previous }
    }
}

#[cfg(unix)]
impl Drop for InterruptGuard {
    fn drop(&mut self) {
        for (sig, old) in self.previous.drain(..) {
            unsafe {
                libc::sigaction(sig, &old, std::ptr::null_mut());
            }
        }
    }
}

// Restore the default terminal-signal dispositions in the child before exec:
// the ignore flag the shell installed would otherwise leak across exec and
// leave the child unable to die from Ctrl-C (e.g. ssh or ping wouldn't stop).
#[cfg(unix)]
unsafe fn reset_child_signals() {
    for sig in [libc::SIGINT, libc::SIGQUIT] {
        unsafe {
            let mut dfl: libc::sigaction = std::mem::zeroed();
            dfl.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(sig, &dfl, std::ptr::null_mut());
        }
    }
}

// Translate a foreground child's exit into the status the shell hands back.
// A non-zero exit is the command's own business: it already explained itself on
// stderr, so Ash stays quiet like other shells. Children killed by a signal
// have no numeric code; report the unexpected ones and use the conventional
// 128+signal. Ctrl-C/Ctrl-\ exits are the normal way to stop a foreground
// command, so those stay quiet too.
fn exit_code(status: &ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            if sig != libc::SIGINT && sig != libc::SIGQUIT {
                registry::print_colored(
                    &format!("ash: command terminated by signal {sig}"),
                    "warning",
                );
            }
            return 128 + sig;
        }
    }
    status.code().unwrap_or(1)
}

// Feeds the (pre-styled) prompt back to rustyline and strips ANSI codes for
// width computation, so colored prompts keep correct cursor positioning.
struct AeroHelper;

impl rustyline::completion::Completer for AeroHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let start = line[..pos].rfind(char::is_whitespace).map_or(0, |i| i + 1);
        let word = &line[start..pos];
        let completing_command = line[..start].trim().is_empty();

        let mut candidates = if completing_command {
            builtins::command_candidates(word)
        } else {
            // Argument completions contributed by packages win over path
            // completion, but fall back to paths when none match.
            let head = line[..start].split_whitespace().next().unwrap_or("");
            let mut package_candidates: Vec<String> = packages::lua_api::completions_for(head)
                .into_iter()
                .filter(|candidate| candidate.starts_with(word))
                .collect();
            if package_candidates.is_empty() {
                package_candidates = builtins::path_candidates(word);
            }
            package_candidates
        };
        candidates.sort();
        candidates.dedup();
        Ok((start, candidates))
    }
}
impl rustyline::hint::Hinter for AeroHelper {
    type Hint = String;
}
impl Highlighter for AeroHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Borrowed(prompt)
    }
}
impl rustyline::validate::Validator for AeroHelper {}
impl rustyline::Helper for AeroHelper {}

struct ShellArgs {
    command: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Option<ShellArgs>> {
    let mut command = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(long) = a.strip_prefix("--") {
            match long {
                "help" => {
                    println!("Ash (aeroshell) {}", config::aero_version());
                    println!("Usage: ash [OPTIONS] [FILE]");
                    println!("  --set-default   set Ash as your default login shell (chsh)");
                    println!("  --version       print the version");
                    println!("  --help          this help");
                    println!("  -c <cmd>        run <cmd> and exit");
                    println!("  -l, -, -il, -i  accepted (login shell flags) and ignored");
                    return Ok(None);
                }
                "version" => {
                    println!("Ash (aeroshell) {}", config::aero_version());
                    return Ok(None);
                }
                "set-default" => {
                    set_default_shell()?;
                    return Ok(None);
                }
                "" => {
                    i += 1;
                    continue;
                }
                _ => anyhow::bail!("aero: unknown option: {a} (try --help)"),
            }
        }
        if a.starts_with('-') {
            // Accept POSIX login-shell flags (-, -l, -il, -li, -i, ...) that
            // terminals/ssh/distrobox pass in; `c` means -c <cmd>.
            if a == "-" || a == "-h" {
                if a == "-h" {
                    return parse_args(&["--help".into()]);
                }
                i += 1;
                continue;
            }
            let flags = &a[1..];
            if let Some(pos) = flags.find('c') {
                let rest = &flags[pos + 1..];
                if rest.is_empty() {
                    i += 1;
                    let Some(cmd) = args.get(i) else {
                        anyhow::bail!("ash: option requires an argument: {a}");
                    };
                    command = Some(cmd.clone());
                } else {
                    command = Some(rest.to_string());
                }
            }
            i += 1;
            continue;
        }
        anyhow::bail!("ash: unknown option: {a} (try --help)");
    }
    Ok(Some(ShellArgs { command }))
}

fn main() -> Result<()> {
    let cli_args: Vec<String> = std::env::args().skip(1).collect();
    register_builtins();

    let Some(shell_args) = parse_args(&cli_args)? else {
        return Ok(()); // --help / --version / --set-default
    };

    let config = config::load_config()?;
    config::set_config(config);

    // Package failures shouldn't stop the shell from starting; each package
    // reports its own warning.
    if let Err(error) = packages::init() {
        registry::print_colored(&format!("ash: packages unavailable: {error:#}"), "warning");
    }

    if let Some(cmd) = shell_args.command {
        // `-c` exits with the command's status, like other shells. exit()
        // skips destructors, so flush first in case output is still buffered.
        let status = run_command_line(&cmd)?.unwrap_or(0);
        std::io::stdout().flush().ok();
        std::process::exit(status);
    }

    registry::print_colored("Type help for commands.", "success");

    let rl_config = RLConfig::builder()
        .history_ignore_dups(true)?
        .history_ignore_space(true)
        .completion_type(CompletionType::List)
        .build();
    let mut rl: Editor<AeroHelper, DefaultHistory> =
        Editor::with_history(rl_config, DefaultHistory::new())?;
    // with_history() leaves the helper unset; Tab needs it explicitly.
    rl.set_helper(Some(AeroHelper));

    if let Ok(path) = config::history_path() {
        let _ = rl.load_history(&path);
    }

    loop {
        if let Err(error) = packages::lua_api::run_prompt_hooks() {
            registry::print_colored(&format!("ash: prompt hook failed: {error:#}"), "warning");
        }
        let config_guard = config::active_config()?;
        let template = config_guard.prompt_template.clone();
        let prompt = config::format_prompt(&template);

        let line = match rl.readline(&prompt) {
            Ok(l) => {
                rl.add_history_entry(&l).ok();
                if let Ok(path) = config::history_path() {
                    let _ = rl.append_history(&path);
                }
                l
            }
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => {
                // Ctrl-D is normal here; EOF from a closed pipe ends input.
                if std::io::stdin().is_terminal() {
                    continue;
                }
                break;
            }
            Err(e) => {
                return Err(e.into());
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if run_command_line(trimmed)?.is_none() {
            break; // exit/quit
        }
    }

    Ok(())
}

// Run one command line (a whole `cmd | cmd | …` pipeline). Returns Ok(None)
// when the line was exit/quit, otherwise the status of the last command.
fn run_command_line(line: &str) -> Result<Option<i32>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        // Blank lines and `#` comments are no-ops. shlex would otherwise strip
        // a `# ..` line to an empty token list and report a misleading "syntax
        // error near unexpected token '|'" — the `|` is just the generic message,
        // not the actual problem.
        return Ok(Some(0));
    }
    // Split on unquoted `|` first, then tokenize each stage.
    let mut stages: Vec<Vec<String>> = Vec::new();
    let mut syntax_error = false;
    for stage in split_pipeline(trimmed) {
        match split(&stage) {
            Some(tokens) if !tokens.is_empty() => stages.push(tokens),
            _ => {
                syntax_error = true;
                break;
            }
        }
    }

    if syntax_error {
        registry::print_colored("ash: syntax error near unexpected token '|'", "error");
        return Ok(Some(2));
    }

    expand_aliases(&mut stages);

    let status = if stages.len() == 1 {
        let cmd = stages[0][0].clone();
        let args: Vec<&str> = stages[0][1..].iter().map(|s| s.as_str()).collect();

        if cmd == "exit" || cmd == "quit" {
            registry::print_colored("Exiting Ash...", "warning");
            return Ok(None);
        }

        if cmd == "unalias" {
            registry::print_colored("ash: command not found: unalias", "error");
            127
        } else {
            if let Err(error) = packages::lua_api::run_command_hooks(&cmd, &args) {
                registry::print_colored(&format!("ash: command hook failed: {error:#}"), "warning");
            }

            packages::lua_api::clear_last_exit_status();
            let status = if let Some(handler) = registry::resolve(&cmd) {
                match handler(&args) {
                    // Package commands report their Lua return value this way;
                    // builtins leave it unset.
                    Ok(()) => packages::lua_api::take_last_exit_status().unwrap_or(0),
                    Err(e) => {
                        registry::print_colored(&format!("Uncaught Error: {e}"), "error");
                        1
                    }
                }
            } else if let Some(path) = builtins::which_cmd(&cmd) {
                run_external_cmd(&cmd, &path, &args)?
            } else {
                registry::print_colored(&format!("ash: command not found: {cmd}"), "error");
                127
            };

            // Hooks see the real status, whether the command was a builtin,
            // a package command, or an external binary.
            if let Err(error) = packages::lua_api::run_command_done_hooks(&cmd, &args, status) {
                registry::print_colored(
                    &format!("ash: command_done hook failed: {error:#}"),
                    "warning",
                );
            }
            status
        }
    } else {
        run_pipeline(&stages)?
    };

    Ok(Some(status))
}

fn register_builtins() {
    use registry::register;

    // Session state
    register("history", builtins::history);
    register("export", builtins::export);
    register("unset", builtins::unset);
    register("alias", builtins::alias);
    register("type", builtins::type_cmd);
    register("which", builtins::type_cmd);

    // Core file/dir operations
    register("ls", builtins::ls);
    register("cd", builtins::cd);
    register("mkdir", builtins::mkdir);
    register("pwd", builtins::pwd);

    // Shell management
    register("exit", builtins::exit);
    register("quit", builtins::exit);
    register("clear", builtins::clear);
    register("help", builtins::help);
    register("ver", builtins::ver);
    register("aepkg", aepkg_cmd::aepkg);
}

// Expand aliases in the first word of every pipeline stage, one level deep.
fn expand_aliases(stages: &mut [Vec<String>]) {
    let Ok(cfg) = config::active_config() else {
        return;
    };
    if cfg.aliases.is_empty() {
        return;
    }
    for stage in stages.iter_mut() {
        let Some(first) = stage.first().cloned() else {
            continue;
        };
        if let Some(value) = cfg.aliases.get(&first)
            && let Some(replacement) = split(value)
        {
            let mut tokens = replacement;
            tokens.extend(stage[1..].iter().cloned());
            *stage = tokens;
        }
    }
}

// Split a command line into pipeline stages on unquoted `|`. Quotes are kept
// as-is so `shlex` tokenizes `'a|b'` as one word.
fn split_pipeline(line: &str) -> Vec<String> {
    let mut stages: Vec<String> = vec![String::new()];
    let mut quote: Option<char> = None;
    let mut chars = line.chars();

    while let Some(ch) = chars.next() {
        match quote {
            Some('"') if ch == '\\' => {
                let stage = stages.last_mut().expect("stage exists");
                stage.push(ch);
                if let Some(next) = chars.next() {
                    stage.push(next);
                }
            }
            Some(q) => {
                stages.last_mut().expect("stage exists").push(ch);
                if ch == q {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    stages.last_mut().expect("stage exists").push(ch);
                }
                '\\' => {
                    let stage = stages.last_mut().expect("stage exists");
                    stage.push(ch);
                    if let Some(next) = chars.next() {
                        stage.push(next);
                    }
                }
                '|' => stages.push(String::new()),
                _ => stages.last_mut().expect("stage exists").push(ch),
            },
        }
    }

    stages
}

// Run `cmd1 | cmd2 | …`. Only the last stage writes to the terminal; stages
// external on PATH (a builtin can't feed a pipe). Returns the last stage's
// status, which is the pipeline's status.
fn run_pipeline(stages: &[Vec<String>]) -> Result<i32> {
    // The shell ignores Ctrl-C while the command runs; this restores the
    // previous signal behavior when the guard is dropped. Acquired before the
    // first spawn so a Ctrl-C can't slip in between spawn and wait.
    #[cfg(unix)]
    let _interrupt_guard = InterruptGuard::acquire();

    let mut children: Vec<Child> = Vec::new();
    let mut prev_stdout: Option<ChildStdout> = None;

    for (i, stage) in stages.iter().enumerate() {
        let name = &stage[0];
        let args: Vec<&str> = stage[1..].iter().map(|s| s.as_str()).collect();

        let Some(path) = builtins::which_cmd(name) else {
            abort_pipeline(&mut children);
            if registry::resolve(name).is_some() {
                registry::print_colored(
                    &format!("ash: builtin '{name}' cannot be piped yet"),
                    "error",
                );
                return Ok(1);
            }
            registry::print_colored(&format!("ash: command not found: {name}"), "error");
            return Ok(127);
        };

        let mut cmd = Command::new(&path);
        // Shells pass the typed name as argv[0], not the resolved path —
        // scripts inspecting $0 (e.g. curl|bash installers) rely on it.
        #[cfg(unix)]
        let _ = cmd.arg0(name);
        cmd.args(&args);

        if let Some(out) = prev_stdout.take() {
            cmd.stdin(Stdio::from(out));
        }
        if i + 1 < stages.len() {
            cmd.stdout(Stdio::piped());
        } else {
            cmd.stdout(Stdio::inherit());
        }

        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                reset_child_signals();
                Result::<(), std::io::Error>::Ok(())
            });
        }

        match cmd.spawn() {
            Ok(mut child) => {
                prev_stdout = child.stdout.take();
                children.push(child);
            }
            Err(e) => {
                abort_pipeline(&mut children);
                registry::print_colored(&format!("ash: failed to run {name}: {e}"), "error");
                return Ok(126);
            }
        }
    }

    let mut last_status = None;
    for child in children.iter_mut() {
        last_status = Some(child.wait()?);
    }

    // Pipeline status is its last stage's; earlier stages report through the
    // last one or not at all, so only translate that status.
    Ok(last_status.map(|status| exit_code(&status)).unwrap_or(0))
}

fn abort_pipeline(children: &mut [Child]) {
    for child in children.iter_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

// ash --set-default: add the binary to /etc/shells, then chsh -s.
fn set_default_shell() -> Result<()> {
    use std::io::Write;

    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/usr/bin/ash".into());
    if !exe.starts_with('/') {
        anyhow::bail!("ash: cannot determine absolute binary path (got {exe})");
    }
    println!("Setting default shell to {exe} ...");

    let shells = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    let listed = shells.lines().any(|l| l.trim() == exe);
    if !listed {
        println!("{exe} is not in /etc/shells — adding it (needs sudo) ...");
        let direct = std::fs::OpenOptions::new()
            .append(true)
            .open("/etc/shells")
            .and_then(|mut f| writeln!(f, "{exe}").map_err(|e| e.into()));
        if direct.is_err() {
            let status = std::process::Command::new("sudo")
                .arg("sh")
                .arg("-c")
                .arg(format!("echo {exe} >> /etc/shells"))
                .status()?;
            if !status.success() {
                anyhow::bail!("ash: failed to add {exe} to /etc/shells");
            }
        }
    }

    let status = std::process::Command::new("chsh")
        .arg("-s")
        .arg(&exe)
        .status()?;
    if !status.success() {
        anyhow::bail!("ash: chsh failed — try `sudo chsh -s {exe} $USER`");
    }

    println!("Default shell is now {exe}. Log out and back in to use it.");
    Ok(())
}

// Run a foreground PATH binary and return its status. Spawn failures are
// reported here instead of bubbling up, so a missing interpreter or an
// unreadable binary doesn't take the whole shell down.
fn run_external_cmd(name: &str, path: &str, args: &[&str]) -> Result<i32> {
    let mut command = std::process::Command::new(path);
    // Same argv[0] rule as the pipeline path: typed name, not resolved path.
    #[cfg(unix)]
    let _ = command.arg0(name);
    command.args(args);

    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            reset_child_signals();
            Result::<(), std::io::Error>::Ok(())
        });
    }

    // Ignore Ctrl-C in the shell while the command runs; the child keeps its
    // own default handling via reset_child_signals().
    #[cfg(unix)]
    let _interrupt_guard = InterruptGuard::acquire();

    match command.status() {
        Ok(status) => Ok(exit_code(&status)),
        Err(error) => {
            registry::print_colored(&format!("ash: failed to run {name}: {error}"), "error");
            Ok(126)
        }
    }
}
