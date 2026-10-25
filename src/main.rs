use anyhow::Result;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::history::{DefaultHistory, History, SearchDirection};
use rustyline::{
    Cmd, CompletionType, ConditionalEventHandler, Config as RLConfig, Editor, Event, EventContext,
    EventHandler, KeyCode, KeyEvent, Modifiers, Movement, RepeatCount,
};
use shlex;
use std::borrow::Cow;
use std::io::{IsTerminal, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use aeroshell::{config, packages, registry};

mod commands;
mod history_search;
mod syntax_highlight;

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

#[derive(Clone, Default)]
struct HintState(Arc<AtomicBool>);

impl HintState {
    fn shown(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn set(&self, shown: bool) {
        self.0.store(shown, Ordering::Relaxed);
    }
}

// Feeds the (pre-styled) prompt back to rustyline and strips ANSI codes for
// width computation, so colored prompts keep correct cursor positioning.
struct AeroHelper {
    hint_state: HintState,
}

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

    fn hint(&self, line: &str, pos: usize, ctx: &rustyline::Context<'_>) -> Option<String> {
        // A package completion is context-specific, so it wins over a history
        // suggestion when both apply. Only registered, in-memory candidates
        // are considered here; filesystem completion stays on Tab.
        let hint = package_completion_hint(line, pos)
            .or_else(|| history_suggestion(line, pos, ctx.history(), ctx.history_index()));
        self.hint_state.set(hint.is_some());
        hint
    }
}

// Preview the rest of a package-provided argument candidate while the word is
// being typed. Package completions are a plain in-memory lookup, so this is
// safe to run on every keystroke; path candidates are not (they read the
// filesystem) and are left to Tab completion.
fn package_completion_hint(line: &str, pos: usize) -> Option<String> {
    if line.is_empty() || pos < line.len() || line.contains('\n') {
        return None;
    }
    let start = line[..pos].rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let word = &line[start..pos];
    if word.is_empty() {
        return None;
    }
    let head = line[..start].split_whitespace().next()?;
    packages::lua_api::completion_preview(head, word)
}

fn history_suggestion(
    line: &str,
    pos: usize,
    history: &dyn History,
    history_index: usize,
) -> Option<String> {
    if line.is_empty() || pos < line.len() || line.contains('\n') {
        return None;
    }

    // While browsing history, stay at or before the entry being edited.
    let mut start = if history_index >= history.len() {
        history.len().saturating_sub(1)
    } else {
        history_index
    };
    loop {
        let found = history
            .starts_with(line, start, SearchDirection::Reverse)
            .ok()
            .flatten()?;
        let suffix = &found.entry[line.len()..];
        if !suffix.is_empty() && !suffix.contains('\n') {
            return Some(suffix.to_string());
        }
        // An exact entry (or a multi-line one) offers nothing to show; try
        // the entry before it.
        if found.idx == 0 {
            return None;
        }
        start = found.idx - 1;
    }
}

// The line being edited when history search was requested, plus the width of
// any inline hint drawn after it. The line is restored at its original cursor
// position if the picker is cancelled, and the hint width lets the caller know
// whether the interrupted prompt occupied more than one terminal row.
#[derive(Clone, Default)]
struct SearchRequest(Arc<Mutex<Option<(String, usize, usize)>>>);

impl SearchRequest {
    fn set(&self, line: &str, pos: usize, hint_len: usize) {
        *self.0.lock().expect("search request lock poisoned") =
            Some((line.to_string(), pos, hint_len));
    }

    fn take(&self) -> Option<(String, usize, usize)> {
        self.0
            .lock()
            .expect("search request lock poisoned")
            .take()
    }

    fn clear(&self) {
        *self.0.lock().expect("search request lock poisoned") = None;
    }
}

// Ctrl-R opens the history picker from anywhere on the line. Up does it only
// when a partial command has been typed (so plain Up still browses history on
// an empty prompt, the way it always has).
struct OpenHistorySearch {
    request: SearchRequest,
    needs_prefix: bool,
}

impl ConditionalEventHandler for OpenHistorySearch {
    fn handle(
        &self,
        _event: &Event,
        _count: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        if self.needs_prefix && ctx.line().trim().is_empty() {
            return Some(Cmd::PreviousHistory);
        }
        let hint_len = ctx.hint_text().map_or(0, |hint| hint.chars().count());
        self.request.set(ctx.line(), ctx.pos(), hint_len);
        Some(Cmd::Interrupt)
    }
}

struct AcceptHint {
    hint_state: HintState,
    fallback: Cmd,
}

impl ConditionalEventHandler for AcceptHint {
    fn handle(
        &self,
        _event: &Event,
        count: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        if self.hint_state.shown() && ctx.pos() == ctx.line().len() {
            return Some(Cmd::CompleteHint);
        }
        Some(match self.fallback {
            Cmd::Move(Movement::ForwardChar(_)) => Cmd::Move(Movement::ForwardChar(count)),
            ref other => other.clone(),
        })
    }
}

impl Highlighter for AeroHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        Cow::Owned(syntax_highlight::highlight(line))
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Borrowed(prompt)
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(config::colorize(hint, "autosuggestion"))
    }

    fn highlight_candidate<'c>(
        &self,
        candidate: &'c str,
        _completion: CompletionType,
    ) -> Cow<'c, str> {
        Cow::Owned(config::colorize(candidate, "autosuggestion"))
    }

    fn highlight_char(&self, line: &str, pos: usize, kind: CmdKind) -> bool {
        // Ask for a repaint when the cursor leaves the end of the line so the
        // now-stale suggestion is erased. It stays hidden until an edit makes
        // rustyline compute a hint again.
        if kind == CmdKind::MoveCursor && self.hint_state.shown() && pos < line.len() {
            self.hint_state.set(false);
        }
        true
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
    let hint_state = HintState::default();
    rl.set_helper(Some(AeroHelper {
        hint_state: hint_state.clone(),
    }));
    let accept_keys = [
        (
            KeyEvent(KeyCode::Right, Modifiers::NONE),
            Cmd::Move(Movement::ForwardChar(1)),
        ),
        (
            KeyEvent(KeyCode::End, Modifiers::NONE),
            Cmd::Move(Movement::EndOfLine),
        ),
        (KeyEvent::ctrl('E'), Cmd::Move(Movement::EndOfLine)),
        (KeyEvent::ctrl('F'), Cmd::Move(Movement::ForwardChar(1))),
    ];
    for (key, fallback) in accept_keys {
        let _ = rl.bind_sequence(
            key,
            EventHandler::Conditional(Box::new(AcceptHint {
                hint_state: hint_state.clone(),
                fallback,
            })),
        );
    }
    let search_request = SearchRequest::default();
    for (key, needs_prefix) in [
        (KeyEvent::ctrl('R'), false),
        (KeyEvent(KeyCode::Up, Modifiers::NONE), true),
    ] {
        let _ = rl.bind_sequence(
            key,
            EventHandler::Conditional(Box::new(OpenHistorySearch {
                request: search_request.clone(),
                needs_prefix,
            })),
        );
    }

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

        let mut line = match read_prompt_line(&mut rl, &prompt, &search_request)? {
            PromptLine::Line(line) => line,
            PromptLine::Interrupted => continue,
            PromptLine::Eof => {
                // Ctrl-D is normal here; EOF from a closed pipe ends input.
                if std::io::stdin().is_terminal() {
                    continue;
                }
                break;
            }
        };

        // An opened heredoc is read from the following lines, up to its
        // delimiter. Keep asking for them here so the command and its body
        // reach the parser together; Ctrl-C or Ctrl-D cancels the heredoc.
        let mut cancelled = false;
        while let Err(HereDocError::Unterminated(delimiter)) = split_heredocs(&line) {
            let continuation = config::colorize("> ", "green");
            match rl.readline(&continuation) {
                Ok(more) => {
                    line.push('\n');
                    line.push_str(&more);
                }
                Err(rustyline::error::ReadlineError::Interrupted)
                | Err(rustyline::error::ReadlineError::Eof) => {
                    // Ctrl-R inside a heredoc body cancels the heredoc rather
                    // than searching; make sure it cannot leak into the next
                    // prompt's Ctrl-C handling.
                    search_request.clear();
                    registry::print_colored(
                        &format!("ash: heredoc '{delimiter}' cancelled"),
                        "warning",
                    );
                    cancelled = true;
                    break;
                }
                Err(e) => return Err(e.into()),
            }
        }
        if cancelled {
            continue;
        }

        if !line.trim().is_empty() {
            rl.add_history_entry(&line).ok();
            if let Ok(path) = config::history_path() {
                let _ = rl.append_history(&path);
            }
        }

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

// The result of reading at the interactive prompt.
enum PromptLine {
    Line(String),
    Interrupted,
    Eof,
}

// Read one prompt line, interposing the history picker when Ctrl-R (or Up on
// a partial command) asked for it. A picker accepts a command back into the
// prompt for editing; cancelling restores the line and cursor that were being
// edited before the search.
fn read_prompt_line(
    rl: &mut Editor<AeroHelper, DefaultHistory>,
    prompt: &str,
    search: &SearchRequest,
) -> rustyline::Result<PromptLine> {
    // (text before the cursor, text after it) for the next readline call.
    let mut resume: Option<(String, String)> = None;
    loop {
        let read = match &resume {
            Some((left, right)) => rl.readline_with_initial(prompt, (left.as_str(), right.as_str())),
            None => rl.readline(prompt),
        };
        match read {
            Ok(line) => return Ok(PromptLine::Line(line)),
            Err(rustyline::error::ReadlineError::Interrupted) => {
                let Some((line, pos, hint_len)) = search.take() else {
                    return Ok(PromptLine::Interrupted);
                };
                let entries = history_entries(rl);
                let picked = history_search::pick(entries, &line, prompt, hint_len);
                resume = Some(match picked {
                    history_search::Picked::Entry(entry) => (entry, String::new()),
                    history_search::Picked::Cancelled => split_at_cursor(line, pos),
                    history_search::Picked::Unavailable => {
                        registry::print_colored("ash: history search unavailable", "warning");
                        split_at_cursor(line, pos)
                    }
                });
            }
            Err(rustyline::error::ReadlineError::Eof) => return Ok(PromptLine::Eof),
            Err(error) => return Err(error),
        }
    }
}

// Every in-memory history entry, oldest first.
fn history_entries(rl: &Editor<AeroHelper, DefaultHistory>) -> Vec<String> {
    rl.history().iter().cloned().collect()
}

// Split `text` at `pos`, backing up to the nearest character boundary so the
// pair can be handed back to rustyline safely.
fn split_at_cursor(text: String, pos: usize) -> (String, String) {
    let mut at = pos.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    let right = text[at..].to_string();
    let left = text[..at].to_string();
    (left, right)
}

// Run one command line: pipelines joined by `&&`, `||` and `;`, with each
// stage's redirections applied. Heredoc bodies are read from the lines that
// follow the command, and anything after the last delimiter runs in turn.
// Returns Ok(None) when the line was exit/quit, otherwise the status of the
// last command that ran.
fn run_command_line(line: &str) -> Result<Option<i32>> {
    let mut text = line.to_string();
    loop {
        let (command_line, bodies, leftover) = match split_heredocs(&text) {
            Ok(parts) => parts,
            Err(HereDocError::Unterminated(delimiter)) => {
                registry::print_colored(
                    &format!("ash: syntax error: unterminated heredoc '{delimiter}'"),
                    "error",
                );
                return Ok(Some(2));
            }
            Err(HereDocError::Syntax(message)) => {
                registry::print_colored(&message, "error");
                return Ok(Some(2));
            }
        };
        let status = match run_single_command_line(&command_line, bodies)? {
            Some(code) => code,
            None => return Ok(None), // exit/quit
        };
        match leftover {
            Some(rest) if !rest.trim().is_empty() => text = rest,
            _ => return Ok(Some(status)),
        }
    }
}

fn run_single_command_line(line: &str, bodies: Vec<String>) -> Result<Option<i32>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        // Blank lines are no-ops. `scan` ends a `#` comment at its own
        // newline, so a multi-line block that starts with a comment still
        // runs the commands after it.
        return Ok(Some(0));
    }

    // Expand aliases on the raw line so that operators (`&&`, `||`, `|`,
    // `;`, `<`, `>`) inside an alias body remain operators of the line,
    // rather than literal arguments handed to the first command.
    let line = expand_aliases_on_line(trimmed);
    let parsed = parse_line(&line).and_then(|mut steps| {
        unquote(&mut steps)?;
        attach_heredocs(&mut steps, bodies)?;
        Ok(steps)
    });
    let steps = match parsed {
        Ok(steps) => steps,
        Err(message) => {
            registry::print_colored(&message, "error");
            return Ok(Some(2));
        }
    };
    if steps.is_empty() {
        return Ok(Some(0));
    }

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

