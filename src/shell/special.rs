//! The special builtins: parser- and evaluator-level commands that need the
//! shell's own state. `return`, `break` and `continue` steer the enclosing
//! construct instead of ending the shell; `exec` replaces the process;
//! `export`, `readonly`, `set`, `shift` and friends touch shell variables.

use crate::commands::builtins;
use crate::registry;

use super::{Flow, Shell, is_special_builtin};

/// Signals a trap can be set for.
const SIGNALS: &[(&str, i32)] = &[
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("KILL", 9),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("CHLD", 17),
    ("CONT", 18),
    ("STOP", 19),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
];

fn signal_number(spec: &str) -> Option<i32> {
    if spec == "0" || spec.eq_ignore_ascii_case("EXIT") {
        return Some(0);
    }
    let upper = spec.to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    if let Some((_, number)) = SIGNALS.iter().find(|(signal, _)| *signal == name) {
        return Some(*number);
    }
    spec.parse::<i32>()
        .ok()
        .filter(|number| (0..=64).contains(number))
}

fn signal_name(number: i32) -> String {
    if number == 0 {
        return "EXIT".to_string();
    }
    SIGNALS
        .iter()
        .find(|(_, n)| *n == number)
        .map(|(name, _)| format!("SIG{name}"))
        .unwrap_or_else(|| number.to_string())
}

impl Shell {
    /// Run a special builtin, or `None` when `name` is not one.
    pub(crate) fn run_special_builtin(&mut self, name: &str, args: &[String]) -> Option<Flow> {
        let flow = match name {
            ":" => Flow::Status(0),
            "exit" | "quit" => self.builtin_exit(args),
            "break" => self.builtin_break(args),
            "continue" => self.builtin_continue(args),
            "return" => self.builtin_return(args),
            "shift" => self.builtin_shift(args),
            "set" => self.builtin_set(args),
            "export" => self.builtin_export(args),
            "unset" => self.builtin_unset(args),
            "readonly" => self.builtin_readonly(args),
            "trap" => self.builtin_trap(args),
            "times" => self.builtin_times(),
            "eval" => self.builtin_eval(args),
            "." | "source" => self.builtin_source(args),
            "read" => self.builtin_read(args),
            "command" => self.builtin_command(args),
            "wait" => self.builtin_wait(args),
            // In a pipeline or a command substitution the shell cannot be
            // replaced; run the command as an ordinary child instead.
            "exec" => self.builtin_exec_child(args),
            _ => return None,
        };
        Some(flow)
    }

    fn builtin_exit(&mut self, args: &[String]) -> Flow {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let status = builtins::exit_status(&refs, self.last_status);
        // Only a session needs to be told; a script's `exit` is silent, like
        // every other shell.
        if self.options.interactive {
            registry::print_colored("Exiting Ash...", "warning");
        }
        Flow::Exit(status)
    }

    fn numbered(args: &[String], name: &str) -> Result<u32, Flow> {
        match args.first() {
            None => Ok(1),
            Some(text) => match text.parse::<u32>() {
                Ok(count) if count >= 1 => Ok(count),
                _ => {
                    registry::print_colored(
                        &format!("ash: {name}: {text}: numeric argument required"),
                        "error",
                    );
                    Err(Flow::Status(2))
                }
            },
        }
    }

    fn builtin_break(&mut self, args: &[String]) -> Flow {
        if self.loop_depth == 0 {
            registry::print_colored(
                "ash: break: only meaningful in a for, while or until loop",
                "warning",
            );
            return Flow::Status(0);
        }
        match Self::numbered(args, "break") {
            Ok(count) => Flow::Break(count),
            Err(flow) => flow,
        }
    }

    fn builtin_continue(&mut self, args: &[String]) -> Flow {
        if self.loop_depth == 0 {
            registry::print_colored(
                "ash: continue: only meaningful in a for, while or until loop",
                "warning",
            );
            return Flow::Status(0);
        }
        match Self::numbered(args, "continue") {
            Ok(count) => Flow::Continue(count),
            Err(flow) => flow,
        }
    }

    fn builtin_return(&mut self, args: &[String]) -> Flow {
        if self.call_depth == 0 {
            registry::print_colored(
                "ash: return: can only 'return' from a function or sourced script",
                "error",
            );
            return Flow::Status(1);
        }
        let status = match args.first() {
            None => self.last_status,
            Some(text) => match text.parse::<i32>() {
                Ok(value) => value.rem_euclid(256),
                Err(_) => {
                    registry::print_colored(
                        &format!("ash: return: {text}: numeric argument required"),
                        "error",
                    );
                    return Flow::Return(2);
                }
            },
        };
        Flow::Return(status)
    }

