use anyhow::{bail, Result};
use aeroshell::commands::{builtins, register_builtins};
use aeroshell::shell::{Flow, Shell, parser};
use aeroshell::{config, packages, registry};
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::history::{DefaultHistory, History, SearchDirection};
use rustyline::{
    Cmd, CompletionType, ConditionalEventHandler, Config as RLConfig, Editor, Event, EventContext,
    EventHandler, KeyCode, KeyEvent, Modifiers, Movement, RepeatCount,
};
use std::borrow::Cow;
use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

mod history_search;
mod syntax_highlight;

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

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ShellArgs {
    command: Option<String>,
    /// With `-c`, the first operand names the shell (`$0`).
    command_name: Option<String>,
    file: Option<String>,
    params: Vec<String>,
    login: bool,
}

fn print_help() {
    println!("Ash (aeroshell) {}", config::aero_version());
    println!("Usage: ash [OPTIONS] [FILE [ARGS...]]");
    println!("  -c <cmd>        run <cmd> and exit");
    println!("  -l, -           login shell (sources /etc/profile)");
    println!("  -i              accepted; forces an interactive shell");
    println!("  --set-default   set Ash as your default login shell (chsh)");
    println!("  --version       print the version");
    println!("  --help          this help");
}

fn parse_args(args: &[String]) -> Result<Option<ShellArgs>> {
    let mut parsed = ShellArgs::default();
    let mut operands: Vec<String> = Vec::new();
    let mut operands_started = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if operands_started {
            operands.push(arg.clone());
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "help" => {
                    print_help();
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
                    operands_started = true;
                    i += 1;
                    continue;
                }
                _ => bail!("ash: unknown option: {arg} (try --help)"),
            }
        }
        if arg == "-h" {
            print_help();
            return Ok(None);
        }
        if arg == "-" {
            parsed.login = true;
            i += 1;
            continue;
        }
        if arg.len() > 1 && arg.starts_with('-') {
            // POSIX login-shell flags (-l, -il, -i, …) plus `c` for -c <cmd>.
            // The characters before a `c` are flags; everything after it is
            // the command itself, exactly like other shells.
            let flags = &arg[1..];
            let (before, after) = match flags.find('c') {
                Some(position) => (&flags[..position], Some(&flags[position + 1..])),
                None => (flags, None),
            };
            for flag in before.chars() {
                match flag {
                    'l' => parsed.login = true,
                    'i' => {}
                    _ => bail!("ash: unknown option: -{flag} (try --help)"),
                }
            }
            if let Some(after) = after {
                if after.is_empty() {
                    i += 1;
                    let Some(command) = args.get(i) else {
                        bail!("ash: option requires an argument: {arg}");
                    };
                    parsed.command = Some(command.clone());
                } else {
                    parsed.command = Some(after.to_string());
                }
            }
            i += 1;
            continue;
        }
        operands_started = true;
        operands.push(arg.clone());
        i += 1;
    }

    let mut operands = operands.into_iter();
    if parsed.command.is_some() {
        // `-c command_string [command_name [argument...]]`
        parsed.command_name = operands.next();
    } else {
        parsed.file = operands.next();
    }
    parsed.params = operands.collect();
    Ok(Some(parsed))
}

fn main() -> Result<()> {
    let cli_args: Vec<String> = std::env::args().skip(1).collect();
    register_builtins();

    let Some(args) = parse_args(&cli_args)? else {
        return Ok(()); // --help / --version / --set-default
    };

    let config = config::load_config()?;
    config::set_config(config);

    // Package failures shouldn't stop the shell from starting; each package
    // reports its own warning.
    if let Err(error) = packages::init() {
        registry::print_colored(&format!("ash: packages unavailable: {error:#}"), "warning");
    }

    let interactive =
        std::io::stdin().is_terminal() && args.command.is_none() && args.file.is_none();
    let mut shell = Shell::new(interactive);
    if args.login {
        // A login shell reads the profiles before anything else, so the
        // profile PATH is what commands see.
        shell.source_login_profiles();
    }

    if let Some(command) = args.command {
        if let Some(name) = args.command_name {
            shell.script_name = name;
        }
        shell.positionals = args.params;
        let flow = shell.run_text(&command);
        exit_shell(&mut shell, flow.status());
    }

    if let Some(file) = args.file {
        let flow = shell.run_file(&file, &args.params);
        exit_shell(&mut shell, flow.status());
    }

    registry::print_colored("Type help for commands.", "success");
    run_repl(&mut shell)
}

/// Run the EXIT trap and hand `status` to the OS, like a shell exiting.
fn exit_shell(shell: &mut Shell, status: i32) -> ! {
    let status = shell.finalize(status);
    std::io::stdout().flush().ok();
    std::process::exit(status);
}