// ---------------------------------------------------------------------------
// Alias expansion on the raw line
//
// We expand aliases *before* parsing, on the raw command line, so that
// operators (`&&`, `||`, `|`, `;`, `<`, `>`) that appear inside an alias
// body remain operators of the line — not literal arguments handed to the
// first command.  The body is spliced verbatim into the line and the shell's
// own scanner re-tokenises the whole thing.
//
// We expand only in *command position* (the first word of each command),
// matching every other shell: an alias name used as an ordinary argument is
// not expanded.
//
// The old `alias create` builder re-quoted every token with `shlex::try_quote`,
// which turned shell metacharacters like `&&` into quoted tokens (e.g.
// `'&&'`) and thus destroyed their operator semantics.  Bodies created by that
// builder are un-migrated at expansion time: quoted operators are turned back
// into bare operators before the body is spliced in, so legacy aliases keep
// working.
//
// Aliases created by the *new* builder are stored verbatim (the raw text the
// user typed), so their operators already work without any migration.
// ---------------------------------------------------------------------------

/// Maximum recursive alias-expansion depth.  Recursion lets an alias whose
// body refers to another alias be fully expanded, but we bound it to avoid
// infinite loops on self-referencing aliases.
const MAX_ALIAS_DEPTH: usize = 10;

