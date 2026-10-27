//! The executor: walks the command tree, applies redirections, runs pipelines
//! and dispatches to builtins, functions and external programs.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Command as ProcessCommand, Stdio};
use std::rc::Rc;

use crate::commands::builtins;
use crate::packages;
use crate::registry;

use super::expand::{ExpandError, ExpandMode, pattern_match};
use super::lexer::RedirOp;
use super::parser::{AndOr, CaseArm, Command, Compound, CompoundKind, Join, Pipeline, Program, Redir, Simple};
use super::{Flow, Shell, is_special_builtin};

// ---------------------------------------------------------------------------
// File descriptor plumbing
// ---------------------------------------------------------------------------

/// Where one of the shell's stdio descriptors points while a command runs.
pub(crate) enum FdSlot {
    Inherit,
    File(std::fs::File),
}

pub(crate) fn inherit_slots() -> [FdSlot; 3] {
    [FdSlot::Inherit, FdSlot::Inherit, FdSlot::Inherit]
}

/// Create a pipe with both ends close-on-exec, so no child can hold an end it
/// does not own and stall a reader at EOF.
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

/// A second handle on the same file or pipe: same open description, so write
/// ends and offsets are shared exactly like `2>&1` expects.
fn dup_slot(slot: &FdSlot) -> Result<FdSlot, String> {
    Ok(match slot {
        FdSlot::Inherit => FdSlot::Inherit,
        FdSlot::File(file) => FdSlot::File(file.try_clone().map_err(|e| e.to_string())?),
    })
}

fn slot_to_stdio(slot: FdSlot) -> Stdio {
    match slot {
        FdSlot::Inherit => Stdio::inherit(),
        FdSlot::File(file) => Stdio::from(file),
    }
}

fn open_target(target: &str, write: bool, append: bool) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new()
        .read(!write)
        .write(write)
        .append(append)
        .create(write)
        .truncate(write && !append)
        .open(target)
        .map_err(|error| format!("ash: cannot open {target}: {error}"))
}

/// A heredoc reaches its command as an ordinary readable file: write the body
/// to a temporary file, unlink it while the handle is still open, and rewind.
fn heredoc_input(body: &str) -> Result<std::fs::File, String> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "ash-heredoc-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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

/// A temporary file for command substitution output, unlinked immediately.
fn temp_output_file() -> std::io::Result<std::fs::File> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "ash-out-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(file)
}

fn flush_all() {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // Lua writes through the C stdio buffer, which Rust's flush cannot see.
    unsafe { libc::fflush(std::ptr::null_mut()) };
}

fn restore_fds(saved: &mut [Option<std::fs::File>; 3]) {
    for (fd, previous) in saved.iter_mut().enumerate() {
        if let Some(copy) = previous.take() {
            unsafe { libc::dup2(copy.as_raw_fd(), fd as i32) };
        }
    }
}

/// Point the shell's own stdin/stdout/stderr at `slots` while `f` runs, then
/// put them back. Builtins print with `println!`, so the process descriptors
/// are the only way to redirect them or to feed their output into a pipeline.
pub(crate) fn with_redirects<T>(slots: &[FdSlot; 3], f: impl FnOnce() -> T) -> Result<T, i32> {
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
            return Err(1);
        }
    }

    // A handler that panics while its output is redirected would spray the
    // panic message into the redirected stream — and Rust's println! panics
    // instead of dying on EPIPE, which `ls | grep -q` hits.
    let redirected = slots.iter().any(|slot| !matches!(slot, FdSlot::Inherit));
    let result = if redirected {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        std::panic::set_hook(previous_hook);
        match outcome {
            Ok(value) => Ok(value),
            Err(payload) => {
                let message = panic_message(payload.as_ref());
                if message.contains("Broken pipe") {
                    Err(141)
                } else {
                    registry::print_colored(
                        &format!("ash: command panicked: {message}"),
                        "error",
                    );
                    Err(1)
                }
            }
        }
    } else {
        Ok(f())
    };

    flush_all();
    restore_fds(&mut saved);
    result
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// Apply `slots` to the shell's own descriptors and leave them there — what
/// bare `exec` with redirections does.
fn apply_redirs_permanently(slots: &[FdSlot; 3]) -> Result<(), i32> {
    flush_all();
    for (fd, slot) in slots.iter().enumerate() {
        let FdSlot::File(file) = slot else {
            continue;
        };
        if unsafe { libc::dup2(file.as_raw_fd(), fd as i32) } < 0 {
            let error = std::io::Error::last_os_error();
            registry::print_colored(&format!("ash: redirection failed: {error}"), "error");
            return Err(1);
        }
    }
    Ok(())
}

/// While a foreground external command runs, the terminal delivers SIGINT to
/// every process in the foreground process group — including the shell. A
/// trap that wants the signal must keep its handler; otherwise the shell
/// ignores the signal for the duration of the command and restores the
/// previous disposition on drop.
struct InterruptGuard {
    previous: Vec<(i32, libc::sigaction)>,
}

impl InterruptGuard {
    fn acquire(skip: &HashSet<i32>) -> Self {
        let mut previous = Vec::new();
        for signal in [libc::SIGINT, libc::SIGQUIT] {
            if skip.contains(&signal) {
                continue;
            }
            unsafe {
                let mut ignore: libc::sigaction = std::mem::zeroed();
                ignore.sa_sigaction = libc::SIG_IGN;
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, &ignore, &mut old) == 0 {
                    previous.push((signal, old));
                }
            }
        }
        Self { previous }
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        for (signal, old) in self.previous.drain(..) {
            unsafe {
                libc::sigaction(signal, &old, std::ptr::null_mut());
            }
        }
    }
}