// ---------------------------------------------------------------------------
// The interactive prompt
// ---------------------------------------------------------------------------

/// The result of reading at the interactive prompt.
enum PromptLine {
    Line(String),
    Interrupted,
    Eof,
}

fn run_repl(shell: &mut Shell) -> Result<()> {
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

    // A history written before repeats were filtered (or by another shell)
    // can hold the same command many times; compact it once at startup so the
    // picker and `history` start clean.
    if let Ok(path) = config::history_path() {
        let _ = rl.load_history(&path);
        // Leave a file that exists but cannot be read untouched: rewriting it
        // from memory would drop the entries this shell never saw.
        if let Some(disk) = disk_history(&path) {
            let entries = history_entries(&rl);
            let merged = merge_history(&disk, &entries);
            if merged != disk && !merged.is_empty() {
                replace_history(&mut rl, merged);
                let _ = rl.save_history(&path);
            }
        }
    }

    // One command may arrive over several lines (an `if` block, a heredoc):
    // keep reading while the parser reports incomplete input and run the
    // whole thing at once.
    let mut buffer = String::new();
    loop {
        if let Err(error) = packages::lua_api::run_prompt_hooks() {
            registry::print_colored(&format!("ash: prompt hook failed: {error:#}"), "warning");
        }
        let prompt = if buffer.trim().is_empty() {
            let config_guard = config::active_config()?;
            let template = config_guard.prompt_template.clone();
            config::format_prompt(&template)
        } else {
            config::colorize("> ", "green")
        };

        match read_prompt_line(&mut rl, &prompt, &search_request)? {
            PromptLine::Line(line) => {
                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(&line);
            }
            PromptLine::Interrupted => {
                // Ctrl-C abandons whatever was being typed.
                buffer.clear();
                continue;
            }
            PromptLine::Eof => {
                // Ctrl-D is normal here; EOF from a closed pipe ends input.
                if std::io::stdin().is_terminal() {
                    buffer.clear();
                    continue;
                }
                exit_shell(shell, shell.last_status);
            }
        }

        if buffer.trim().is_empty() {
            buffer.clear();
            continue;
        }

        // The prompt is the interactive path, so it is where the user's own
        // aliases apply; scripts and `-c` never see them.
        let aliases = shell.aliases();
        match parser::parse(&buffer, &aliases) {
            Err(parser::ParseError::Incomplete) => continue,
            Err(parser::ParseError::Message(message)) => {
                registry::print_colored(&message, "error");
                buffer.clear();
            }
            Ok(program) => {
                rl.add_history_entry(&buffer).ok();
                if let Ok(path) = config::history_path() {
                    if compact_history(&mut rl, &path) {
                        // Old copies were removed, so the file has to be
                        // rewritten rather than appended to.
                        let _ = rl.save_history(&path);
                    } else {
                        let _ = rl.append_history(&path);
                    }
                }
                let flow = shell.exec_program(&program);
                if let Flow::Exit(status) = flow {
                    exit_shell(shell, status);
                }
                buffer.clear();
            }
        }
    }
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

// Every entry currently in the history file, repeats included (a reload drops
// consecutive repeats, so that filter is off here). This is the other shells'
// view of history. A missing file reads as empty; `None` means the file exists
// but could not be read, and callers must not rewrite it.
fn disk_history(path: &Path) -> Option<Vec<String>> {
    let config = RLConfig::builder()
        .history_ignore_dups(false)
        .map(|builder| builder.build())
        .unwrap_or_default();
    let mut history = DefaultHistory::with_config(&config);
    match history.load(path) {
        Ok(()) => Some(history.iter().cloned().collect()),
        Err(rustyline::error::ReadlineError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            Some(Vec::new())
        }
        Err(_) => None,
    }
}

// Merge this shell's history with the file's without losing either side: the
// shared prefix comes first, then entries only the file has (another shell
// wrote them since this one loaded), then entries only this shell has (the
// command that was just run). Each command keeps its newest position.
fn merge_history(disk: &[String], session: &[String]) -> Vec<String> {
    let shared = disk
        .iter()
        .zip(session)
        .take_while(|(disk_entry, session_entry)| disk_entry == session_entry)
        .count();
    let mut merged = Vec::with_capacity(disk.len() + session.len());
    merged.extend_from_slice(&session[..shared]);
    merged.extend_from_slice(&disk[shared..]);
    merged.extend_from_slice(&session[shared..]);
    unique_history(&merged)
}

// Keep only the newest copy of each command, preserving history order.
fn unique_history(entries: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(entries.len());
    for entry in entries.iter().rev() {
        if seen.insert(entry.as_str()) {
            unique.push(entry.clone());
        }
    }
    unique.reverse();
    unique
}

// Rebuild the history from `entries`. rustyline can only clear or append, so
// removing an older duplicate means repopulating the list.
fn replace_history(rl: &mut Editor<AeroHelper, DefaultHistory>, entries: Vec<String>) {
    rl.clear_history().ok();
    for entry in entries {
        rl.add_history_entry(entry).ok();
    }
}

// Drop older copies of a command that was run again, so it lands at the end
// of history instead of being recorded another time. Commands other shells
// wrote in the meantime are merged back in. Returns whether anything was
// removed, which means the file has to be rewritten.
fn compact_history(rl: &mut Editor<AeroHelper, DefaultHistory>, path: &Path) -> bool {
    let entries = history_entries(rl);
    let unique = unique_history(&entries);
    if unique.len() == entries.len() {
        return false;
    }
    // Merge the full session list rather than `unique`: its older copies are
    // what line this shell's history up with the file's, so commands another
    // shell added in between keep their place in time. Without a readable
    // file there is nothing to merge, and appending is safe where rewriting
    // is not.
    let Some(disk) = disk_history(path) else {
        return false;
    };
    let merged = merge_history(&disk, &entries);
    replace_history(rl, merged);
    true
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

    fn parse_ok(args: &[&str]) -> ShellArgs {
        parse_args(&v(args))
            .unwrap_or_else(|e| panic!("{args:?}: {e:#}"))
            .unwrap_or_else(|| panic!("{args:?}: help/version"))
    }

    fn args_error(args: &[&str]) -> bool {
        parse_args(&v(args)).is_err()
    }

    #[test]
    fn parse_args_reads_the_c_flag_in_its_forms() {
        assert_eq!(parse_ok(&["-c", "echo hi"]).command, Some("echo hi".into()));
        assert_eq!(parse_ok(&["-lc", "ls"]).command, Some("ls".into()));
        assert_eq!(parse_ok(&["-cecho hi"]).command, Some("echo hi".into()));
        assert_eq!(parse_ok(&["-il"]).command, None);
        assert_eq!(parse_ok(&["-"]).login, true);
        assert_eq!(parse_ok(&["-il"]).login, true);
    }

    #[test]
    fn parse_args_takes_a_script_file_with_positional_arguments() {
        let parsed = parse_ok(&["script.sh", "one", "two words"]);
        assert_eq!(parsed.file, Some("script.sh".into()));
        assert_eq!(parsed.params, ["one", "two words"]);
        assert_eq!(parsed.command, None);
    }

    #[test]
    fn parse_args_names_the_shell_after_a_c_command() {
        let parsed = parse_ok(&["-c", "echo $0 $1", "myshell", "arg"]);
        assert_eq!(parsed.command_name, Some("myshell".into()));
        assert_eq!(parsed.params, ["arg"]);
    }

    #[test]
    fn parse_args_rejects_unknown_flags_and_missing_c_arguments() {
        assert!(args_error(&["--bogus"]));
        assert!(args_error(&["-c"]));
        assert!(args_error(&["-Z"]));
    }

    #[test]
    fn exit_status_follows_posix_argument_rules() {
        // No argument keeps the status of the last command that ran.
        assert_eq!(builtins::exit_status(&[], 7), 7);
        assert_eq!(builtins::exit_status(&["71"], 7), 71);
        assert_eq!(builtins::exit_status(&["-1"], 0), 255);
        assert_eq!(builtins::exit_status(&["nope"], 0), 2);
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

    #[test]
    fn unique_history_keeps_the_newest_copy_in_order() {
        let entries = v(&["make", "git status", "make", "cargo test", "git status"]);
        assert_eq!(unique_history(&entries), ["make", "cargo test", "git status"]);
        assert_eq!(unique_history(&v(&[])), Vec::<String>::new());
        assert_eq!(unique_history(&v(&["ls", "ls", "ls"])), ["ls"]);
    }

    #[test]
    fn merge_history_keeps_commands_another_shell_wrote() {
        // This shell ran `make` again while another one appended `git status`.
        let disk = v(&["make", "make test", "git status"]);
        let session = v(&["make", "make test", "make"]);
        assert_eq!(
            merge_history(&disk, &session),
            ["make test", "git status", "make"]
        );

        // Histories that no longer line up still lose nothing.
        assert_eq!(
            merge_history(&v(&["b", "a"]), &v(&["a", "b", "c"])),
            ["a", "b", "c"]
        );
        assert_eq!(merge_history(&v(&[]), &v(&["ls"])), ["ls"]);
        assert_eq!(merge_history(&v(&["ls"]), &v(&[])), ["ls"]);
    }

    #[test]
    fn disk_history_tells_a_missing_file_from_an_unreadable_one() {
        let dir = std::env::temp_dir().join(format!("ash-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let missing = dir.join("missing.txt");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(disk_history(&missing), Some(Vec::new()));
        // A directory exists but cannot be read as a history file.
        assert_eq!(disk_history(&dir), None);
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
}