/// Expand the alias names of `line` into their stored bodies and return the
// resulting line to parse.  Only command-position words are expanded; quoted
// alias names are left alone; bodies are spliced verbatim and re-scanned.
fn expand_aliases_on_line(line: &str) -> String {
    expand_aliases_on_line_inner(line, 0)
}

fn expand_aliases_on_line_inner(line: &str, depth: usize) -> String {
    // Bail out of recursion before we burn the stack on a self-referencing
    // alias (e.g. `alias create x x`).
    if depth >= MAX_ALIAS_DEPTH {
        return line.to_string();
    }

    let Ok(cfg) = config::active_config() else {
        return line.to_string();
    };
    if cfg.aliases.is_empty() {
        return line.to_string();
    }

    // Collect the bodies *before* we start mutating the output, so that
    // recursive expansion sees a stable snapshot of the alias table.
    let aliases: Vec<(String, String)> =
        cfg.aliases.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

    let mut out = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut pos: usize = 0;
    let mut in_command_position = true;

    while pos < chars.len() {
        let ch = chars[pos];

        // Non-word bytes (whitespace, punctuation, operators, quotes,
        // backticks, dollar signs, ...) are copied verbatim.  The scanner
        // already handles these when it re-tokenises the expanded line, so
        // we keep the original characters exactly.
        if ch.is_ascii_whitespace()
            || ch.is_ascii_punctuation()
            || ch == '`'
            || ch == '$'
        {
            out.push(ch);
            if ch == ';' || ch == '|' || ch == '&' || ch == '\n' {
                in_command_position = true;
            } else if ch == '<' || ch == '>' {
                in_command_position = false;
            }
            pos += 1;
            continue;
        }

        // A word: either quoted or unquoted.  Read the whole word (including
        // its quotes, if any), then decide whether to expand it.
        let (word, next_pos) = read_word(&chars, pos);
        pos = next_pos;

        if !in_command_position || word.starts_with('\'') || word.starts_with('"') {
            // Active quotes prevent alias expansion in every shell.
            out.push_str(&word);
            in_command_position = false;
            continue;
        }

        // Command position, unquoted word: try to expand it.
        if let Some(body) = aliases.iter().find(|(name, _)| name == &word).map(|(_, b)| b.as_str())
        {
            // Un-migrate a legacy (re-quoted) body so its operators come back
            // to life before we splice it in as raw text.
            let unmigrated = unmigrate_stored_body(body);
            let expanded_body = expand_aliases_on_line_inner(&unmigrated, depth + 1);
            out.push_str(&expanded_body);
            // After expanding, we consumed the word; the next word is still
            // in command position if the body ended at a command separator,
            // but we let the scanner decide once the line is re-parsed.
            in_command_position = false;
        } else {
            out.push_str(&word);
            in_command_position = false;
        }
    }

    out
}

