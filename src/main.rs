use anyhow::Result;
use rustyline::highlight::Highlighter;
use rustyline::history::DefaultHistory;
use rustyline::{CompletionType, Config as RLConfig, Editor};
use shlex::split;
use std::borrow::Cow;
use std::io::{IsTerminal, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};

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

// Same status translation without the signal report: stages of a pipeline
// other than the last die quietly (a reader closing early, e.g. `grep -q`, is
// normal pipeline traffic), like other shells.
fn quiet_exit_code(status: &ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
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

// Run one command line: pipelines joined by `&&`, `||` and `;`, with each
// stage's redirections applied. Returns Ok(None) when the line was exit/quit,
// otherwise the status of the last command that ran.
fn run_command_line(line: &str) -> Result<Option<i32>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        // Blank lines and `#` comments are no-ops. `scan` stops at an unquoted
        // `#` that starts a word, so this only keeps the common case cheap.
        return Ok(Some(0));
    }

    let parsed = parse_line(trimmed).and_then(|mut steps| unquote(&mut steps).map(|()| steps));
    let mut steps = match parsed {
        Ok(steps) => steps,
        Err(message) => {
            registry::print_colored(&message, "error");
            return Ok(Some(2));
        }
    };
    if steps.is_empty() {
        return Ok(Some(0));
    }

    expand_aliases(&mut steps);

    // Run the steps left to right: `&&` needs the previous status to be 0,
    // `||` needs it non-zero, `;` runs regardless. A skipped step leaves the
    // status untouched, so `false && a || b` reaches `b`.
    let mut status = 0;
    for (i, step) in steps.iter().enumerate() {
        if i > 0 && !step_runs_after(&steps[i - 1], status) {
            continue;
        }
        // exit/quit ends the session. Report it here instead of letting the
        // builtin call exit() so the REPL breaks and `-c` exits cleanly.
        if let [stage] = step.stages.as_slice()
            && stage
                .words
                .first()
                .is_some_and(|w| w == "exit" || w == "quit")
        {
            registry::print_colored("Exiting Ash...", "warning");
            return Ok(None);
        }
        status = run_pipeline(&step.stages)?;
    }

    Ok(Some(status))
}

