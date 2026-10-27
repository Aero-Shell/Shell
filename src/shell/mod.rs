//! Ash's shell engine: lexer, parser, expansion and executor.
//!
//! Both surfaces — the interactive REPL and the non-interactive one
//! (`ash -c '…'`, `ash file.sh`, login profiles) — drive the same [`Shell`].
//! The only differences are explicit and small: an interactive shell keeps
//! looping at the prompt, and a failed `exec` ends a non-interactive shell
//! (POSIX 2.8.1) instead of returning to the prompt.

pub mod alias;
pub mod exec;
pub mod expand;
pub mod lexer;
pub mod parser;
pub mod special;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::registry;
use parser::{Compound, ParseError};

/// The builtins POSIX calls "special": they are recognized before external
/// commands and are part of the shell itself rather than the command registry.
/// `read`, `command` and `wait` are not special per POSIX but are shell-level
/// builtins all the same.
pub const SPECIAL_BUILTINS: &[&str] = &[
    ":", ".", "source", "eval", "exec", "export", "readonly", "set", "shift", "trap", "times",
    "return", "break", "continue", "unset", "read", "command", "exit", "quit", "wait",
];

pub fn is_special_builtin(name: &str) -> bool {
    SPECIAL_BUILTINS.contains(&name)
}

// ---------------------------------------------------------------------------
// Control flow
// ---------------------------------------------------------------------------

/// What running a command did. Anything other than `Status` unwinds: `break`
/// and `continue` travel out of loops, `return` out of functions and sourced
/// files, and `exit` out of the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Status(i32),
    Break(u32),
    Continue(u32),
    Return(i32),
    Exit(i32),
}