/// Read one word from `chars` starting at `pos`, returning the word text
// (including surrounding quotes, if any) and the next index after the word.
// Whitespace and metacharacters delimit the word.
fn unmigrate_stored_body(body: &str) -> String {
    let tokens = tokenize_stored_body(body);
    tokens.join(" ")
}

fn tokenize_stored_body(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        if chars[i] == '\'' {
            // Single-quoted token.
            i += 1; // skip opening quote
            let start = i;
            while i < chars.len() && chars[i] != '\'' {
                i += 1;
            }
            let content: String = chars[start..i].iter().collect();
            if i < chars.len() {
                i += 1; // skip closing quote
            }
            if is_operator_content(&content) {
                tokens.push(content); // un-quote: bare operator
            } else {
                tokens.push(format!("'{}'", content)); // keep quoted
            }
        } else if chars[i] == '"' {
            // Double-quoted token (rare in old stored bodies; keep quoted).
            i += 1; // skip opening quote
            let start = i;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2; // skip escaped char
                } else {
                    i += 1;
                }
            }
            let content: String = chars[start..i].iter().collect();
            if i < chars.len() {
                i += 1; // skip closing quote
            }
            tokens.push(format!("\"{}\"", content));
        } else {
            // Unquoted token.
            let start = i;
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            tokens.push(chars[start..i].iter().collect());
        }
    }
    tokens
}

fn is_operator_content(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut has_operator = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            continue;
        }
        if matches!(c, '|' | '&' | ';' | '<' | '>') {
            has_operator = true;
            continue;
        }
        return false;
    }
    has_operator
}