// Whether the step after `previous` runs, given the status before it.
fn step_runs_after(previous: &Step, status: i32) -> bool {
    match previous.join {
        Some(Join::And) => status == 0,
        Some(Join::Or) => status != 0,
        _ => true,
    }
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

// Expand aliases in the command word of every pipeline stage, one level deep.
fn expand_aliases(steps: &mut [Step]) {
    let Ok(cfg) = config::active_config() else {
        return;
    };
    if cfg.aliases.is_empty() {
        return;
    }
    for step in steps {
        for stage in &mut step.stages {
            let Some(first) = stage.words.first().cloned() else {
                continue;
            };
            if let Some(value) = cfg.aliases.get(&first)
                && let Some(replacement) = split(value)
            {
                let mut tokens = replacement;
                tokens.extend(stage.words[1..].iter().cloned());
                stage.words = tokens;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Line parsing: sequencing, pipelines and redirection
//
// A command line parses into "steps" joined by `&&`, `||` and `;`; each step
// is a pipeline of stages, and each stage is its words plus its redirections
// (`<`, `>`, `>>`, `2>`, `2>>`, `&>`, `2>&1`).
// ---------------------------------------------------------------------------

// One token from the scanner. Word text keeps its quotes and backslashes so
// `shlex` can strip them later (alias expansion re-tokenizes too).
enum Tok {
    Word(String),
    Pipe,
    And,
    Or,
    Semi,
    Redir(RedirKind),
}

#[derive(Clone, Copy)]
enum RedirKind {
    In,                         // < file
    Out,                        // > file
    Append,                     // >> file
    ErrOut,                     // 2> file
    ErrAppend,                  // 2>> file
    OutErr,                     // &> file
    Dup { fd: i32, from: i32 }, // 2>&1 (takes no target word)
}

impl RedirKind {
    // How the operator is named in "syntax error near unexpected token '…'".
    fn token(self) -> &'static str {
        match self {
            RedirKind::In => "<",
            RedirKind::Out => ">",
            RedirKind::Append => ">>",
            RedirKind::ErrOut => "2>",
            RedirKind::ErrAppend => "2>>",
            RedirKind::OutErr => "&>",
            RedirKind::Dup { .. } => ">&",
        }
    }

    // Whether the operator takes a target file word after it.
    fn takes_target(self) -> bool {
        !matches!(self, RedirKind::Dup { .. })
    }
}

struct Redir {
    kind: RedirKind,
    target: String,
}

#[derive(Default)]
struct Stage {
    words: Vec<String>,
    redirs: Vec<Redir>,
}

// How a step connects to the step after it.
enum Join {
    And,
    Or,
    Semi,
}

struct Step {
    stages: Vec<Stage>,
    join: Option<Join>, // None on the last step
}

fn syntax_error(token: &str) -> String {
    format!("ash: syntax error near unexpected token '{token}'")
}

fn flush_word(word: &mut String, toks: &mut Vec<Tok>) {
    if !word.is_empty() {
        toks.push(Tok::Word(std::mem::take(word)));
    }
}

// Scan a line into words and operators. Quotes and backslash escapes keep
// their characters (so `shlex` sees the original word), operator characters
// inside quotes stay part of the word, and an unquoted `#` at the start of a
// word ends the line as a comment.
fn scan(line: &str) -> Vec<Tok> {
    let chars: Vec<char> = line.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        match quote {
            Some(q) => {
                if ch == q {
                    // Keep the closing quote in the word: shlex strips the
                    // balanced pair later.
                    word.push(ch);
                    quote = None;
                    i += 1;
                } else if q == '"' && ch == '\\' && i + 1 < chars.len() {
                    word.push(ch);
                    word.push(chars[i + 1]);
                    i += 2;
                } else {
                    word.push(ch);
                    i += 1;
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    word.push(ch);
                    i += 1;
                }
                '\\' if i + 1 < chars.len() => {
                    word.push(ch);
                    word.push(chars[i + 1]);
                    i += 2;
                }
                '\\' => {
                    word.push(ch);
                    i += 1;
                }
                '#' if word.is_empty() => break,
                c if c.is_whitespace() => {
                    flush_word(&mut word, &mut toks);
                    i += 1;
                }
                '|' => {
                    flush_word(&mut word, &mut toks);
                    if chars.get(i + 1) == Some(&'|') {
                        toks.push(Tok::Or);
                        i += 2;
                    } else {
                        toks.push(Tok::Pipe);
                        i += 2;
                    }
                }
                '&' => {
                    flush_word(&mut word, &mut toks);
                    if chars.get(i + 1) == Some(&'&') {
                        toks.push(Tok::And);
                        i += 2;
                    } else if chars.get(i + 1) == Some(&'>') {
                        toks.push(Tok::Redir(RedirKind::OutErr));
                        i += 2;
                    } else {
                        word.push('&');
                        i += 1;
                    }
                }
                ';' => {
                    flush_word(&mut word, &mut toks);
                    toks.push(Tok::Semi);
                    i += 1;
                }
                '<' => {
                    flush_word(&mut word, &mut toks);
                    toks.push(Tok::Redir(RedirKind::In));
                    i += 1;
                }
                '>' => {
                    flush_word(&mut word, &mut toks);
                    i = scan_redirect(&chars, i, 1, &mut toks);
                }
                // Digits directly before `>` name the descriptor: `2>`,
                // `2>>`, `2>&1`, and the `1` in `1>` is the default anyway.
                d if d.is_ascii_digit() && word.is_empty() => {
                    let mut j = i;
                    while j < chars.len() && chars[j].is_ascii_digit() {
                        j += 1;
                    }
                    let fd: i32 = chars[i..j].iter().collect::<String>().parse().unwrap_or(-1);
                    if chars.get(j) == Some(&'>') && (fd == 1 || fd == 2) {
                        i = scan_redirect(&chars, j, fd, &mut toks);
                    } else {
                        word.push(d);
                        i += 1;
                    }
                }
                c => {
                    word.push(c);
                    i += 1;
                }
            },
        }
    }

    flush_word(&mut word, &mut toks);
    toks
}

// Scan a `>` seen at index `i` (descriptor 1 for a bare `>`), pushing the
// matching redirect token and returning the next index.
fn scan_redirect(chars: &[char], i: usize, fd: i32, toks: &mut Vec<Tok>) -> usize {
    let mut j = i + 1;
    let kind = if chars.get(j) == Some(&'>') {
        j += 1;
        if fd == 1 {
            RedirKind::Append
        } else {
            RedirKind::ErrAppend
        }
    } else if chars.get(j) == Some(&'&') && chars.get(j + 1).is_some_and(|c| c.is_ascii_digit()) {
        // `2>&1` / `>&1`: duplicate one descriptor onto another, no file word.
        let mut k = j + 2;
        while k < chars.len() && chars[k].is_ascii_digit() {
            k += 1;
        }
        let from: i32 = chars[j + 2..k]
            .iter()
            .collect::<String>()
            .parse()
            .unwrap_or(1);
        toks.push(Tok::Redir(RedirKind::Dup { fd, from }));
        return k;
    } else if fd == 1 {
        RedirKind::Out
    } else {
        RedirKind::ErrOut
    };
    toks.push(Tok::Redir(kind));
    j
}

fn tok_name(tok: &Tok) -> &'static str {
    match tok {
        Tok::Word(_) => "word",
        Tok::Pipe => "|",
        Tok::And => "&&",
        Tok::Or => "||",
        Tok::Semi => ";",
        Tok::Redir(kind) => kind.token(),
    }
}

// Group the token stream into steps of pipelines. Errors carry the full
// `ash: syntax error …` message.
fn parse_line(line: &str) -> Result<Vec<Step>, String> {
    let toks = scan(line);
    if toks.is_empty() {
        return Ok(Vec::new());
    }

    let mut steps: Vec<Step> = Vec::new();
    let mut cur: Vec<Stage> = Vec::new(); // stages of the step being built
    let mut stage = Stage::default();
    let mut open = ""; // operator that opened `stage`, for error messages
    let mut target: Option<RedirKind> = None; // redirect waiting for its file

    for tok in toks {
        // A redirect takes the next word as its target file.
        if let Some(kind) = target {
            match tok {
                Tok::Word(word) => {
                    target = None;
                    stage.redirs.push(Redir { kind, target: word });
                    continue;
                }
                other => return Err(syntax_error(tok_name(&other))),
            }
        }

        match tok {
            Tok::Word(word) => stage.words.push(word),
            Tok::Redir(kind) if kind.takes_target() => target = Some(kind),
            Tok::Redir(kind) => stage.redirs.push(Redir {
                kind,
                target: String::new(),
            }),
            op => {
                let (name, join) = match op {
                    Tok::Pipe => ("|", None),
                    Tok::And => ("&&", Some(Join::And)),
                    Tok::Or => ("||", Some(Join::Or)),
                    _ => (";", Some(Join::Semi)),
                };
                if stage.words.is_empty() && stage.redirs.is_empty() {
                    return Err(syntax_error(name));
                }
                cur.push(std::mem::take(&mut stage));
                open = name;
                if let Some(join) = join {
                    steps.push(Step {
                        stages: std::mem::take(&mut cur),
                        join: Some(join),
                    });
                }
            }
        }
    }

    // A dangling redirect with no file, or a line that stops right after `|`,
    // `&&` or `||`, is a syntax error. A trailing `;` is fine.
    if let Some(kind) = target {
        return Err(syntax_error(kind.token()));
    }
    if stage.words.is_empty() && stage.redirs.is_empty() {
        if !open.is_empty() && open != ";" {
            return Err(syntax_error(open));
        }
    } else {
        cur.push(stage);
    }
    if !cur.is_empty() {
        steps.push(Step {
            stages: cur,
            join: None,
        });
    }
    Ok(steps)
}

// Turn each raw word and redirect target into its final text. Quotes and
// escapes survive the scanner, so `shlex` removes them here — exactly as it
// tokenized whole stages before redirections existed.
fn unquote(steps: &mut [Step]) -> Result<(), String> {
    for step in steps {
        for stage in &mut step.stages {
            let mut words = Vec::with_capacity(stage.words.len());
            for word in stage.words.drain(..) {
                match shlex::split(&word) {
                    Some(parts) => words.extend(parts),
                    None => return Err("ash: syntax error: unmatched quote".to_string()),
                }
            }
            stage.words = words;
            for redir in &mut stage.redirs {
                if !redir.kind.takes_target() {
                    continue;
                }
                match shlex::split(&redir.target) {
                    Some(parts) => redir.target = parts.into_iter().next().unwrap_or_default(),
                    None => return Err("ash: syntax error: unmatched quote".to_string()),
                }
            }
        }
    }
    Ok(())
}

// Run one pipeline: `stage1 | stage2 | …`, each stage with its own
// redirections. Builtins and package commands run in-process with the shell's
// stdio temporarily swapped (they print straight to the terminal otherwise);
// external commands are spawned first so every pipe already has a consumer
// before a builtin starts writing. Returns the last stage's status, which is
// the pipeline's status.
fn run_pipeline(stages: &[Stage]) -> Result<i32> {
    let n = stages.len();

    // Ignore Ctrl-C only when external commands are involved: their children
    // restore the default dispositions before exec (see InterruptGuard), while
    // a builtin alone behaves exactly as it does outside a pipeline.
    #[cfg(unix)]
    let _interrupt_guard = stages
        .iter()
        .any(|stage| {
            stage
                .words
                .first()
                .is_some_and(|name| registry::resolve(name).is_none())
        })
        .then(InterruptGuard::acquire);

    // One pipe per stage boundary: `pipes[i]` carries stage i's stdout into
    // stage i + 1's stdin. A stage's own redirections override the connection
    // later, like POSIX (redirections apply after the pipeline is set up).
    let mut pipes: Vec<(std::fs::File, std::fs::File)> = Vec::with_capacity(n);
    for _ in 0..n.saturating_sub(1) {
        let Some((read, write)) = pipe_pair() else {
            registry::print_colored("ash: cannot create pipe", "error");
            return Ok(1);
        };
        pipes.push((read, write));
    }

    // Where each stage's fds point. `None` marks a stage whose redirection
    // couldn't be opened: it doesn't run and reports status 1.
    let mut slots: Vec<Option<[FdSlot; 3]>> = Vec::with_capacity(n);
    let mut status: Vec<Option<i32>> = vec![None; n];
    for i in 0..n {
        let stdin = (i > 0).then(|| &pipes[i - 1].0);
        let stdout = pipes.get(i).map(|(_read, write)| write);
        match stage_slots(&stages[i], stdin, stdout) {
            Ok(resolved) => slots.push(Some(resolved)),
            Err(message) => {
                registry::print_colored(&message, "error");
                status[i] = Some(1);
                slots.push(None);
            }
        }
    }
    // Only the per-stage copies remain, so every pipe closes as its stage ends.
    drop(pipes);

    // A stage of only redirections runs no command: its files are already
    // open (that is the whole effect), and holding a pipe end would stall the
    // stage on the other side of it.
    for (i, stage) in stages.iter().enumerate() {
        if stage.words.is_empty() {
            slots[i] = None;
            if status[i].is_none() {
                status[i] = Some(0);
            }
        }
    }

    // Package `command` hooks, in stage order, before anything runs.
    let mut hooked = vec![false; n];
    for (i, stage) in stages.iter().enumerate() {
        let Some(name) = stage.words.first() else {
            continue;
        };
        if slots[i].is_none() {
            continue;
        }
        hooked[i] = true;
        let args: Vec<&str> = stage.words[1..].iter().map(|s| s.as_str()).collect();
        if let Err(error) = packages::lua_api::run_command_hooks(name, &args) {
            registry::print_colored(&format!("ash: command hook failed: {error:#}"), "warning");
        }
    }

    // External stages first, in stage order: every pipe gets its consumer
    // before a builtin starts producing output into one.
    let mut children: Vec<(usize, Child)> = Vec::new();
    for (i, stage) in stages.iter().enumerate() {
        let Some(name) = stage.words.first() else {
            continue; // redirection-only stage: handled above
        };
        if registry::resolve(name).is_some() {
            continue; // builtin or package command: the phase below
        }
        let Some(resolved) = slots[i].take() else {
            continue; // redirection failed
        };
        let args: Vec<&str> = stage.words[1..].iter().map(|s| s.as_str()).collect();

        if name == "unalias" {
            registry::print_colored("ash: command not found: unalias", "error");
            status[i] = Some(127);
            continue;
        }
        let Some(path) = builtins::which_cmd(name) else {
            registry::print_colored(&format!("ash: command not found: {name}"), "error");
            status[i] = Some(127);
            continue;
        };

        let mut cmd = Command::new(&path);
        // Shells pass the typed name as argv[0], not the resolved path —
        // scripts inspecting $0 (e.g. curl|bash installers) rely on it.
        #[cfg(unix)]
        let _ = cmd.arg0(name);
        cmd.args(&args);
        let [stdin, stdout, stderr] = resolved;
        cmd.stdin(slot_to_stdio(stdin));
        cmd.stdout(slot_to_stdio(stdout));
        cmd.stderr(slot_to_stdio(stderr));

        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                reset_child_signals();
                Result::<(), std::io::Error>::Ok(())
            });
        }

        match cmd.spawn() {
            Ok(child) => children.push((i, child)),
            Err(error) => {
                registry::print_colored(&format!("ash: failed to run {name}: {error}"), "error");
                status[i] = Some(126);
            }
        }
    }

    // Builtins and package commands next, in stage order, each with the
    // shell's own fds pointed at its stage's destinations for the duration.
    for (i, stage) in stages.iter().enumerate() {
        let Some(name) = stage.words.first() else {
            continue;
        };
        if registry::resolve(name).is_none() {
            continue;
        }
        let Some(resolved) = slots[i].as_ref() else {
            continue; // redirection failed
        };
        let args: Vec<&str> = stage.words[1..].iter().map(|s| s.as_str()).collect();
        status[i] = run_registered(name, &args, resolved);
    }

    // Builtins are done, so close every descriptor they were holding: a
    // consumer waiting for EOF on a builtin's pipe must see it before the
    // waits below.
    drop(slots);

    // Wait for the external stages in stage order. Only the last stage's
    // status is the pipeline's; the others are never reported, so their
    // signal deaths stay quiet.
    for (index, child) in children.iter_mut() {
        let raw = child.wait()?;
        status[*index] = Some(if *index == n - 1 {
            exit_code(&raw)
        } else {
            quiet_exit_code(&raw)
        });
    }

    // External stages report their `command_done` hooks once every status is
    // known; builtins already reported theirs above.
    for (i, stage) in stages.iter().enumerate() {
        if !hooked[i] {
            continue;
        }
        let Some(name) = stage.words.first() else {
            continue;
        };
        if registry::resolve(name).is_some() {
            continue;
        }
        let args: Vec<&str> = stage.words[1..].iter().map(|s| s.as_str()).collect();
        let code = status[i].unwrap_or(0);
        if let Err(error) = packages::lua_api::run_command_done_hooks(name, &args, code) {
            registry::print_colored(
                &format!("ash: command_done hook failed: {error:#}"),
                "warning",
            );
        }
    }

    Ok(status[n - 1].unwrap_or(0))
}