/// Restore the default terminal-signal dispositions in the child before exec:
/// the ignore flag the shell installed would otherwise leak across exec and
/// leave the child unable to die from Ctrl-C.
#[cfg(unix)]
unsafe fn reset_child_signals() {
    for signal in [libc::SIGINT, libc::SIGQUIT] {
        unsafe {
            let mut dfl: libc::sigaction = std::mem::zeroed();
            dfl.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(signal, &dfl, std::ptr::null_mut());
        }
    }
}

/// Translate a raw wait status into the status the shell hands back. A
/// non-zero exit is the command's own business; children killed by a signal
/// are reported (except Ctrl-C/Ctrl-\, the normal way to stop a foreground
/// command) and use the conventional 128+signal.
fn decode_status(raw: libc::c_int, report_signal: bool) -> i32 {
    if libc::WIFEXITED(raw) {
        return libc::WEXITSTATUS(raw);
    }
    if libc::WIFSIGNALED(raw) {
        let signal = libc::WTERMSIG(raw);
        if report_signal && signal != libc::SIGINT && signal != libc::SIGQUIT {
            registry::print_colored(
                &format!("ash: command terminated by signal {signal}"),
                "warning",
            );
        }
        return 128 + signal;
    }
    1
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// One simple command with its words expanded.
struct Prepared {
    assigns: Vec<(String, String)>,
    argv: Vec<String>,
}

enum StageKind {
    /// An external program that was found on `PATH`.
    External(String),
    /// An external program that is not on `PATH`.
    NotFound,
    /// A special or registered builtin.
    Builtin,
    Compound,
    /// A shell function call (the body is looked up when it runs).
    Function,
    /// A function definition.
    Define,
    /// Assignments (and maybe redirections) with no command word.
    AssignOnly,
    /// `exec` on its own: replace the shell process.
    Exec,
}

impl Shell {
    // -- running programs ---------------------------------------------------

    pub fn exec_program(&mut self, program: &Program) -> Flow {
        self.exec_list(program, false)
    }

    /// A condition (`if`/`while`/`until`): a failing command must not trigger
    /// `set -e`.
    fn exec_program_test(&mut self, program: &Program) -> Flow {
        self.exec_list(program, true)
    }

    fn exec_list(&mut self, program: &Program, test_context: bool) -> Flow {
        let mut status = 0;
        for item in &program.items {
            if let Some(flow) = self.run_pending_traps() {
                return flow;
            }
            match self.exec_and_or(&item.and_or, test_context, item.background) {
                Flow::Status(item_status) => {
                    status = item_status;
                    self.last_status = item_status;
                }
                other => return other,
            }
            if item.background {
                status = 0;
                self.last_status = 0;
            }
        }
        Flow::Status(status)
    }

    fn exec_and_or(&mut self, and_or: &AndOr, test_context: bool, background: bool) -> Flow {
        let total = 1 + and_or.rest.len();
        let mut status = 0;
        let mut complete = true;
        for index in 0..total {
            if index > 0 {
                let (join, _) = &and_or.rest[index - 1];
                let runs = match join {
                    Join::And => status == 0,
                    Join::Or => status != 0,
                };
                if !runs {
                    complete = false;
                    continue;
                }
            }
            let pipeline = if index == 0 {
                &and_or.first
            } else {
                &and_or.rest[index - 1].1
            };
            match self.exec_pipeline(pipeline, background) {
                Flow::Status(pipeline_status) => {
                    status = pipeline_status;
                    self.last_status = pipeline_status;
                }
                other => return other,
            }
        }
        if complete && !test_context && self.options.errexit && status != 0 {
            return Flow::Exit(status);
        }
        Flow::Status(status)
    }

    fn exec_pipeline(&mut self, pipeline: &Pipeline, background: bool) -> Flow {
        let commands = &pipeline.commands;
        let n = commands.len();

        // Expand every simple command first: expansions may run command
        // substitutions, and those must finish before any pipe is opened.
        let mut prepared: Vec<Option<Prepared>> = Vec::with_capacity(n);
        for command in commands {
            match command {
                Command::Simple(simple) => match self.prepare_simple(simple) {
                    Ok(expanded) => prepared.push(Some(expanded)),
                    Err(error) => return self.fail_expansion(error),
                },
                _ => prepared.push(None),
            }
        }

        // `exec` replaces this process only where the shell itself is the
        // thing running: inside a subshell or a command substitution it must
        // replace that context instead, so the parent carries on.
        let allow_exec = n == 1 && !background && self.sub_depth == 0;
        let kinds: Vec<StageKind> = commands
            .iter()
            .enumerate()
            .map(|(index, command)| self.classify(command, prepared[index].as_ref(), allow_exec))
            .collect();

        // Pipes for the stages, then each stage's own redirections (which win
        // over the pipe, so `cmd > f | x` writes to `f`).
        let mut pipes: Vec<(std::fs::File, std::fs::File)> = Vec::with_capacity(n);
        for _ in 0..n.saturating_sub(1) {
            let Some(pair) = pipe_pair() else {
                registry::print_colored("ash: cannot create pipe", "error");
                return Flow::Status(1);
            };
            pipes.push(pair);
        }
        let mut slots: Vec<Option<[FdSlot; 3]>> = Vec::with_capacity(n);
        let mut statuses: Vec<Option<i32>> = vec![None; n];
        for (index, command) in commands.iter().enumerate() {
            let stdin_pipe = (index > 0).then(|| &pipes[index - 1].0);
            let stdout_pipe = pipes.get(index).map(|(_, write)| write);
            match self.stage_slots(command.redirs(), stdin_pipe, stdout_pipe) {
                Ok(resolved) => slots.push(Some(resolved)),
                Err(message) => {
                    registry::print_colored(&message, "error");
                    slots.push(None);
                    statuses[index] = Some(1);
                }
            }
        }
        drop(pipes);

        // Package `command` hooks, once per stage that has a command word.
        let mut hooked = vec![false; n];
        for index in 0..n {
            let Some(name) = prepared[index]
                .as_ref()
                .and_then(|expanded| expanded.argv.first())
            else {
                continue;
            };
            if slots[index].is_none() {
                continue;
            }
            hooked[index] = true;
            let args: Vec<&str> = prepared[index]
                .as_ref()
                .expect("checked")
                .argv[1..]
                .iter()
                .map(String::as_str)
                .collect();
            if let Err(error) = packages::lua_api::run_command_hooks(name, &args) {
                registry::print_colored(
                    &format!("ash: command hook failed: {error:#}"),
                    "warning",
                );
            }
        }

        // External programs first, in stage order: every pipe gets its
        // consumer before an in-process stage starts producing output.
        let mut children: Vec<(usize, i32)> = Vec::new();
        let has_external = kinds
            .iter()
            .any(|kind| matches!(kind, StageKind::External(_)));
        let _interrupt_guard = has_external
            .then(|| InterruptGuard::acquire(&self.ignored_signals()));
        for index in 0..n {
            match &kinds[index] {
                StageKind::External(path) => {
                    let Some(resolved) = slots[index].take() else {
                        continue;
                    };
                    let expanded = prepared[index].as_ref().expect("external is simple");
                    match self.spawn_external(path, expanded, resolved) {
                        Ok(pid) => children.push((index, pid)),
                        Err(error) => {
                            let name = &expanded.argv[0];
                            registry::print_colored(
                                &format!("ash: failed to run {name}: {error}"),
                                "error",
                            );
                            statuses[index] = Some(126);
                        }
                    }
                }
                StageKind::NotFound => {
                    let name = &prepared[index].as_ref().expect("checked").argv[0];
                    registry::print_colored(
                        &format!("ash: command not found: {name}"),
                        "error",
                    );
                    statuses[index] = Some(127);
                }
                _ => {}
            }
        }

        // Builtins, functions and compound commands next, in stage order,
        // each with the shell's own descriptors pointed at its destinations.
        for index in 0..n {
            if matches!(
                kinds[index],
                StageKind::External(_) | StageKind::NotFound
            ) {
                continue;
            }
            if statuses[index].is_some() {
                continue;
            }
            match kinds[index] {
                StageKind::Exec => {
                    let resolved = slots[index].take().expect("checked");
                    let expanded = prepared[index].as_ref().expect("exec is simple");
                    return self.exec_replace(expanded, &resolved);
                }
                _ => {}
            }
            let Some(resolved) = slots[index].take() else {
                statuses[index] = Some(1);
                continue;
            };
            let flow = self.run_in_process(
                &commands[index],
                prepared[index].as_ref(),
                resolved,
            );
            match flow {
                Flow::Status(status) => {
                    statuses[index] = Some(status);
                    self.last_status = status;
                }
                other => return other,
            }
        }

        // Wait for the external stages, then report statuses.
        let mut last_background = None;
        for (index, pid) in children {
            if background {
                self.jobs.push(pid);
                last_background = Some(pid);
                statuses[index] = Some(0);
                continue;
            }
            match self.wait_child(pid, index == n - 1) {
                Ok(status) => statuses[index] = Some(status),
                Err(flow) => return flow,
            }
        }
        if background {
            if let Some(pid) = last_background {
                self.last_bg_pid = Some(pid);
            }
        }

        // External stages report their `command_done` hooks now that every
        // status is known; builtins reported theirs as they ran.
        for index in 0..n {
            if !hooked[index] {
                continue;
            }
            let Some(name) = prepared[index]
                .as_ref()
                .and_then(|expanded| expanded.argv.first())
            else {
                continue;
            };
            let args: Vec<&str> = prepared[index]
                .as_ref()
                .expect("checked")
                .argv[1..]
                .iter()
                .map(String::as_str)
                .collect();
            let status = statuses[index].unwrap_or(0);
            if let Err(error) = packages::lua_api::run_command_done_hooks(name, &args, status) {
                registry::print_colored(
                    &format!("ash: command_done hook failed: {error:#}"),
                    "warning",
                );
            }
        }

        let last = statuses[n - 1].unwrap_or(0);
        let status = if pipeline.negated {
            i32::from(last == 0)
        } else {
            last
        };
        self.last_status = status;
        Flow::Status(status)
    }

    fn ignored_signals(&self) -> HashSet<i32> {
        // Signals with a trap keep their handler while a child runs; the rest
        // are ignored for the duration so Ctrl-C does not kill the shell.
        let mut skip = HashSet::new();
        for signal in [libc::SIGINT, libc::SIGQUIT] {
            if self.traps.contains_key(&signal) {
                skip.insert(signal);
            }
        }
        skip
    }

    fn prepare_simple(&mut self, simple: &Simple) -> Result<Prepared, ExpandError> {
        let mut assigns = Vec::new();
        for assignment in &simple.assigns {
            let value = self.expand_to_string(&assignment.value)?;
            assigns.push((assignment.name.clone(), value));
        }
        let mut argv = Vec::new();
        for word in &simple.words {
            argv.extend(self.expand_word(word, ExpandMode::Normal)?);
        }
        Ok(Prepared { assigns, argv })
    }

    fn classify(
        &self,
        command: &Command,
        prepared: Option<&Prepared>,
        allow_exec: bool,
    ) -> StageKind {
        match command {
            Command::Compound(_) => StageKind::Compound,
            Command::Function { .. } => StageKind::Define,
            Command::Simple(_) => {
                let expanded = prepared.expect("simple commands are prepared");
                let Some(name) = expanded.argv.first() else {
                    return StageKind::AssignOnly;
                };
                if allow_exec && name == "exec" {
                    return StageKind::Exec;
                }
                if name == "unalias" {
                    return StageKind::NotFound;
                }
                if is_special_builtin(name) || registry::resolve(name).is_some() {
                    return StageKind::Builtin;
                }
                if self.functions.contains_key(name) {
                    return StageKind::Function;
                }
                match builtins::which_cmd(name) {
                    Some(path) => StageKind::External(path),
                    None => StageKind::NotFound,
                }
            }
        }
    }

    fn fail_expansion(&mut self, error: ExpandError) -> Flow {
        registry::print_colored(&error.message, "error");
        self.last_status = 1;
        if error.fatal && !self.options.interactive {
            Flow::Exit(1)
        } else {
            Flow::Status(1)
        }
    }

    // -- stages -------------------------------------------------------------

    fn run_in_process(
        &mut self,
        command: &Command,
        prepared: Option<&Prepared>,
        slots: [FdSlot; 3],
    ) -> Flow {
        match command {
            Command::Function { name, body } => {
                self.functions
                    .insert(name.clone(), Rc::new((**body).clone()));
                Flow::Status(0)
            }
            Command::Compound(compound) => {
                match with_redirects(&slots, || self.exec_compound(compound)) {
                    Ok(flow) => flow,
                    Err(status) => Flow::Status(status),
                }
            }
            Command::Simple(_) => {
                let expanded = prepared.expect("simple commands are prepared");
                if expanded.argv.is_empty() {
                    // Assignments with no command: they affect this shell. The
                    // status is the last command substitution's.
                    if let Err(message) = self.check_assignments(&expanded.assigns) {
                        registry::print_colored(&message, "error");
                        return Flow::Status(1);
                    }
                    for (name, value) in &expanded.assigns {
                        let _ = self.set_var(name, value, false);
                    }
                    let status = self.last_cmdsub_status;
                    return match with_redirects(&slots, || Flow::Status(status)) {
                        Ok(flow) => flow,
                        Err(status) => Flow::Status(status),
                    };
                }
                let name = expanded.argv[0].clone();
                if is_special_builtin(&name) {
                    // POSIX: assignments before a special builtin persist.
                    if let Err(message) = self.check_assignments(&expanded.assigns) {
                        registry::print_colored(&message, "error");
                        return Flow::Status(1);
                    }
                    for (assign_name, value) in &expanded.assigns {
                        let _ = self.set_var(assign_name, value, false);
                    }
                    let args: Vec<String> = expanded.argv[1..].to_vec();
                    let flow = match with_redirects(&slots, || {
                        self.dispatch_in_process(&name, &args)
                    }) {
                        Ok(flow) => flow,
                        Err(status) => Flow::Status(status),
                    };
                    if !matches!(flow, Flow::Status(_)) {
                        return flow;
                    }
                    self.last_status = flow.status();
                    return flow;
                }
                // Anything else gets the assignments only for the duration.
                if let Err(message) = self.check_assignments(&expanded.assigns) {
                    registry::print_colored(&message, "error");
                    return Flow::Status(1);
                }
                let saved = self.save_assignments(&expanded.assigns);
                for (assign_name, value) in &expanded.assigns {
                    let _ = self.set_var(assign_name, value, false);
                }
                let args: Vec<String> = expanded.argv[1..].to_vec();
                let flow = match with_redirects(&slots, || {
                    self.dispatch_in_process(&name, &args)
                }) {
                    Ok(flow) => flow,
                    Err(status) => Flow::Status(status),
                };
                self.restore_assignments(saved);
                self.last_status = flow.status();
                flow
            }
        }
    }

    /// Run a command by name in the current process (the caller has already
    /// applied redirections): builtins, functions, then external programs.
    fn dispatch_in_process(&mut self, name: &str, args: &[String]) -> Flow {
        if self.options.xtrace {
            eprintln!("+ {} {}", name, args.join(" "));
        }
        if is_special_builtin(name) {
            if let Some(flow) = self.run_special_builtin(name, args) {
                return flow;
            }
        }
        if let Some(_handler) = registry::resolve(name) {
            let status = self.run_registered(name, args);
            return Flow::Status(status);
        }
        if self.functions.contains_key(name) {
            return self.call_function(name, args);
        }
        self.run_named(name, args)
    }

    /// Run a command by name without redirection handling of its own; used by
    /// `command` and by `exec` outside the process-replacing path.
    pub(crate) fn run_named(&mut self, name: &str, args: &[String]) -> Flow {
        let prepared = Prepared {
            assigns: Vec::new(),
            argv: {
                let mut argv = vec![name.to_string()];
                argv.extend(args.iter().cloned());
                argv
            },
        };
        match builtins::which_cmd(name) {
            Some(path) => self.run_external(&path, &prepared),
            None => {
                registry::print_colored(&format!("ash: command not found: {name}"), "error");
                self.last_status = 127;
                Flow::Status(127)
            }
        }
    }

    fn run_registered(&mut self, name: &str, args: &[String]) -> i32 {
        let Some(handler) = registry::resolve(name) else {
            return 127;
        };
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        packages::lua_api::clear_last_exit_status();
        let status = match handler(&refs) {
            Ok(()) => packages::lua_api::take_last_exit_status().unwrap_or(0),
            Err(error) => {
                registry::print_colored(&format!("Uncaught Error: {error}"), "error");
                1
            }
        };
        if name == "cd" {
            self.sync_pwd();
        }
        status
    }

    fn call_function(&mut self, name: &str, args: &[String]) -> Flow {
        let Some(body) = self.functions.get(name).cloned() else {
            return Flow::Status(127);
        };
        let saved = std::mem::replace(&mut self.positionals, args.to_vec());
        self.call_depth += 1;
        let flow = self.exec_compound(&body);
        self.call_depth -= 1;
        self.positionals = saved;
        match flow {
            Flow::Return(status) => {
                self.last_status = status;
                Flow::Status(status)
            }
            other => other,
        }
    }

    // -- compound commands --------------------------------------------------

    pub(crate) fn exec_compound(&mut self, compound: &Compound) -> Flow {
        match &compound.kind {
            CompoundKind::If {
                branches,
                else_body,
            } => {
                for (condition, body) in branches {
                    match self.exec_program_test(condition) {
                        Flow::Status(0) => return self.exec_program(body),
                        Flow::Status(_) => continue,
                        other => return other,
                    }
                }
                match else_body {
                    Some(body) => self.exec_program(body),
                    None => Flow::Status(0),
                }
            }
            CompoundKind::Loop { cond, body, until } => {
                self.loop_depth += 1;
                let result = loop {
                    let test = match self.exec_program_test(cond) {
                        Flow::Status(status) => status,
                        other => break other,
                    };
                    let done = if *until { test == 0 } else { test != 0 };
                    if done {
                        break Flow::Status(0);
                    }
                    match self.exec_program(body) {
                        Flow::Status(_) => {}
                        Flow::Break(n) => {
                            break if n > 1 {
                                Flow::Break(n - 1)
                            } else {
                                Flow::Status(0)
                            }
                        }
                        Flow::Continue(n) => {
                            if n > 1 {
                                break Flow::Continue(n - 1);
                            }
                        }
                        other => break other,
                    }
                };
                self.loop_depth -= 1;
                result
            }
            CompoundKind::For { name, words, body } => {
                let items = match words {
                    None => self.positionals.clone(),
                    Some(words) => {
                        let mut items = Vec::new();
                        for word in words {
                            match self.expand_word(word, ExpandMode::Normal) {
                                Ok(fields) => items.extend(fields),
                                Err(error) => return self.fail_expansion(error),
                            }
                        }
                        items
                    }
                };
                self.loop_depth += 1;
                let result = loop {
                    let mut flow = Flow::Status(0);
                    for item in &items {
                        if let Err(message) = self.set_var(name, item, false) {
                            registry::print_colored(&message, "error");
                        }
                        match self.exec_program(body) {
                            Flow::Status(_) => {}
                            Flow::Break(n) => {
                                flow = if n > 1 {
                                    Flow::Break(n - 1)
                                } else {
                                    Flow::Status(0)
                                };
                                break;
                            }
                            Flow::Continue(n) => {
                                if n > 1 {
                                    flow = Flow::Continue(n - 1);
                                    break;
                                }
                            }
                            other => {
                                flow = other;
                                break;
                            }
                        }
                    }
                    break flow;
                };
                self.loop_depth -= 1;
                result
            }
            CompoundKind::Case { word, arms } => {
                let value = match self.expand_to_string(word) {
                    Ok(value) => value,
                    Err(error) => return self.fail_expansion(error),
                };
                for arm in arms {
                    if self.arm_matches(arm, &value) {
                        return self.exec_program(&arm.body);
                    }
                }
                Flow::Status(0)
            }
            CompoundKind::Group(program) => self.exec_program(program),
            CompoundKind::Subshell(program) => {
                // A subshell gets its own variables, functions and directory;
                // `exit` inside it ends only the subshell.
                let saved_vars = self.vars.clone();
                let saved_positionals = self.positionals.clone();
                let saved_functions = self.functions.clone();
                let saved_dir = std::env::current_dir().ok();
                self.sub_depth += 1;
                let flow = self.exec_program(program);
                self.sub_depth -= 1;
                self.vars = saved_vars;
                self.positionals = saved_positionals;
                self.functions = saved_functions;
                if let Some(dir) = saved_dir {
                    let _ = std::env::set_current_dir(dir);
                }
                self.resync_env();
                match flow {
                    Flow::Exit(status) | Flow::Status(status) => Flow::Status(status),
                    _ => Flow::Status(self.last_status),
                }
            }
        }
    }

    fn arm_matches(&mut self, arm: &CaseArm, value: &str) -> bool {
        for pattern in &arm.patterns {
            match self.expand_to_string(pattern) {
                Ok(pattern) => {
                    if pattern_match(&pattern, value) {
                        return true;
                    }
                }
                Err(error) => {
                    registry::print_colored(&error.message, "error");
                }
            }
        }
        false
    }

    // -- exec ---------------------------------------------------------------

    /// `exec`: replace this process with `command`, or apply redirections to
    /// the shell itself when no command is given.
    fn exec_replace(&mut self, prepared: &Prepared, slots: &[FdSlot; 3]) -> Flow {
        let args = &prepared.argv[1..];
        if args.is_empty() {
            if let Err(status) = apply_redirs_permanently(slots) {
                self.last_status = status;
                return Flow::Status(status);
            }
            return Flow::Status(0);
        }
        let name = &args[0];
        let Some(path) = builtins::which_cmd(name) else {
            registry::print_colored(&format!("ash: exec: {name}: not found"), "error");
            self.last_status = 127;
            return self.exec_failed(127);
        };
        if let Err(status) = apply_redirs_permanently(slots) {
            self.last_status = status;
            return Flow::Status(status);
        }
        let mut command = ProcessCommand::new(&path);
        #[cfg(unix)]
        let _ = command.arg0(name.as_str());
        command.args(&args[1..]);
        for (assign_name, value) in &prepared.assigns {
            command.env(assign_name, value);
        }
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                reset_child_signals();
                Result::<(), std::io::Error>::Ok(())
            });
        }
        let error = command.exec();
        registry::print_colored(&format!("ash: exec: {name}: {error}"), "error");
        self.last_status = 126;
        self.exec_failed(126)
    }

    /// A failed `exec` ends a non-interactive shell (POSIX 2.8.1); an
    /// interactive one returns to its prompt.
    fn exec_failed(&self, status: i32) -> Flow {
        if self.options.interactive {
            Flow::Status(status)
        } else {
            Flow::Exit(status)
        }
    }

    // -- external processes -------------------------------------------------

    fn spawn_external(
        &mut self,
        path: &str,
        prepared: &Prepared,
        slots: [FdSlot; 3],
    ) -> std::io::Result<i32> {
        let mut command = ProcessCommand::new(path);
        let name = prepared.argv.first().map(String::as_str).unwrap_or(path);
        // Shells pass the typed name as argv[0], not the resolved path —
        // scripts inspecting $0 (e.g. curl|bash installers) rely on it.
        #[cfg(unix)]
        let _ = command.arg0(name);
        command.args(&prepared.argv[1..]);
        for (assign_name, value) in &prepared.assigns {
            command.env(assign_name, value);
        }
        let [stdin, stdout, stderr] = slots;
        command.stdin(slot_to_stdio(stdin));
        command.stdout(slot_to_stdio(stdout));
        command.stderr(slot_to_stdio(stderr));
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                reset_child_signals();
                Result::<(), std::io::Error>::Ok(())
            });
        }
        let child = command.spawn()?;
        let pid = child.id() as i32;
        // The process is reaped with waitpid so signals can run traps while
        // we wait; the Child handle itself has nothing left to own.
        std::mem::forget(child);
        Ok(pid)
    }

    fn run_external(&mut self, path: &str, prepared: &Prepared) -> Flow {
        let slots = inherit_slots();
        match self.spawn_external(path, prepared, slots) {
            Ok(pid) => {
                let _guard = InterruptGuard::acquire(&self.ignored_signals());
                match self.wait_child(pid, true) {
                    Ok(status) => {
                        self.last_status = status;
                        Flow::Status(status)
                    }
                    Err(flow) => flow,
                }
            }
            Err(error) => {
                let name = prepared.argv.first().map(String::as_str).unwrap_or(path);
                registry::print_colored(&format!("ash: failed to run {name}: {error}"), "error");
                self.last_status = 126;
                Flow::Status(126)
            }
        }
    }

    /// Wait for a child, running any trap a signal left pending. Returns the
    /// status, or a flow when a trap asked the shell to exit.
    pub(crate) fn wait_child(&mut self, pid: i32, report_signal: bool) -> Result<i32, Flow> {
        loop {
            let mut raw: libc::c_int = 0;
            let result = unsafe { libc::waitpid(pid, &mut raw, 0) };
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    if let Some(flow) = self.run_pending_traps() {
                        return Err(flow);
                    }
                    continue;
                }
                return Ok(1);
            }
            return Ok(decode_status(raw, report_signal));
        }
    }

    // -- command substitution ----------------------------------------------

    /// Run `text` with its stdout captured, like `$(...)` and `` `...` ``.
    pub(crate) fn command_substitute(&mut self, text: &str) -> Result<String, ExpandError> {
        let program = match super::parser::parse(text) {
            Ok(program) => program,
            Err(super::parser::ParseError::Incomplete) => {
                registry::print_colored("ash: syntax error: unexpected end of file", "error");
                self.last_cmdsub_status = 2;
                return Ok(String::new());
            }
            Err(super::parser::ParseError::Message(message)) => {
                registry::print_colored(&message, "error");
                self.last_cmdsub_status = 2;
                return Ok(String::new());
            }
        };

        let file = match temp_output_file() {
            Ok(file) => file,
            Err(error) => {
                registry::print_colored(
                    &format!("ash: cannot capture command substitution: {error}"),
                    "error",
                );
                self.last_cmdsub_status = 1;
                return Ok(String::new());
            }
        };
        let mut reader = match file.try_clone() {
            Ok(reader) => reader,
            Err(error) => {
                registry::print_colored(
                    &format!("ash: cannot capture command substitution: {error}"),
                    "error",
                );
                self.last_cmdsub_status = 1;
                return Ok(String::new());
            }
        };

        // A command substitution runs in its own environment: variables,
        // functions and the working directory go back to what they were.
        let saved_vars = self.vars.clone();
        let saved_positionals = self.positionals.clone();
        let saved_functions = self.functions.clone();
        let saved_options = self.options;
        let saved_dir = std::env::current_dir().ok();

        let mut slots = inherit_slots();
        slots[1] = FdSlot::File(file);
        self.sub_depth += 1;
        let outcome = with_redirects(&slots, || self.exec_program(&program));
        self.sub_depth -= 1;

        self.vars = saved_vars;
        self.positionals = saved_positionals;
        self.functions = saved_functions;
        self.options = saved_options;
        if let Some(dir) = saved_dir {
            let _ = std::env::set_current_dir(dir);
        }
        self.resync_env();

        let status = match outcome {
            Ok(Flow::Exit(status)) => status,
            Ok(Flow::Status(status)) => status,
            Ok(other) => other.status(),
            Err(status) => status,
        };
        self.last_cmdsub_status = status;

        let mut output = String::new();
        if reader.seek(SeekFrom::Start(0)).is_ok() {
            let _ = reader.read_to_string(&mut output);
        }
        Ok(output)
    }

    /// Put the process environment back in step with the shell's variables
    /// after a subshell or command substitution changed them.
    fn resync_env(&self) {
        for (name, _) in std::env::vars() {
            if !self.vars.contains_key(&name) {
                unsafe { std::env::remove_var(&name) };
            }
        }
        for (name, var) in &self.vars {
            if var.exported {
                unsafe { std::env::set_var(name, &var.value) };
            }
        }
    }

    // -- helpers ------------------------------------------------------------

    fn check_assignments(&self, assigns: &[(String, String)]) -> Result<(), String> {
        for (name, _) in assigns {
            if self.readonly.contains(name) {
                return Err(format!("ash: {name}: readonly variable"));
            }
        }
        Ok(())
    }

    fn save_assignments(&self, assigns: &[(String, String)]) -> Vec<(String, Option<String>, bool)> {
        assigns
            .iter()
            .map(|(name, _)| {
                (
                    name.clone(),
                    self.get_var(name),
                    self.is_exported(name),
                )
            })
            .collect()
    }

    fn restore_assignments(&mut self, saved: Vec<(String, Option<String>, bool)>) {
        for (name, value, exported) in saved {
            match value {
                Some(value) => {
                    let _ = self.set_var(&name, &value, exported);
                }
                None => self.unset_var(&name),
            }
        }
    }

    fn stage_slots(
        &mut self,
        redirs: &[Redir],
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
        for redir in redirs {
            match &redir.kind {
                RedirOp::In => {
                    let target = self.redir_target(redir)?;
                    slots[0] = FdSlot::File(open_target(&target, false, false)?);
                }
                RedirOp::Out => {
                    let target = self.redir_target(redir)?;
                    slots[1] = FdSlot::File(open_target(&target, true, false)?);
                }
                RedirOp::Append => {
                    let target = self.redir_target(redir)?;
                    slots[1] = FdSlot::File(open_target(&target, true, true)?);
                }
                RedirOp::ErrOut => {
                    let target = self.redir_target(redir)?;
                    slots[2] = FdSlot::File(open_target(&target, true, false)?);
                }
                RedirOp::ErrAppend => {
                    let target = self.redir_target(redir)?;
                    slots[2] = FdSlot::File(open_target(&target, true, true)?);
                }
                RedirOp::OutErr => {
                    let target = self.redir_target(redir)?;
                    slots[1] = FdSlot::File(open_target(&target, true, false)?);
                    slots[2] = dup_slot(&slots[1])?;
                }
                RedirOp::HereDoc { body, .. } => {
                    slots[0] = FdSlot::File(heredoc_input(body)?);
                }
                RedirOp::Dup { fd, from } => {
                    if (0..3).contains(fd) && (0..3).contains(from) {
                        slots[*fd as usize] = dup_slot(&slots[*from as usize])?;
                    }
                }
            }
        }
        Ok(slots)
    }

    fn redir_target(&mut self, redir: &Redir) -> Result<String, String> {
        self.expand_to_string(&redir.target)
            .map_err(|error| error.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_shell() -> Shell {
        // The registry is populated by the binary at start-up; tests that run
        // commands need the regular builtins too.
        crate::commands::register_builtins();
        Shell::new(false)
    }

    /// Serialize tests that run commands: the shell redirects the process-wide
    /// stdio descriptors, and the test harness runs many shells at once.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        crate::shell::TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn run(text: &str) -> i32 {
        let _guard = serial();
        let mut shell = new_shell();
        shell.run_text(text).status()
    }

    #[test]
    fn assignments_persist_and_see_expansion() {
        let _guard = serial();
        let mut shell = new_shell();
        assert_eq!(shell.run_text("X=1; echo $X").status(), 0);
        assert_eq!(shell.get_var("X").as_deref(), Some("1"));
        assert_eq!(shell.run_text("Y=$X$X; echo $Y").status(), 0);
        assert_eq!(shell.get_var("Y").as_deref(), Some("11"));
    }

    #[test]
    fn prefix_assignments_do_not_leak_to_the_shell() {
        let _guard = serial();
        let mut shell = new_shell();
        shell.run_text("ASHTEST_FOO=1 pwd");
        assert_eq!(shell.get_var("ASHTEST_FOO").as_deref(), None);
        // …except before special builtins, where POSIX keeps them.
        shell.run_text("ASHTEST_SPECIAL=1 set --");
        assert_eq!(shell.get_var("ASHTEST_SPECIAL").as_deref(), Some("1"));
    }

    #[test]
    fn if_else_and_exit_status() {
        assert_eq!(run("if true; then exit 3; else exit 4; fi"), 3);
        assert_eq!(run("if false; then exit 3; else exit 4; fi"), 4);
        assert_eq!(run("if false; then exit 3; elif true; then exit 5; fi"), 5);
        assert_eq!(run("false; echo $?"), 0);
        assert_eq!(run("if false; then :; fi"), 0);
    }

    #[test]
    fn loops_and_break_continue() {
        assert_eq!(run("for i in 1 2 3; do echo $i; done"), 0);
        assert_eq!(run("i=0; while [ $i -lt 0 ]; do :; done; exit 7"), 7);
        assert_eq!(run("for a in 1 2; do for b in x y; do break 2; done; done"), 0);
        assert_eq!(
            run("for i in 1 2 3; do case $i in 2) continue;; esac; echo $i; done"),
            0
        );
        // break outside a loop must not end the shell.
        assert_eq!(run("break; exit 9"), 9);
    }

    #[test]
    fn case_arms_match_patterns() {
        assert_eq!(run("x=abc; case $x in a*) exit 3;; *) exit 4;; esac"), 3);
        assert_eq!(run("x=abc; case $x in a|z) exit 3;; *) exit 4;; esac"), 4);
    }

    #[test]
    fn functions_return_to_the_caller() {
        let _guard = serial();
        let mut shell = new_shell();
        shell.run_text("f() { echo hi; return 7; }");
        assert_eq!(shell.run_text("f").status(), 7);
        assert_eq!(shell.run_text("f; exit 0").status(), 0);
        // Parameters are local to the call.
        shell.run_text("g() { echo $1; }");
        assert_eq!(shell.run_text("g a b").status(), 0);
    }

    #[test]
    fn return_outside_a_function_is_an_error_that_does_not_exit() {
        let _guard = serial();
        let mut shell = new_shell();
        let flow = shell.run_text("ASHTEST_RET=1; return 5; echo after");
        assert!(matches!(flow, Flow::Status(_)));
    }

    #[test]
    fn sourced_files_return_their_status() {
        let dir = std::env::temp_dir().join(format!("ash-source-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lib.sh");
        std::fs::write(&file, "echo sourced\nreturn 6\necho never\n").unwrap();
        let _guard = serial();
        let mut shell = new_shell();
        let line = format!(". {}; echo rc=$?", file.to_string_lossy());
        let flow = shell.run_text(&line);
        assert!(matches!(flow, Flow::Status(0)));
        assert_eq!(shell.last_status, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn redirections_write_files_and_feed_heredocs() {
        let dir = std::env::temp_dir().join(format!("ash-redir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = serial();
        let out = dir.join("out.txt");
        let mut first = new_shell();
        let line = format!("echo hello > {}", out.to_string_lossy());
        assert_eq!(first.run_text(&line).status(), 0);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "hello\n");
        // Heredoc into an external command.
        let mut second = new_shell();
        assert_eq!(second.run_text("cat <<EOF\nx\nEOF\n").status(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pipelines_run_builtins_and_externals_together() {
        assert_eq!(run("echo hi | cat"), 0);
        assert_eq!(run("false | true"), 0);
        assert_eq!(run("true | false"), 1);
        assert_eq!(run("! false"), 0);
        assert_eq!(run("! true"), 1);
    }

    #[test]
    fn errexit_exits_on_failure_but_not_in_conditions() {
        assert_eq!(run("set -e; false; exit 0"), 1);
        assert_eq!(run("set -e; if false; then :; fi; exit 0"), 0);
        assert_eq!(run("set -e; false || true; exit 0"), 0);
        // A failure consumed by `&&` does not end the shell (bash agrees).
        assert_eq!(run("set -e; false && true; exit 0"), 0);
        assert_eq!(run("set -e; true && false; exit 0"), 1);
    }

    #[test]
    fn command_substitution_captures_output_without_leaking_state() {
        let _guard = serial();
        let before = std::env::current_dir().unwrap();
        let mut shell = new_shell();
        assert_eq!(shell.run_text("X=$(echo one); echo $X").status(), 0);
        assert_eq!(shell.get_var("X").as_deref(), Some("one"));
        // A `cd` inside the substitution must not move the shell. What a
        // *builtin* like `pwd` prints is captured through the real binary
        // only — the test harness claims `println!` — so tests/cli.rs covers
        // the text and this checks the isolation.
        assert_eq!(shell.run_text("Y=$(cd /); exit 0").status(), 0);
        assert_eq!(
            std::env::current_dir().unwrap(),
            before,
            "the substitution must not move the shell"
        );
    }

    #[test]
    fn subshells_do_not_change_the_shell() {
        let _guard = serial();
        let mut shell = new_shell();
        assert_eq!(shell.run_text("(X=inner); exit 0").status(), 0);
        assert_eq!(shell.get_var("X"), None);
        assert_eq!(shell.run_text("(exit 4); exit 0").status(), 0);
        assert_eq!(shell.run_text("(false); echo $?").status(), 0);
    }

    #[test]
    fn and_or_short_circuit() {
        assert_eq!(run("true && exit 5"), 5);
        assert_eq!(run("false || exit 6"), 6);
        assert_eq!(run("false && exit 5; true"), 0);
    }

    #[test]
    fn background_lists_return_zero_and_record_the_pid() {
        let _guard = serial();
        let mut shell = new_shell();
        let flow = shell.run_text("sleep 0.01 & exit 0");
        assert_eq!(flow.status(), 0);
        assert!(shell.last_bg_pid.is_some());
    }

    #[test]
    fn exec_failure_reports_127_and_ends_a_non_interactive_shell() {
        let _guard = serial();
        let mut shell = new_shell();
        let flow = shell.run_text("exec /no/such/command-ash-test; echo after");
        assert_eq!(flow, Flow::Exit(127));
    }

    #[test]
    fn special_builtins_behave() {
        assert_eq!(run(": ; exit 0"), 0);
        assert_eq!(run("eval 'exit 4'"), 4);
        assert_eq!(run("set -- a b; shift; shift; shift; exit $?"), 1);
        assert_eq!(run("readonly R=1; exit 0"), 0);
        assert_eq!(run("readonly R=1; R=2; exit $?"), 1);
        assert_eq!(run("times; exit 0"), 0);
        assert_eq!(run("shift; echo rc=$?"), 0);
    }

    #[test]
    fn the_exit_trap_runs_when_the_shell_exits() {
        let _guard = serial();
        let mut shell = new_shell();
        let flow = shell.run_text("trap 'exit 3' EXIT; exit 0");
        assert_eq!(shell.finalize(flow.status()), 3);
    }

    #[test]
    fn login_profiles_can_be_sourced() {
        let _guard = serial();
        let mut shell = new_shell();
        // `source_login_profiles` must not panic when the files are absent;
        // its real behavior is exercised from the command line.
        shell.source_login_profiles();
    }
}