fn read_word(chars: &[char], mut pos: usize) -> (String, usize) {
    if pos >= chars.len() {
        return (String::new(), pos);
    }

    let (quoted, quote_ch) = match chars[pos] {
        '\'' | '"' => (true, chars[pos]),
        _ => (false, '\0'),
    };

    if !quoted {
        // Unquoted word: read until whitespace/metacharacter/backslash-escape.
        let start = pos;
        while pos < chars.len() {
            let ch = chars[pos];
            if ch.is_ascii_whitespace()
                || ch == '&'
                || ch == '|'
                || ch == ';'
                || ch == '<'
                || ch == '>'
                || ch == '#'
                || ch == '`'
            {
                break;
            }
            if ch == '\\' && pos + 1 < chars.len() {
                // Backslash-escaped character: skip both bytes.
                pos += 2;
                continue;
            }
            pos += 1;
        }
        let word: String = chars[start..pos].iter().collect();
        return (word, pos);
    }

    // Quoted word: read until the closing quote, preserving backslashes
    // inside double quotes (they escape the next char), and preserving
    // backslash-quote inside double quotes too.
    let start = pos;
    pos += 1; // skip opening quote
    while pos < chars.len() {
        let ch = chars[pos];
        if ch == quote_ch {
            pos += 1; // closing quote
            break;
        }
        if ch == '\\' && quote_ch == '"' && pos + 1 < chars.len() {
            // Inside double quotes: backslash escapes the next char.
            pos += 2;
            continue;
        }
        if ch == '\\' && quote_ch == '\'' && pos + 1 < chars.len() && chars[pos + 1] == '\'' {
            // Inside single quotes: `\'` is valid in POSIX shells?
            // Actually POSIX single quotes have no escapes.  But we keep
            // the backslash so that the word is preserved verbatim for
            // re-scanning, which matches what the old builder did.
            pos += 2;
            continue;
        }
        pos += 1;
    }

    let word: String = chars[start..pos].iter().collect();
    (word, pos)
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
    HereDoc,                    // << word / <<- word (body collected separately)
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
            RedirKind::HereDoc => "<<",
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
    // Heredoc input, filled in once the body lines have been read.
    body: String,
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
    // Whether the line ended in a `#` comment, which must not let the next
    // line satisfy a redirect that was still waiting for its file.
    let mut commented = false;
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
                '\\' if matches!(chars.get(i + 1), Some('\n' | '\r')) => {
                    // Backslash-newline is a line continuation: drop it (and
                    // a paired `\r` from a CRLF paste) so the command goes on
                    // with the next line.
                    i += if chars.get(i + 1) == Some(&'\r') && chars.get(i + 2) == Some(&'\n')
                    {
                        3
                    } else {
                        2
                    };
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
                '\n' => {
                    // A newline ends the command, so pasted multi-line input
                    // runs line by line. Blank lines and a newline right after
                    // a separator are no-ops; a newline after `|`, `&&`, `||`
                    // or a redirect keeps reading, so a pipeline may wrap.
                    flush_word(&mut word, &mut toks);
                    // A comment can eat a redirect's file. The redirect is
                    // then dangling, so input ends here and reports the
                    // redirect error instead of taking the next line's first
                    // word as its target.
                    if commented
                        && matches!(toks.last(), Some(Tok::Redir(kind)) if kind.takes_target())
                    {
                        break;
                    }
                    let continues = match toks.last() {
                        None | Some(Tok::Semi | Tok::Pipe | Tok::And | Tok::Or) => true,
                        Some(Tok::Redir(kind)) => kind.takes_target(),
                        Some(Tok::Word(_)) => false,
                    };
                    commented = false;
                    if !continues {
                        toks.push(Tok::Semi);
                    }
                    i += 1;
                }
                '#' if word.is_empty() => {
                    // A comment runs to the end of its line only; the newline
                    // arm then separates it from the next command.
                    commented = true;
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                }
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
                    if chars.get(i + 1) == Some(&'<') {
                        // `<<-` strips leading tabs from the body; `<<` keeps
                        // them. The delimiter is the next word, taken by the
                        // parser like a redirect target.
                        let strip_tabs = chars.get(i + 2) == Some(&'-');
                        toks.push(Tok::Redir(RedirKind::HereDoc));
                        i += if strip_tabs { 3 } else { 2 };
                    } else {
                        toks.push(Tok::Redir(RedirKind::In));
                        i += 1;
                    }
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

// ---------------------------------------------------------------------------
// Heredocs (`<< 'EOF'`, `<<-EOF`)
//
// A heredoc's body is not part of its command line: it is read from the lines
// that follow, up to the delimiter line. `split_heredocs` separates the two so
// the line parser only ever sees the command itself; each body is attached to
// its `<<` redirection and later handed to the command as stdin.
// ---------------------------------------------------------------------------

struct HereDocSpec {
    strip_tabs: bool,
    delimiter: String,
}

enum HereDocError {
    Unterminated(String), // still waiting for this delimiter: ask for more input
    Syntax(String),       // `<<` with no delimiter word
}

// The heredocs opened on one command line, in the order they appear.
fn find_heredocs(line: &str) -> Result<Vec<HereDocSpec>, String> {
    let chars: Vec<char> = line.chars().collect();
    let mut specs = Vec::new();
    let mut quote: Option<char> = None;
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        match quote {
            Some(q) => {
                if ch == '\\' && q == '"' && i + 1 < chars.len() {
                    i += 2;
                } else {
                    if ch == q {
                        quote = None;
                    }
                    i += 1;
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    i += 1;
                }
                '\\' if i + 1 < chars.len() => i += 2,
                // An unquoted `#` at the start of a word begins a comment.
                '#' if i == 0 || chars[i - 1].is_whitespace() => break,
                '<' if chars.get(i + 1) == Some(&'<') => {
                    let strip_tabs = chars.get(i + 2) == Some(&'-');
                    i += if strip_tabs { 3 } else { 2 };
                    while chars.get(i).is_some_and(|c| *c == ' ' || *c == '\t') {
                        i += 1;
                    }
                    let (delimiter, next) = read_delimiter(&chars, i);
                    if delimiter.is_empty() {
                        return Err(syntax_error("<<"));
                    }
                    specs.push(HereDocSpec {
                        strip_tabs,
                        delimiter,
                    });
                    i = next;
                }
                _ => i += 1,
            },
        }
    }
    Ok(specs)
}

// Read one heredoc delimiter word, removing quotes and backslashes the way the
// shell would. Quoting the delimiter suppresses expansion in POSIX; Ash does
// not expand body text either way, so the dequoted text is all that matters.
fn read_delimiter(chars: &[char], mut i: usize) -> (String, usize) {
    let mut delimiter = String::new();
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() || matches!(ch, '|' | '&' | ';' | '<' | '>' | '(' | ')') {
            break;
        }
        match ch {
            '\'' | '"' => {
                i += 1;
                while i < chars.len() && chars[i] != ch {
                    if ch == '"' && chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    delimiter.push(chars[i]);
                    i += 1;
                }
                i += 1; // closing quote
            }
            '\\' if i + 1 < chars.len() => {
                delimiter.push(chars[i + 1]);
                i += 2;
            }
            _ => {
                delimiter.push(ch);
                i += 1;
            }
        }
    }
    (delimiter, i)
}