impl Flow {
    pub fn status(self) -> i32 {
        match self {
            Flow::Status(status)
            | Flow::Return(status)
            | Flow::Exit(status) => status,
            Flow::Break(_) | Flow::Continue(_) => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Shell state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
pub struct Options {
    pub errexit: bool,
    pub nounset: bool,
    pub xtrace: bool,
    pub interactive: bool,
}

impl Options {
    /// The letters `$-` reports.
    pub fn flags(&self) -> String {
        let mut flags = String::new();
        if self.errexit {
            flags.push('e');
        }
        if self.interactive {
            flags.push('i');
        }
        if self.nounset {
            flags.push('u');
        }
        if self.xtrace {
            flags.push('x');
        }
        flags
    }
}

#[derive(Clone)]
struct Var {
    value: String,
    exported: bool,
}

pub struct Shell {
    vars: HashMap<String, Var>,
    pub readonly: HashSet<String>,
    pub positionals: Vec<String>,
    pub functions: HashMap<String, Rc<Compound>>,
    /// Signal traps by signal number; key 0 is the EXIT trap.
    pub traps: HashMap<i32, String>,
    pub options: Options,
    pub last_status: i32,
    /// Status of the last command substitution (what an assignment-only
    /// command reports).
    pub last_cmdsub_status: i32,
    pub last_bg_pid: Option<i32>,
    /// `$0`
    pub script_name: String,
    /// Background process ids, for `wait`.
    pub jobs: Vec<i32>,
    /// Nesting depth of loops, so `break`/`continue` know when they are
    /// meaningless.
    pub loop_depth: u32,
    /// Nesting depth of functions and sourced files, for `return`.
    pub call_depth: u32,
    /// Nesting depth of command substitutions / subshells, where `exec` must
    /// not replace the shell process.
    pub sub_depth: u32,
    /// A `Flow` produced while waiting for a child (a trap asked to exit).
    pub deferred: Option<Flow>,
}

/// Signals whose handlers have fired, waiting to run their trap commands.
/// Kept as a bitmask so the handler is async-signal-safe.
static PENDING_TRAPS: AtomicU64 = AtomicU64::new(0);

/// Tests that run commands (and so may redirect or capture the process-wide
/// stdio descriptors) take this lock. The shell itself is single-threaded;
/// only the test harness runs many shells at once.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

extern "C" fn trap_signal_handler(signal: libc::c_int) {
    if (1..63).contains(&signal) {
        PENDING_TRAPS.fetch_or(1u64 << signal, Ordering::SeqCst);
    }
}

impl Shell {
    pub fn new(interactive: bool) -> Shell {
        let mut vars = HashMap::new();
        for (name, value) in std::env::vars() {
            vars.insert(
                name,
                Var {
                    value,
                    exported: true,
                },
            );
        }
        let script_name = std::env::args()
            .next()
            .and_then(|arg0| {
                std::path::Path::new(&arg0)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "ash".to_string());
        let mut shell = Shell {
            vars,
            readonly: HashSet::new(),
            positionals: Vec::new(),
            functions: HashMap::new(),
            traps: HashMap::new(),
            options: Options {
                interactive,
                ..Options::default()
            },
            last_status: 0,
            last_cmdsub_status: 0,
            last_bg_pid: None,
            script_name,
            jobs: Vec::new(),
            loop_depth: 0,
            call_depth: 0,
            sub_depth: 0,
            deferred: None,
        };
        if let Ok(cwd) = std::env::current_dir() {
            let cwd = cwd.to_string_lossy().into_owned();
            if shell.get_var("PWD").is_none() {
                let _ = shell.set_var("PWD", &cwd, true);
            }
        }
        shell
    }

    // -- variables ----------------------------------------------------------

    pub fn get_var(&self, name: &str) -> Option<String> {
        self.vars.get(name).map(|var| var.value.clone())
    }

    pub fn is_exported(&self, name: &str) -> bool {
        self.vars.get(name).is_some_and(|var| var.exported)
    }

    /// Set a shell variable. Exported variables are mirrored into the process
    /// environment so children inherit them.
    pub fn set_var(&mut self, name: &str, value: &str, export: bool) -> Result<(), String> {
        if self.readonly.contains(name) {
            return Err(format!("ash: {name}: readonly variable"));
        }
        let already_exported = self.vars.get(name).is_some_and(|var| var.exported);
        let exported = export || already_exported;
        self.vars.insert(
            name.to_string(),
            Var {
                value: value.to_string(),
                exported,
            },
        );
        if exported {
            unsafe { std::env::set_var(name, value) };
        }
        Ok(())
    }

    pub fn export_var(&mut self, name: &str) -> Result<(), String> {
        let value = self.get_var(name).unwrap_or_default();
        self.set_var(name, &value, true)
    }

    pub fn unexport_var(&mut self, name: &str) -> Result<(), String> {
        if self.readonly.contains(name) {
            return Err(format!("ash: {name}: readonly variable"));
        }
        let value = self.get_var(name).unwrap_or_default();
        self.vars.insert(
            name.to_string(),
            Var {
                value: value.clone(),
                exported: false,
            },
        );
        unsafe { std::env::remove_var(name) };
        Ok(())
    }

    pub fn unset_var(&mut self, name: &str) {
        if self.readonly.contains(name) {
            registry::print_colored(
                &format!("ash: {name}: readonly variable"),
                "error",
            );
            return;
        }
        self.vars.remove(name);
        unsafe { std::env::remove_var(name) };
    }

    /// Keep `PWD`/`OLDPWD` in step with the process directory, as every shell
    /// does after `cd`.
    pub fn sync_pwd(&mut self) {
        let Ok(cwd) = std::env::current_dir() else {
            return;
        };
        let cwd = cwd.to_string_lossy().into_owned();
        if self.get_var("PWD").as_deref() == Some(cwd.as_str()) {
            return;
        }
        if let Some(previous) = self.get_var("PWD") {
            let _ = self.set_var("OLDPWD", &previous, true);
        }
        let _ = self.set_var("PWD", &cwd, true);
    }

    // -- traps --------------------------------------------------------------

    pub fn set_trap(&mut self, signal: i32, command: String) {
        self.traps.insert(signal, command.clone());
        if signal == 0 {
            return; // EXIT is run by finalize()
        }
        #[cfg(unix)]
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = if command.is_empty() {
                libc::SIG_IGN
            } else {
                trap_signal_handler as *const () as usize
            };
            action.sa_flags = 0;
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }

    pub fn reset_trap(&mut self, signal: i32) {
        self.traps.remove(&signal);
        if signal == 0 {
            return;
        }
        #[cfg(unix)]
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            action.sa_flags = 0;
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }

    /// Run the trap commands of any signal that fired since the last check.
    /// Returns a flow to unwind with when the trap ended the shell.
    pub fn run_pending_traps(&mut self) -> Option<Flow> {
        let pending = PENDING_TRAPS.swap(0, Ordering::SeqCst);
        if pending == 0 {
            return None;
        }
        for signal in 1..63 {
            if pending & (1u64 << signal) == 0 {
                continue;
            }
            let Some(command) = self.traps.get(&signal).cloned() else {
                continue;
            };
            if command.is_empty() {
                continue;
            }
            match self.run_text(&command) {
                Flow::Status(_) => {}
                other => return Some(other),
            }
        }
        None
    }

    /// Run the EXIT trap (if any) and return the status the shell ends with.
    pub fn finalize(&mut self, status: i32) -> i32 {
        if let Some(command) = self.traps.remove(&0) {
            if !command.is_empty() {
                // `$?` inside the EXIT trap is the status the shell is
                // leaving with, exactly as in bash and dash.
                self.last_status = status;
                match self.run_text(&command) {
                    Flow::Exit(exit) => return exit,
                    _ => {}
                }
            }
        }
        status
    }

    // -- entry points -------------------------------------------------------

    /// Parse and run `text` in this shell.
    pub fn run_text(&mut self, text: &str) -> Flow {
        match parser::parse(text) {
            Ok(program) => self.exec_program(&program),
            Err(ParseError::Incomplete) => {
                registry::print_colored(
                    "ash: syntax error: unexpected end of file",
                    "error",
                );
                self.last_status = 2;
                Flow::Status(2)
            }
            Err(ParseError::Message(message)) => {
                registry::print_colored(&message, "error");
                self.last_status = 2;
                Flow::Status(2)
            }
        }
    }

    /// Run a script file: the file's arguments become the positional
    /// parameters and `$0` is the file, like any shell.
    pub fn run_file(&mut self, path: &str, args: &[String]) -> Flow {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                registry::print_colored(&format!("ash: {path}: {error}"), "error");
                self.last_status = 127;
                return Flow::Status(127);
            }
        };
        let saved_name = std::mem::replace(&mut self.script_name, path.to_string());
        let saved_positionals = std::mem::replace(&mut self.positionals, args.to_vec());
        self.call_depth += 1;
        let flow = self.run_text(&text);
        self.call_depth -= 1;
        self.positionals = saved_positionals;
        self.script_name = saved_name;
        match flow {
            Flow::Return(status) => Flow::Status(status),
            other => other,
        }
    }

    /// Source a file into this shell: `. file [args]`. Positional parameters
    /// are set for the duration, `$0` stays put, and `return` ends the file.
    pub(crate) fn source_file(&mut self, path: &str, args: &[String]) -> Flow {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                registry::print_colored(&format!("ash: .: {path}: {error}"), "error");
                self.last_status = 1;
                return Flow::Status(1);
            }
        };
        let saved_positionals = if args.is_empty() {
            None
        } else {
            Some(std::mem::replace(&mut self.positionals, args.to_vec()))
        };
        self.call_depth += 1;
        let flow = self.run_text(&text);
        self.call_depth -= 1;
        if let Some(saved) = saved_positionals {
            self.positionals = saved;
        }
        match flow {
            Flow::Return(status) => Flow::Status(status),
            other => other,
        }
    }

    /// Source the login profiles, as a login shell does before reading
    /// commands: `/etc/profile`, then `~/.profile` when present.
    pub fn source_login_profiles(&mut self) {
        let mut profiles = vec![std::path::PathBuf::from("/etc/profile")];
        if let Some(home) = dirs::home_dir() {
            profiles.push(home.join(".profile"));
        }
        for profile in profiles {
            if !profile.is_file() {
                continue;
            }
            let path = profile.to_string_lossy().into_owned();
            match self.source_file(&path, &[]) {
                Flow::Status(_) => {}
                Flow::Exit(status) => {
                    // An exit in a profile ends the login shell.
                    let status = self.finalize(status);
                    std::io::Write::flush(&mut std::io::stdout()).ok();
                    std::process::exit(status);
                }
                _ => {}
            }
        }
    }
}