// Where one of the shell's stdio descriptors points while a stage runs.
enum FdSlot {
    Inherit,             // leave the descriptor as it is (the terminal)
    File(std::fs::File), // redirect here; pipe ends count as files
}

fn inherit_slots() -> [FdSlot; 3] {
    [FdSlot::Inherit, FdSlot::Inherit, FdSlot::Inherit]
}

// Create a pipe. Both ends are close-on-exec so no other stage (or a command
// a builtin spawns) can inherit an end it doesn't own: a stray end would hold
// the pipe open and stall its reader at EOF.
fn pipe_pair() -> Option<(std::fs::File, std::fs::File)> {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    for fd in fds {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let write = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    Some((read, write))
}

// Work out where each stdio descriptor points for one stage: the pipeline
// pipes first, then the stage's own redirections — which win, so `cmd > f | x`
// writes to `f` and the pipe reads nothing.
fn stage_slots(
    stage: &Stage,
    stdin_pipe: Option<&std::fs::File>,
    stdout_pipe: Option<&std::fs::File>,
) -> Result<[FdSlot; 3], String> {
    let mut slots = inherit_slots();
    if let Some(read) = stdin_pipe {
        slots[0] = FdSlot::File(read.try_clone().map_err(|e| e.to_string())?);
    }
    if let Some(write) = stdout_pipe {
        slots[1] = FdSlot::File(write.try_clone().map_err(|e| e.to_string())?);
    }
    for redir in &stage.redirs {
        match redir.kind {
            RedirKind::In => slots[0] = FdSlot::File(open_target(&redir.target, false, false)?),
            RedirKind::Out => slots[1] = FdSlot::File(open_target(&redir.target, true, false)?),
            RedirKind::Append => slots[1] = FdSlot::File(open_target(&redir.target, true, true)?),
            RedirKind::ErrOut => slots[2] = FdSlot::File(open_target(&redir.target, true, false)?),
            RedirKind::ErrAppend => {
                slots[2] = FdSlot::File(open_target(&redir.target, true, true)?)
            }
            RedirKind::OutErr => {
                slots[1] = FdSlot::File(open_target(&redir.target, true, false)?);
                slots[2] = dup_slot(&slots[1])?;
            }
            // `2>&1` copies wherever fd 1 currently points — a pipe, a file
            // or the terminal — onto fd 2, in the order the user wrote it.
            RedirKind::Dup { fd, from } => {
                if (0..3).contains(&fd) && (0..3).contains(&from) {
                    slots[fd as usize] = dup_slot(&slots[from as usize])?;
                }
            }
        }
    }
    Ok(slots)
}

// Open a redirection target, expanding a leading `~` like the shell does
// everywhere else.
fn open_target(target: &str, write: bool, append: bool) -> Result<std::fs::File, String> {
    let path = builtins::expand_tilde(target);
    std::fs::OpenOptions::new()
        .read(!write)
        .write(write)
        .append(append)
        .create(write)
        .truncate(write && !append)
        .open(path)
        .map_err(|error| format!("ash: cannot open {target}: {error}"))
}

// A second handle on the same file or pipe: same open description, so write
// ends, offsets and pipes are shared exactly like `2>&1` expects.
fn dup_slot(slot: &FdSlot) -> Result<FdSlot, String> {
    Ok(match slot {
        FdSlot::Inherit => FdSlot::Inherit,
        FdSlot::File(file) => FdSlot::File(file.try_clone().map_err(|e| e.to_string())?),
    })
}

// Hand a stage's stdio slot to a child process.
fn slot_to_stdio(slot: FdSlot) -> Stdio {
    match slot {
        FdSlot::Inherit => Stdio::inherit(),
        FdSlot::File(file) => Stdio::from(file),
    }
}

// Point the shell's own stdin/stdout/stderr at `slots` while `f` runs, then
// put them back. Builtins and package commands print with println! and
// print_colored, so the process descriptors are the only way to redirect
// them or to feed their output into a pipeline.
fn with_redirects(slots: &[FdSlot; 3], f: impl FnOnce() -> i32) -> i32 {
    // Land buffered output on the current descriptors before the switch…
    flush_all();
    let mut saved: [Option<std::fs::File>; 3] = [None, None, None];
    for (fd, slot) in slots.iter().enumerate() {
        let target = match slot {
            FdSlot::Inherit => continue,
            FdSlot::File(file) => file.as_raw_fd(),
        };
        let previous = unsafe { libc::dup(fd as i32) };
        if previous >= 0 {
            saved[fd] = Some(unsafe { std::fs::File::from_raw_fd(previous) });
        }
        if previous < 0 || unsafe { libc::dup2(target, fd as i32) } < 0 {
            let error = std::io::Error::last_os_error();
            restore_fds(&mut saved);
            registry::print_colored(&format!("ash: redirection failed: {error}"), "error");
            return 1;
        }
    }

    // A handler that panics while its output is redirected would spray the
    // panic message into the redirected stream — and Rust's println! panics
    // instead of dying on EPIPE, which `ls | grep -q` hits when grep exits on
    // its first match. Catch it while redirected, keep the hook quiet, and
    // report after the descriptors are restored.
    let redirected = slots.iter().any(|slot| !matches!(slot, FdSlot::Inherit));
    let status = if redirected {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        std::panic::set_hook(previous_hook);
        match result {
            Ok(status) => status,
            Err(payload) => {
                let message = panic_message(payload.as_ref());
                if message.contains("Broken pipe") {
                    // The consumer is gone; report the usual 128 + SIGPIPE, as
                    // if the builtin had died from the signal.
                    141
                } else {
                    registry::print_colored(&format!("ash: command panicked: {message}"), "error");
                    1
                }
            }
        }
    } else {
        f()
    };

    // …and flush again before restoring, so the output reaches the pipe or
    // file rather than the terminal we are about to switch back to.
    flush_all();
    restore_fds(&mut saved);
    status
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

fn flush_all() {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // Lua writes through the C stdio buffer, which Rust's flush can't see.
    unsafe { libc::fflush(std::ptr::null_mut()) };
}

fn restore_fds(saved: &mut [Option<std::fs::File>; 3]) {
    for (fd, previous) in saved.iter_mut().enumerate() {
        if let Some(copy) = previous.take() {
            unsafe { libc::dup2(copy.as_raw_fd(), fd as i32) };
            // `copy` (a spare of the original) closes here.
        }
    }
}

// Run a registered builtin or package command, with its hooks and Lua exit
// status accounting, pointing the shell's stdio at `slots` while the handler
// runs. Returns None when `name` isn't registered.
fn run_registered(name: &str, args: &[&str], slots: &[FdSlot; 3]) -> Option<i32> {
    let handler = registry::resolve(name)?;
    let status = with_redirects(slots, || {
        packages::lua_api::clear_last_exit_status();
        match handler(args) {
            // Package commands report their Lua return value this way;
            // builtins leave it unset.
            Ok(()) => packages::lua_api::take_last_exit_status().unwrap_or(0),
            Err(e) => {
                registry::print_colored(&format!("Uncaught Error: {e}"), "error");
                1
            }
        }
    });
    // Packages see the real status of their command.
    if let Err(error) = packages::lua_api::run_command_done_hooks(name, args, status) {
        registry::print_colored(
            &format!("ash: command_done hook failed: {error:#}"),
            "warning",
        );
    }
    Some(status)
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