    fn builtin_shift(&mut self, args: &[String]) -> Flow {
        let count = match args.first() {
            None => 1usize,
            Some(text) => match text.parse::<usize>() {
                Ok(value) => value,
                Err(_) => {
                    registry::print_colored(
                        &format!("ash: shift: {text}: numeric argument required"),
                        "error",
                    );
                    return Flow::Status(2);
                }
            },
        };
        if count > self.positionals.len() {
            return Flow::Status(1);
        }
        self.positionals.drain(0..count);
        Flow::Status(0)
    }

    fn builtin_set(&mut self, args: &[String]) -> Flow {
        if args.is_empty() {
            let mut names: Vec<&String> = self.vars.keys().collect();
            names.sort();
            for name in names {
                let value = self.get_var(name).unwrap_or_default();
                println!("{name}={value}");
            }
            return Flow::Status(0);
        }
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            if arg == "--" {
                index += 1;
                break;
            }
            if (arg.starts_with('-') || arg.starts_with('+')) && arg.len() > 1 {
                let enable = arg.starts_with('-');
                for flag in arg.chars().skip(1) {
                    match flag {
                        'e' => self.options.errexit = enable,
                        'u' => self.options.nounset = enable,
                        'x' => self.options.xtrace = enable,
                        // Accepted no-ops: `-f` (noglob), `-C`/`-i` and the
                        // like have no effect here.
                        'f' | 'C' | 'i' | 'a' | 'b' | 'm' | 'h' => {}
                        _ => {
                            registry::print_colored(
                                &format!("ash: set: invalid option: {arg}"),
                                "error",
                            );
                            return Flow::Status(2);
                        }
                    }
                }
                index += 1;
                continue;
            }
            break;
        }
        if index < args.len() {
            self.positionals = args[index..].to_vec();
        }
        Flow::Status(0)
    }

    fn builtin_export(&mut self, args: &[String]) -> Flow {
        if args.iter().any(|arg| arg == "-p") || args.is_empty() {
            let mut names: Vec<&String> = self
                .vars
                .iter()
                .filter(|(_, var)| var.exported)
                .map(|(name, _)| name)
                .collect();
            names.sort();
            for name in names {
                let value = self.get_var(name).unwrap_or_default();
                println!("export {name}={}", quote(&value));
            }
            return Flow::Status(0);
        }
        let mut unexport = false;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "-n" => unexport = true,
                "--" => {
                    index += 1;
                    break;
                }
                _ => break,
            }
            index += 1;
        }
        let mut status = 0;
        for arg in &args[index..] {
            let (name, value) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (arg.as_str(), None),
            };
            if !crate::shell::lexer::is_valid_name(name) {
                registry::print_colored(
                    &format!("ash: export: `{name}': not a valid identifier"),
                    "error",
                );
                status = 1;
                continue;
            }
            let result = match (value, unexport) {
                (Some(value), false) => self.set_var(name, value, true),
                (None, false) => self.export_var(name),
                (_, true) => self.unexport_var(name),
            };
            if let Err(message) = result {
                registry::print_colored(&message, "error");
                status = 1;
            }
        }
        Flow::Status(status)
    }

    fn builtin_unset(&mut self, args: &[String]) -> Flow {
        let mut functions_only = false;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "-f" => functions_only = true,
                "-v" => functions_only = false,
                "--" => {
                    index += 1;
                    break;
                }
                _ => break,
            }
            index += 1;
        }
        for name in &args[index..] {
            if functions_only {
                self.functions.remove(name);
            } else if self.vars.contains_key(name) {
                self.unset_var(name);
            } else {
                // Like bash: without a variable there may be a function.
                self.functions.remove(name);
            }
        }
        Flow::Status(0)
    }

    fn builtin_readonly(&mut self, args: &[String]) -> Flow {
        if args.is_empty() || args[0] == "-p" {
            let mut names: Vec<String> = self.readonly.iter().cloned().collect();
            names.sort();
            for name in names {
                let value = self.get_var(&name).unwrap_or_default();
                println!("readonly {name}={}", quote(&value));
            }
            return Flow::Status(0);
        }
        let mut status = 0;
        for arg in args {
            if arg == "-p" {
                continue;
            }
            let (name, value) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (arg.as_str(), None),
            };
            if !crate::shell::lexer::is_valid_name(name) {
                registry::print_colored(
                    &format!("ash: readonly: `{name}': not a valid identifier"),
                    "error",
                );
                status = 1;
                continue;
            }
            if self.readonly.contains(name) {
                registry::print_colored(
                    &format!("ash: readonly: {name}: readonly variable"),
                    "error",
                );
                status = 1;
                continue;
            }
            if let Some(value) = value {
                if let Err(message) = self.set_var(name, value, false) {
                    registry::print_colored(&message, "error");
                    status = 1;
                    continue;
                }
            } else if self.get_var(name).is_none() {
                let _ = self.set_var(name, "", false);
            }
            self.readonly.insert(name.to_string());
        }
        Flow::Status(status)
    }

    fn builtin_trap(&mut self, args: &[String]) -> Flow {
        if args.is_empty() {
            let mut traps: Vec<(i32, String)> = self
                .traps
                .iter()
                .map(|(signal, command)| (*signal, command.clone()))
                .collect();
            traps.sort_by_key(|(signal, _)| *signal);
            for (signal, command) in traps {
                println!("trap -- {} {}", quote(&command), signal_name(signal));
            }
            return Flow::Status(0);
        }
        if args[0] == "-l" {
            for chunk in SIGNALS.chunks(5) {
                let mut line = String::new();
                for (index, (name, number)) in chunk.iter().enumerate() {
                    let position = index + 1;
                    line.push_str(&format!("{number:>3}) SIG{name:<6}"));
                    if position < chunk.len() {
                        line.push('\t');
                    }
                }
                println!("{line}");
            }
            return Flow::Status(0);
        }
        if args[0] == "-" {
            let specs: Vec<String> = args[1..].to_vec();
            if specs.is_empty() {
                let signals: Vec<i32> = self.traps.keys().copied().collect();
                for signal in signals {
                    self.reset_trap(signal);
                }
            } else {
                for spec in specs {
                    match signal_number(&spec) {
                        Some(signal) => self.reset_trap(signal),
                        None => {
                            registry::print_colored(
                                &format!("ash: trap: {spec}: invalid signal specification"),
                                "error",
                            );
                            return Flow::Status(1);
                        }
                    }
                }
            }
            return Flow::Status(0);
        }

        let command = args[0].clone();
        let specs: Vec<String> = args[1..].to_vec();
        if specs.is_empty() {
            registry::print_colored(
                "ash: trap: usage: trap [-lp] [[arg] signal_spec ...]",
                "error",
            );
            return Flow::Status(2);
        }
        for spec in specs {
            match signal_number(&spec) {
                Some(signal) => self.set_trap(signal, command.clone()),
                None => {
                    registry::print_colored(
                        &format!("ash: trap: {spec}: invalid signal specification"),
                        "error",
                    );
                    return Flow::Status(1);
                }
            }
        }
        Flow::Status(0)
    }

    fn builtin_times(&mut self) -> Flow {
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let ticks = if ticks > 0 { ticks as f64 } else { 100.0 };
        let mut tms: libc::tms = unsafe { std::mem::zeroed() };
        unsafe { libc::times(&mut tms) };
        println!(
            "{} {}",
            format_time(tms.tms_utime as f64 / ticks),
            format_time(tms.tms_stime as f64 / ticks)
        );
        println!(
            "{} {}",
            format_time(tms.tms_cutime as f64 / ticks),
            format_time(tms.tms_cstime as f64 / ticks)
        );
        Flow::Status(0)
    }

    fn builtin_eval(&mut self, args: &[String]) -> Flow {
        let text = args.join(" ");
        if text.trim().is_empty() {
            return Flow::Status(0);
        }
        match super::parser::parse(&text) {
            Ok(program) => self.exec_program(&program),
            Err(super::parser::ParseError::Incomplete) => {
                registry::print_colored("ash: syntax error: unexpected end of file", "error");
                Flow::Status(2)
            }
            Err(super::parser::ParseError::Message(message)) => {
                registry::print_colored(&message, "error");
                Flow::Status(2)
            }
        }
    }

    fn builtin_source(&mut self, args: &[String]) -> Flow {
        let Some(path) = args.first() else {
            registry::print_colored("ash: .: filename argument required", "error");
            return Flow::Status(2);
        };
        let params: Vec<String> = args[1..].to_vec();
        self.source_file(path, &params)
    }

    fn builtin_read(&mut self, args: &[String]) -> Flow {
        let mut raw = false;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "-r" => raw = true,
                "--" => {
                    index += 1;
                    break;
                }
                other if other.starts_with('-') && other.len() > 1 => {
                    registry::print_colored(
                        &format!("ash: read: invalid option: {other}"),
                        "error",
                    );
                    return Flow::Status(2);
                }
                _ => break,
            }
            index += 1;
        }
        let names: Vec<String> = if index < args.len() {
            args[index..].to_vec()
        } else {
            vec!["REPLY".to_string()]
        };
        for name in &names {
            if !crate::shell::lexer::is_valid_name(name) {
                registry::print_colored(
                    &format!("ash: read: `{name}': not a valid identifier"),
                    "error",
                );
                return Flow::Status(2);
            }
        }

        let Some(mut line) = read_line_from_stdin() else {
            for name in &names {
                let _ = self.set_var(name, "", false);
            }
            return Flow::Status(1);
        };
        if !raw {
            while ends_with_unescaped_backslash(&line) {
                let Some(next) = read_line_from_stdin() else {
                    break;
                };
                line.pop();
                line.push_str(&next);
            }
            line = unescape(&line);
        }

        let ifs = self.get_var("IFS").unwrap_or_else(|| " \t\n".to_string());
        let fields = super::expand::split_fields(&line, &ifs);
        let separator = ifs.chars().next().unwrap_or(' ').to_string();
        let mut status = 0;
        for (position, name) in names.iter().enumerate() {
            let value = if position + 1 == names.len() && fields.len() > names.len() {
                fields[position..].join(&separator)
            } else {
                fields.get(position).cloned().unwrap_or_default()
            };
            if let Err(message) = self.set_var(name, &value, false) {
                registry::print_colored(&message, "error");
                status = 1;
            }
        }
        Flow::Status(status)
    }

    fn builtin_command(&mut self, args: &[String]) -> Flow {
        let mut verbose = false;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "-v" | "-V" => verbose = true,
                "-p" => {}
                "--" => {
                    index += 1;
                    break;
                }
                _ => break,
            }
            index += 1;
        }
        let rest = &args[index..];
        if rest.is_empty() {
            return Flow::Status(0);
        }
        if verbose {
            let mut status = 0;
            for name in rest {
                if is_special_builtin(name)
                    || registry::resolve(name).is_some()
                    || self.functions.contains_key(name)
                {
                    println!("{name}");
                } else if let Some(path) = builtins::which_cmd(name) {
                    println!("{path}");
                } else {
                    status = 1;
                }
            }
            return Flow::Status(status);
        }
        self.run_named(&rest[0], &rest[1..])
    }

    fn builtin_wait(&mut self, args: &[String]) -> Flow {
        if args.is_empty() {
            let jobs = std::mem::take(&mut self.jobs);
            let mut status = 0;
            for pid in jobs {
                match self.wait_child(pid, false) {
                    Ok(child_status) => status = child_status,
                    Err(flow) => return flow,
                }
            }
            return Flow::Status(status);
        }
        let mut status = 0;
        for arg in args {
            let Ok(pid) = arg.parse::<i32>() else {
                registry::print_colored(
                    &format!("ash: wait: {arg}: not a pid"),
                    "error",
                );
                status = 2;
                continue;
            };
            if let Some(position) = self.jobs.iter().position(|job| *job == pid) {
                self.jobs.remove(position);
                match self.wait_child(pid, false) {
                    Ok(child_status) => status = child_status,
                    Err(flow) => return flow,
                }
            } else {
                status = 127;
            }
        }
        Flow::Status(status)
    }

    /// `exec` where the shell cannot replace its own process: a pipeline
    /// stage, a subshell or a command substitution. The command still runs and
    /// the surrounding construct stops there, exactly as if that context had
    /// been replaced; only the parent shell keeps going.
    fn builtin_exec_child(&mut self, args: &[String]) -> Flow {
        if args.is_empty() {
            return Flow::Status(0);
        }
        let flow = self.run_named(&args[0], &args[1..]);
        match flow {
            Flow::Exit(status) => Flow::Exit(status),
            other if self.sub_depth > 0 => Flow::Exit(other.status()),
            other => other,
        }
    }
}

fn format_time(seconds: f64) -> String {
    let minutes = (seconds / 60.0).floor() as i64;
    format!("{minutes}m{:.3}s", seconds - minutes as f64 * 60.0)
}

/// Single-quote a string for `trap`/`export` output.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Read one line from fd 0 without buffering, so `while read` sees exactly one
/// line per call even when the input is a pipe shared with other readers.
fn read_line_from_stdin() -> Option<String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) };
        if read < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if read == 0 {
            if bytes.is_empty() {
                return None;
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn ends_with_unescaped_backslash(line: &str) -> bool {
    line.chars().rev().take_while(|c| *c == '\\').count() % 2 == 1
}

/// Remove backslashes, keeping the character they escaped.
fn unescape(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}