// Separate `text` (a command line followed by the lines typed after it) into
// the command line, one body per heredoc in order, and anything left after the
// last delimiter as further input to run.
fn split_heredocs(text: &str) -> Result<(String, Vec<String>, Option<String>), HereDocError> {
    let (command_line, rest) = text.split_once('\n').unwrap_or((text, ""));
    let specs = find_heredocs(command_line).map_err(HereDocError::Syntax)?;
    if specs.is_empty() {
        return Ok((text.to_string(), Vec::new(), None));
    }

    let mut lines = rest.split('\n');
    let mut bodies = Vec::with_capacity(specs.len());
    for spec in &specs {
        let mut body = String::new();
        loop {
            let Some(raw) = lines.next() else {
                return Err(HereDocError::Unterminated(spec.delimiter.clone()));
            };
            let line = if spec.strip_tabs {
                raw.trim_start_matches('\t')
            } else {
                raw
            };
            if line == spec.delimiter {
                break;
            }
            body.push_str(line);
            body.push('\n');
        }
        bodies.push(body);
    }

    let leftover: Vec<&str> = lines.collect();
    let leftover = (!leftover.is_empty()).then(|| leftover.join("\n"));
    Ok((command_line.to_string(), bodies, leftover))
}

// Attach the collected bodies to their redirections, in the order they were
// read. `find_heredocs` and the parser agree on that order.
fn attach_heredocs(steps: &mut [Step], bodies: Vec<String>) -> Result<(), String> {
    let mut bodies = bodies.into_iter();
    for step in steps.iter_mut() {
        for stage in step.stages.iter_mut() {
            for redir in stage.redirs.iter_mut() {
                if matches!(redir.kind, RedirKind::HereDoc) {
                    redir.body = bodies.next().ok_or_else(|| syntax_error("<<"))?;
                }
            }
        }
    }
    Ok(())
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
                    stage.redirs.push(Redir {
                        kind,
                        target: word,
                        body: String::new(),
                    });
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
                body: String::new(),
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
            RedirKind::HereDoc => slots[0] = FdSlot::File(heredoc_input(&redir.body)?),
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

// A heredoc reaches its command as an ordinary readable file: write the body
// to a temporary file, unlink it while the handle is still open, and rewind.
fn heredoc_input(body: &str) -> Result<std::fs::File, String> {
    use std::io::{Seek, SeekFrom};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "ash-heredoc-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("ash: cannot create heredoc: {error}"))?;
    let _ = std::fs::remove_file(&path);
    file.write_all(body.as_bytes())
        .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .map_err(|error| format!("ash: cannot write heredoc: {error}"))?;
    Ok(file)
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::hint::Hinter;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // Parse `line` fully (quotes removed) and return the words of each stage.
    fn stages(line: &str) -> Vec<Vec<String>> {
        let mut steps = parse_line(line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
        unquote(&mut steps).unwrap_or_else(|e| panic!("{line:?}: {e}"));
        steps
            .into_iter()
            .flat_map(|step| step.stages)
            .map(|stage| stage.words)
            .collect()
    }

    fn words(line: &str) -> Vec<String> {
        stages(line).into_iter().flatten().collect()
    }

    fn step_count(line: &str) -> usize {
        parse_line(line)
            .unwrap_or_else(|e| panic!("{line:?}: {e}"))
            .len()
    }

    fn parse_error(line: &str) -> String {
        parse_line(line)
            .err()
            .unwrap_or_else(|| panic!("{line:?} should not parse"))
    }

    fn unquote_error(line: &str) -> String {
        let mut steps = parse_line(line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
        unquote(&mut steps)
            .err()
            .unwrap_or_else(|| panic!("{line:?} should not unquote"))
    }

    fn joins(line: &str) -> Vec<&'static str> {
        parse_line(line)
            .unwrap_or_else(|e| panic!("{line:?}: {e}"))
            .iter()
            .map(|step| match step.join {
                Some(Join::And) => "&&",
                Some(Join::Or) => "||",
                Some(Join::Semi) => ";",
                None => "end",
            })
            .collect()
    }

    fn redirs(line: &str) -> Vec<(&'static str, String)> {
        let mut steps = parse_line(line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
        unquote(&mut steps).unwrap_or_else(|e| panic!("{line:?}: {e}"));
        steps
            .into_iter()
            .flat_map(|step| step.stages)
            .flat_map(|stage| stage.redirs)
            .map(|redir| (redir.kind.token(), redir.target))
            .collect()
    }

    fn command_arg(args: &[&str]) -> Option<String> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_args(&owned)
            .unwrap_or_else(|e| panic!("{args:?}: {e:#}"))
            .and_then(|shell| shell.command)
    }

    fn args_error(args: &[&str]) -> bool {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_args(&owned).is_err()
    }

    #[test]
    fn a_simple_command_splits_into_words() {
        assert_eq!(words("ls -la /tmp"), v(&["ls", "-la", "/tmp"]));
        assert_eq!(stages("ls -la /tmp").len(), 1);
    }

    #[test]
    fn quotes_group_words_and_are_removed() {
        assert_eq!(
            words(r#"echo "hello world" 'second arg'"#),
            v(&["echo", "hello world", "second arg"])
        );
    }

    #[test]
    fn backslash_escapes_stay_inside_one_word() {
        assert_eq!(words(r"echo hello\ world"), v(&["echo", "hello world"]));
        assert_eq!(words(r#"echo "a\"b""#), v(&["echo", "a\"b"]));
    }

    #[test]
    fn unmatched_quotes_are_reported() {
        assert_eq!(
            unquote_error(r#"echo "oops"#),
            "ash: syntax error: unmatched quote"
        );
        assert_eq!(
            unquote_error("echo 'oops"),
            "ash: syntax error: unmatched quote"
        );
    }

    #[test]
    fn pipelines_stay_in_one_step() {
        assert_eq!(stages("cat f | grep x | head -1").len(), 3);
        assert_eq!(step_count("cat f | grep x"), 1);
        assert_eq!(joins("cat f | grep x"), vec!["end"]);
    }

    #[test]
    fn sequencing_splits_steps_and_records_the_join() {
        assert_eq!(step_count("a && b || c ; d"), 4);
        assert_eq!(joins("a && b || c ; d"), vec!["&&", "||", ";", "end"]);
    }

    #[test]
    fn trailing_semicolons_and_blank_lines() {
        assert_eq!(step_count("echo a;"), 1);
        assert_eq!(step_count("   "), 0);
        assert_eq!(step_count("\t\n"), 0);
    }

    #[test]
    fn missing_commands_around_operators_are_syntax_errors() {
        assert_eq!(parse_error("| cat"), syntax_error("|"));
        assert_eq!(parse_error("cat |"), syntax_error("|"));
        assert_eq!(parse_error("a &&"), syntax_error("&&"));
        assert_eq!(parse_error("a ||"), syntax_error("||"));
        assert_eq!(parse_error("a ;; b"), syntax_error(";"));
    }

    #[test]
    fn redirections_keep_their_kind_and_target() {
        assert_eq!(
            redirs("echo hi > out.txt 2>> err.txt < in.txt &> all.txt"),
            vec![
                (">", "out.txt".to_string()),
                ("2>>", "err.txt".to_string()),
                ("<", "in.txt".to_string()),
                ("&>", "all.txt".to_string()),
            ]
        );
    }

    #[test]
    fn descriptor_duplication_takes_no_file() {
        assert_eq!(redirs("cmd 2>&1"), vec![(">&", String::new())]);
        assert_eq!(words("cmd 2>&1"), v(&["cmd"]));
    }

    #[test]
    fn redirect_targets_are_unquoted() {
        assert_eq!(
            redirs(r#"echo hi > "my file.txt""#),
            vec![(">", "my file.txt".to_string())]
        );
    }

    #[test]
    fn a_redirect_without_a_file_is_a_syntax_error() {
        assert_eq!(parse_error("echo hi >"), syntax_error(">"));
        assert_eq!(parse_error("echo hi 2>"), syntax_error("2>"));
        assert_eq!(parse_error("echo hi <"), syntax_error("<"));
    }

    #[test]
    fn a_comment_ends_the_line_but_not_a_word() {
        assert_eq!(words("echo a # trailing"), v(&["echo", "a"]));
        assert_eq!(step_count("# only a comment"), 0);
        assert_eq!(words("echo a#b"), v(&["echo", "a#b"]));
        assert_eq!(words(r#"echo "a # b""#), v(&["echo", "a # b"]));
        assert_eq!(words(r"echo a\#b"), v(&["echo", "a#b"]));
    }

    #[test]
    fn read_word_stops_at_metacharacters_and_keeps_quotes() {
        let chars: Vec<char> = "foo|bar".chars().collect();
        assert_eq!(read_word(&chars, 0), ("foo".to_string(), 3));
        let quoted: Vec<char> = r#""a b" tail"#.chars().collect();
        assert_eq!(read_word(&quoted, 0), (r#""a b""#.to_string(), 5));
    }

    #[test]
    fn stored_bodies_keep_quotes_except_for_operators() {
        assert_eq!(
            tokenize_stored_body("ls -la '&&' x"),
            vec!["ls", "-la", "&&", "x"]
        );
        assert_eq!(tokenize_stored_body("'echo hi'"), vec!["'echo hi'"]);
        assert_eq!(
            unmigrate_stored_body("'echo' 'a' '&&' 'echo' 'b'"),
            "'echo' 'a' && 'echo' 'b'"
        );
        assert!(is_operator_content("2>&1"));
        assert!(is_operator_content("&&"));
        assert!(!is_operator_content("ls"));
        assert!(!is_operator_content(""));
    }

    #[test]
    fn parse_args_reads_the_c_flag_in_its_forms() {
        assert_eq!(
            command_arg(&["-c", "echo hi"]),
            Some("echo hi".to_string())
        );
        assert_eq!(command_arg(&["-lc", "ls"]), Some("ls".to_string()));
        assert_eq!(command_arg(&["-cecho hi"]), Some("echo hi".to_string()));
        assert_eq!(command_arg(&["-il"]), None);
        assert_eq!(command_arg(&["-"]), None);
    }

    #[test]
    fn parse_args_rejects_unknown_flags_and_missing_c_arguments() {
        assert!(args_error(&["--bogus"]));
        assert!(args_error(&["file.txt"]));
        assert!(args_error(&["-c"]));
    }

    #[test]
    fn split_at_cursor_stays_on_character_boundaries() {
        assert_eq!(
            split_at_cursor("abc".to_string(), 1),
            ("a".to_string(), "bc".to_string())
        );
        assert_eq!(
            split_at_cursor("abc".to_string(), 99),
            ("abc".to_string(), String::new())
        );
        // Byte 2 is inside "é"; the split backs up to the boundary before it.
        assert_eq!(
            split_at_cursor("aé".to_string(), 2),
            ("a".to_string(), "é".to_string())
        );
    }

    fn with_join(join: Option<Join>) -> Step {
        Step {
            stages: Vec::new(),
            join,
        }
    }

    #[test]
    fn step_runs_after_honours_and_or_and_semicolon() {
        assert!(step_runs_after(&with_join(Some(Join::And)), 0));
        assert!(!step_runs_after(&with_join(Some(Join::And)), 1));
        assert!(step_runs_after(&with_join(Some(Join::Or)), 1));
        assert!(!step_runs_after(&with_join(Some(Join::Or)), 0));
        assert!(step_runs_after(&with_join(Some(Join::Semi)), 1));
        assert!(step_runs_after(&with_join(None), 1));
    }

    fn hint_for(line: &str, pos: usize, entries: &[&str]) -> Option<String> {
        let mut history = rustyline::history::MemHistory::new();
        for entry in entries {
            history.add(entry).unwrap();
        }
        let helper = AeroHelper {
            hint_state: HintState::default(),
        };
        helper.hint(line, pos, &rustyline::Context::new(&history))
    }

    #[test]
    fn history_hint_suggests_the_rest_of_the_newest_match() {
        let entries = ["echo old", "echo new"];
        assert_eq!(hint_for("ec", 2, &entries), Some("ho new".into()));
        assert_eq!(hint_for("git st", 6, &["git status"]), Some("atus".into()));
    }

    #[test]
    fn history_hint_stays_out_of_the_way_when_it_has_nothing_to_add() {
        let entries = ["echo hi"];
        assert_eq!(hint_for("", 0, &entries), None);
        assert_eq!(hint_for("zzz", 3, &entries), None);
        assert_eq!(hint_for("ec", 1, &entries), None); // cursor mid-line
        assert_eq!(hint_for("ec", 2, &["echo a\necho b"]), None); // multi-line entry
        assert_eq!(hint_for("ec", 2, &[]), None);
    }

    #[test]
    fn history_hint_skips_exact_and_multi_line_entries() {
        // The newest entry is an exact match, so an older completion wins.
        assert_eq!(
            hint_for("echo hi", 7, &["echo hi there", "echo hi"]),
            Some(" there".into())
        );
        // A multi-line heredoc entry is skipped, not rendered as a hint.
        assert_eq!(
            hint_for("ec", 2, &["echo old", "echo a\necho b"]),
            Some("ho old".into())
        );
    }

    fn set_completions(command: &str, candidates: &[&str]) {
        packages::lua_api::COMPLETIONS.lock().unwrap().insert(
            command.to_string(),
            candidates.iter().map(|s| s.to_string()).collect(),
        );
    }

    fn clear_completions(command: &str) {
        packages::lua_api::COMPLETIONS
            .lock()
            .unwrap()
            .remove(command);
    }

    #[test]
    fn package_completion_hint_previews_the_first_matching_candidate() {
        let command = "preview-demo";
        set_completions(command, &["beta", "alpine", "alpha"]);

        let hint = |line: &str| package_completion_hint(line, line.len());
        assert_eq!(hint("preview-demo al"), Some("pha".into()));
        assert_eq!(hint("preview-demo a"), Some("lpha".into()));
        assert_eq!(hint("preview-demo alpha"), None);
        assert_eq!(hint("preview-demo zz"), None);
        assert_eq!(hint("preview-demo "), None);
        assert_eq!(package_completion_hint("preview-demo al", 14), None);
        assert_eq!(hint("preview-demo"), None);

        clear_completions(command);
    }

    #[test]
    fn package_completion_hint_wins_over_history_but_falls_back_to_it() {
        let command = "preview-priority";
        set_completions(command, &["staging"]);

        let line = "preview-priority st";
        assert_eq!(
            hint_for(line, line.len(), &["preview-priority status"]),
            Some("aging".into())
        );

        let line = "preview-priority x";
        assert_eq!(
            hint_for(line, line.len(), &["preview-priority xyz"]),
            Some("yz".into())
        );

        let line = "preview-priority st";
        assert_eq!(hint_for(line, line.len(), &[]), Some("aging".into()));

        clear_completions(command);
    }

    #[test]
    fn moving_off_the_end_marks_the_hint_hidden() {
        let helper = AeroHelper {
            hint_state: HintState::default(),
        };
        let mut history = rustyline::history::MemHistory::new();
        history.add("echo hi").unwrap();
        let ctx = rustyline::Context::new(&history);

        assert_eq!(helper.hint("ec", 2, &ctx), Some("ho hi".into()));
        assert!(helper.hint_state.shown());
        assert!(helper.highlight_char("ec", 2, CmdKind::MoveCursor));
        assert!(helper.highlight_char("ec", 1, CmdKind::MoveCursor));
        assert!(!helper.hint_state.shown());
    }
}
