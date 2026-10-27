//! End-to-end tests for the surfaces a script, a session manager or a login
//! profile drives: `ash -c '…'`, `ash script.sh …` and `ash -l -c '…'`.
//!
//! The unit tests inside `src/shell` cover the evaluator in-process; anything
//! a descriptor carries belongs here instead, because a capture taken
//! in-process shares that descriptor with the test harness's own output — and
//! the harness also claims `println!`, so what a builtin prints never reaches
//! it. These tests run the real binary and read its actual stdout, stderr and
//! exit status.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Outcome {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str]) -> Outcome {
    run_with_env(args, &[])
}

fn run_with_env(args: &[&str], env: &[(&str, &str)]) -> Outcome {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aeroshell"));
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("run ash");
    Outcome {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn shell(command: &str) -> Outcome {
    run(&["-c", command])
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ash-cli-{tag}-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).expect("write script");
    path
}

/// A script the shell can `exec`: [`write_script`] plus the execute bit.
fn write_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = write_script(dir, name, body);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod +x");
    }
    path
}

/// The login user's name, read the way the shell reads `$USER`.
fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| {
        let out = Command::new("id").arg("-un").output().expect("id -un");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    })
}

#[test]
fn exec_replaces_the_shell_with_the_command() {
    // The exact shape gnome-session uses to re-exec itself through a login
    // shell: `$SHELL -c "exec '…' --no-reexec"`.
    let outcome = shell("exec '/bin/echo' --no-reexec");
    assert_eq!(outcome.status, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, "--no-reexec\n");

    // Nothing after a successful `exec` runs, because the shell is gone.
    let outcome = shell("exec /bin/echo replaced; echo after");
    assert_eq!(outcome.stdout, "replaced\n");
    assert_eq!(outcome.status, 0);
}

#[test]
fn exec_inside_a_subshell_or_substitution_replaces_only_that_context() {
    // The captured text comes back to the parent, which keeps running.
    let outcome = shell("printf '%s/' \"$(exec /bin/echo sub)\"; echo ok");
    assert_eq!(outcome.stdout, "sub/ok\n", "stderr: {}", outcome.stderr);
    assert_eq!(outcome.status, 0);

    // The rest of the subshell does not run; the parent does.
    let outcome = shell("(exec /bin/echo a; /bin/echo never); /bin/echo after");
    assert_eq!(outcome.stdout, "a\nafter\n");
}

#[test]
fn exec_reports_its_failure_and_ends_a_non_interactive_shell() {
    let outcome = shell("exec /no/such/ash-test-binary; echo after");
    assert_eq!(outcome.status, 127, "stderr: {}", outcome.stderr);
    assert!(!outcome.stdout.contains("after"));
    // `exec` keeps the wording it has always used for an operand it cannot
    // find; bash words a missing path "No such file or directory".
    assert!(
        outcome
            .stderr
            .contains("ash: exec: /no/such/ash-test-binary: not found"),
        "{}",
        outcome.stderr
    );
}

#[test]
fn exec_with_only_redirections_applies_them_to_the_shell() {
    let dir = temp_dir("exec-redir");
    let target = dir.join("out.txt");
    let line = format!("exec > {}; echo redirected", target.display());
    let outcome = shell(&line);
    assert_eq!(outcome.status, 0, "stderr: {}", outcome.stderr);
    assert!(outcome.stdout.is_empty(), "stdout: {}", outcome.stdout);
    assert_eq!(fs::read_to_string(&target).unwrap(), "redirected\n");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn exec_option_terminator_and_operand_statuses() {
    // `--` ends `exec`'s own options, and the operand keeps its spaces: the
    // argument list must arrive un-split, command and flags alike.
    let dir = temp_dir("exec-dashdash");
    let spaced = dir.join("dir with space");
    fs::create_dir_all(&spaced).expect("create spaced dir");
    let stub = write_executable(
        &spaced,
        "show-args",
        "#!/bin/sh\nprintf 'argv0=%s args=%s\\n' \"$0\" \"$*\"\nexit 7\n",
    );
    let path = stub.to_string_lossy().into_owned();
    let outcome = shell(&format!("exec -- '{path}' --no-reexec"));
    assert_eq!(outcome.status, 7, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, format!("argv0={path} args=--no-reexec\n"));

    // `exec --` on its own is a no-op that leaves the shell running.
    let outcome = shell("exec --; echo after");
    assert_eq!(outcome.stdout, "after\n");
    assert_eq!(outcome.status, 0);

    // A directory is "cannot execute" (126), reported without Rust's
    // `(os error 21)` — for an `exec` operand and for a script operand alike.
    let outcome = shell("exec /tmp; echo after");
    assert_eq!(outcome.status, 126, "stderr: {}", outcome.stderr);
    assert!(!outcome.stdout.contains("after"), "{}", outcome.stdout);
    assert!(
        outcome.stderr.contains("ash: exec: /tmp: Is a directory"),
        "{}",
        outcome.stderr
    );
    assert!(!outcome.stderr.contains("os error"), "{}", outcome.stderr);

    let outcome = run(&["/tmp"]);
    assert_eq!(outcome.status, 126, "stderr: {}", outcome.stderr);
    assert!(
        outcome.stderr.contains("/tmp: Is a directory"),
        "{}",
        outcome.stderr
    );
    assert!(!outcome.stderr.contains("os error"), "{}", outcome.stderr);

    // Something that exists but cannot be executed is 126 too, with the same
    // clean reason.
    let outcome = shell("exec /etc/hostname; echo after");
    assert_eq!(outcome.status, 126, "stderr: {}", outcome.stderr);
    assert!(
        outcome.stderr.contains("Permission denied"),
        "{}",
        outcome.stderr
    );
    assert!(!outcome.stderr.contains("os error"), "{}", outcome.stderr);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn unrunnable_commands_report_the_reason_a_shell_reports() {
    // Bash's status and wording for each way a command cannot run: everything
    // that exists but cannot execute is 126, everything that resolves to
    // nothing is 127, and no diagnostic carries Rust's `(os error N)`.
    let cases = [
        ("/tmp", 126, "/tmp: Is a directory"),
        ("/etc/hostname", 126, "/etc/hostname: Permission denied"),
        (
            "/no/such/ash-cmd",
            127,
            "/no/such/ash-cmd: No such file or directory",
        ),
        ("./nope", 127, "./nope: No such file or directory"),
        (
            "ash-no-such-command",
            127,
            "ash-no-such-command: command not found",
        ),
        // `command` runs a name through the same table, with the same reasons
        // and statuses.
        ("command /tmp", 126, "/tmp: Is a directory"),
        (
            "command /no/such/ash-cmd",
            127,
            "/no/such/ash-cmd: No such file or directory",
        ),
        (
            "command ash-no-such-command",
            127,
            "ash-no-such-command: command not found",
        ),
    ];
    for (command, status, message) in cases {
        let outcome = shell(command);
        assert_eq!(outcome.status, status, "{command}: {}", outcome.stderr);
        assert!(
            outcome.stderr.contains(message),
            "{command}: {}",
            outcome.stderr
        );
        assert!(!outcome.stderr.contains("os error"), "{command}: {}", outcome.stderr);
    }

    // The same failures inside a script, which keeps going and reports each
    // status.
    let dir = temp_dir("unrunnable");
    let script = write_script(
        &dir,
        "unrunnable.sh",
        "/tmp\necho first=$?\n./nope\necho second=$?\nash-no-such-command\necho third=$?\n",
    );
    let outcome = run(&[&script.to_string_lossy()]);
    assert_eq!(outcome.status, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, "first=126\nsecond=127\nthird=127\n");
    assert!(
        outcome.stderr.contains("/tmp: Is a directory"),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("./nope: No such file or directory"),
        "{}",
        outcome.stderr
    );
    assert!(!outcome.stderr.contains("os error"), "{}", outcome.stderr);

    // A `PATH` hit that exists without execute permission is 126 and names the
    // path it resolved to; a directory in `PATH` is skipped, so a later
    // directory can still hold the program.
    let bin_one = dir.join("bin1");
    let bin_two = dir.join("bin2");
    fs::create_dir_all(bin_one.join("adir")).expect("create bin dirs");
    fs::create_dir_all(&bin_two).expect("create bin dirs");
    write_script(&bin_one, "plain", "echo never-runs\n");
    write_executable(&bin_two, "plain", "#!/bin/sh\necho from-bin2\n");

    let path = format!("{}:{}", bin_one.display(), bin_two.display());
    let outcome = run_with_env(&["-c", "plain"], &[("PATH", &path)]);
    assert_eq!(outcome.status, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, "from-bin2\n");

    let outcome = run_with_env(&["-c", "adir"], &[("PATH", &path)]);
    assert_eq!(outcome.status, 127, "stderr: {}", outcome.stderr);
    assert!(
        outcome.stderr.contains("adir: command not found"),
        "{}",
        outcome.stderr
    );

    let only_unrunnable = bin_one.display().to_string();
    let outcome = run_with_env(&["-c", "plain"], &[("PATH", &only_unrunnable)]);
    assert_eq!(outcome.status, 126, "stderr: {}", outcome.stderr);
    assert!(
        outcome
            .stderr
            .contains(&format!("{}/plain: Permission denied", bin_one.display())),
        "{}",
        outcome.stderr
    );
    assert!(!outcome.stderr.contains("os error"), "{}", outcome.stderr);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn heredoc_bodies_expand_unless_the_delimiter_is_quoted() {
    // Bash's output for each of these, byte for byte.
    let outcome = shell("X=1\ncat <<EOF\n$X ${X} $(echo sub) `echo bt`\nEOF\n");
    assert_eq!(outcome.stdout, "1 1 sub bt\n", "stderr: {}", outcome.stderr);
    assert_eq!(outcome.status, 0);

    let outcome = shell("X=1\ncat <<'EOF'\n$X $(echo sub)\nEOF\n");
    assert_eq!(
        outcome.stdout, "$X $(echo sub)\n",
        "stderr: {}",
        outcome.stderr
    );

    // A loop variable is re-expanded for every iteration.
    let outcome = shell("for i in 1 2; do cat <<EOF\nline $i\nEOF\ndone\n");
    assert_eq!(
        outcome.stdout, "line 1\nline 2\n",
        "stderr: {}",
        outcome.stderr
    );

    // `\$`, `\\` and backslash-newline are the only escapes an unquoted body
    // honours; `\"` keeps its backslash.
    let outcome = shell("cat <<EOF\na\\$X b\\\\c \\\nd \\\"q\\\"\nEOF\n");
    assert_eq!(
        outcome.stdout, "a$X b\\c d \\\"q\\\"\n",
        "stderr: {}",
        outcome.stderr
    );

    // A body reaches a builtin loop the same way it reaches an external.
    let outcome = shell("while read l; do echo \"got $l\"; done <<EOF\n$USER\nEOF\n");
    assert_eq!(
        outcome.stdout,
        format!("got {}\n", whoami()),
        "stderr: {}",
        outcome.stderr
    );
    assert_eq!(outcome.status, 0);

    // A heredoc whose command writes to a file expands the same way, and the
    // body is on the file the redirection names.
    let dir = temp_dir("heredoc-file");
    let target = dir.join("out.txt").display().to_string();
    let script = write_script(
        &dir,
        "heredoc-file.sh",
        &format!(
            "W=expanded\ncat <<BODY > {target}\n$W ${{W}} $(/bin/echo sub)\nBODY\ncat <<'QUOTED' >> {target}\n$W $(/bin/echo sub)\nQUOTED\ncat {target}\n"
        ),
    );
    let outcome = run(&[&script.to_string_lossy()]);
    assert_eq!(
        outcome.stdout, "expanded expanded sub\n$W $(/bin/echo sub)\n",
        "stderr: {}",
        outcome.stderr
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn alias_names_take_hyphens_and_a_missing_name_fails() {
    // A temp HOME keeps the definition out of the user's real config.
    let dir = temp_dir("alias-home");
    let home = dir.to_string_lossy().into_owned();

    // `/etc/profile.d/mingw64.sh` defines `alias mingw64-env=…`; names like
    // that are legal for bash and dash, so they are legal here.
    let outcome = run_with_env(
        &["-c", "alias mingw64-env='echo m'; echo rc=$?"],
        &[("HOME", home.as_str())],
    );
    assert_eq!(outcome.stdout, "rc=0\n", "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stderr, "", "stderr: {}", outcome.stderr);

    // Asking about a name that does not exist is a failure (status 1), as in
    // bash and dash.
    let outcome = run_with_env(
        &["-c", "alias nosuchalias; echo rc=$?"],
        &[("HOME", home.as_str())],
    );
    assert_eq!(outcome.stdout, "rc=1\n");
    assert!(outcome.stderr.contains("nosuchalias"), "{}", outcome.stderr);

    // None of it may make a real login run noisy.
    let outcome = run_with_env(&["-l", "-c", "echo login-ok"], &[("HOME", home.as_str())]);
    assert_eq!(outcome.stdout, "login-ok\n", "stderr: {}", outcome.stderr);
    assert!(!outcome.stderr.contains("alias:"), "{}", outcome.stderr);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn special_builtins_run_without_external_commands() {
    assert_eq!(shell(": ; echo colon=$?").stdout, "colon=0\n");
    assert_eq!(shell("eval 'echo evaluated'").stdout, "evaluated\n");
    assert_eq!(shell("eval 'exit 4'").status, 4);

    let outcome = shell("readonly R=1; R=2; echo status=$?");
    assert_eq!(outcome.status, 0);
    assert_eq!(outcome.stdout, "status=1\n");
    assert!(outcome.stderr.contains("readonly"), "{}", outcome.stderr);

    assert_eq!(
        shell("set -- a b c; echo \"$#:$*\"; shift; echo \"$#:$*\"").stdout,
        "3:a b c\n2:b c\n"
    );
    assert_eq!(shell("trap 'echo bye' EXIT; echo body").stdout, "body\nbye\n");

    // `$?` inside the EXIT trap is the status the shell is leaving with.
    let outcome = shell("trap 'echo status=$?' EXIT; exit 9");
    assert_eq!(outcome.stdout, "status=9\n");
    assert_eq!(outcome.status, 9);
    assert_eq!(shell("times >/dev/null; echo times-ok").stdout, "times-ok\n");
}

#[test]
fn control_flow_builtins_stay_inside_their_enclosing_construct() {
    // `return` ends the function, not the shell.
    let outcome = shell("f() { echo in-f; return 7; echo never; }; f; echo rc=$?");
    assert_eq!(outcome.stdout, "in-f\nrc=7\n");
    assert_eq!(outcome.status, 0);

    // `break` and `continue` drive the loop, and the shell keeps going.
    let outcome = shell(
        "for i in 1 2 3; do if [ $i = 2 ]; then continue; fi; if [ $i = 3 ]; then break; fi; echo i=$i; done; echo done",
    );
    assert_eq!(outcome.stdout, "i=1\ndone\n");

    // Outside any function or sourced file, `return` is an error that does
    // not end the shell.
    let outcome = shell("return 5; echo continued");
    assert_eq!(outcome.stdout, "continued\n");
    assert_eq!(outcome.status, 0);
}

#[test]
fn parameters_and_command_substitution_expand() {
    let outcome = run_with_env(
        &["-c", "echo $HOME; echo ${HOME}; echo \"$HOME\""],
        &[("HOME", "/home/ash-test")],
    );
    assert_eq!(
        outcome.stdout,
        "/home/ash-test\n/home/ash-test\n/home/ash-test\n"
    );

    assert_eq!(
        shell("echo \"backtick: `echo legacy`\"").stdout,
        "backtick: legacy\n"
    );
    assert_eq!(
        shell("echo \"dollar: $(echo modern)\"").stdout,
        "dollar: modern\n"
    );
    assert_eq!(shell("echo $(echo a)$(echo b)").stdout, "ab\n");

    // A substitution runs in its own environment, and its output is the
    // part of the word it sat in.
    let outcome = shell("Y=$(cd /; pwd); echo \"[$Y]\" \"$(pwd)\"");
    assert!(outcome.stdout.starts_with("[/] "), "{}", outcome.stdout);

    // The captured text is spliced into its word, its trailing newlines are
    // stripped, and a substitution that captures nothing contributes no field.
    assert_eq!(shell("printf '[%s]' x$(echo hi)y").stdout, "[xhiy]");
    assert_eq!(shell("printf '[%s]' \"$(echo hi)\"").stdout, "[hi]");
    assert_eq!(shell("set -- $(true); echo $#").stdout, "0\n");

    // Variables and the working directory a substitution sets stay inside it.
    assert_eq!(
        shell("Z=$(ASH_LEAK=1; cd /; pwd); echo \"z=$Z leak=${ASH_LEAK-unset}\"").stdout,
        "z=/ leak=unset\n"
    );
}

#[test]
fn quoted_empty_words_stay_in_the_argument_list() {
    // `[ x != "" ]` and friends depend on `""` reaching the command.
    assert_eq!(
        shell("printf '[%s]' a \"\" b; echo").stdout,
        "[a][][b]\n"
    );
    assert_eq!(shell("set -- a \"\" b; echo $#").stdout, "3\n");
    assert_eq!(shell("[ x != \"\" ]; echo rc=$?").stdout, "rc=0\n");
}

#[test]
fn assignments_and_compound_commands_run() {
    assert_eq!(shell("X=1; echo x=$X").stdout, "x=1\n");
    assert_eq!(shell("X=1 Y=2 env | grep -c '^[XY]='").stdout, "2\n");
    assert_eq!(
        shell("if false; then echo no; elif true; then echo elif; else echo no; fi").stdout,
        "elif\n"
    );
    assert_eq!(
        shell("i=0; while [ $i -lt 2 ]; do echo w=$i; i=$((i+1)); done").stdout,
        "w=0\nw=1\n"
    );
    assert_eq!(
        shell("for f in a 'b c'; do echo \"<$f>\"; done").stdout,
        "<a>\n<b c>\n"
    );
    assert_eq!(shell("case x in a) echo A;; x) echo X;; esac").stdout, "X\n");
    assert_eq!(shell("echo $(( 2 + 3 * 4 )) $(( 1 + 2 == 3 ))").stdout, "14 1\n");
}

#[test]
fn a_non_option_argument_is_read_as_a_script_file() {
    let dir = temp_dir("script");
    let script = write_script(
        &dir,
        "s_exec.sh",
        "echo \"name=$0 count=$# first=$1 second=$2\"\nX=script\nif [ \"$1\" = fail ]; then exit 42; fi\necho \"X=$X\"\n",
    );
    let path = script.to_string_lossy().into_owned();

    let outcome = run(&[&path, "one", "two words"]);
    assert_eq!(outcome.status, 0, "stderr: {}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        format!("name={path} count=2 first=one second=two words\nX=script\n")
    );

    // The script's exit status becomes the shell's.
    assert_eq!(run(&[&path, "fail"]).status, 42);

    // No arguments: the positionals are simply empty.
    let outcome = run(&[&path]);
    assert_eq!(
        outcome.stdout,
        format!("name={path} count=0 first= second=\nX=script\n")
    );

    // A script that cannot be read is an error, not a usage message.
    let missing = run(&[&format!("{path}.missing")]);
    assert_ne!(missing.status, 0);
    assert!(!missing.stderr.contains("unknown option"), "{}", missing.stderr);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_login_shell_reads_the_profiles_first() {
    let dir = temp_dir("login");
    write_script(
        &dir,
        ".profile",
        "export ASH_LOGIN_SENTINEL=from-profile\nPATH=/usr/bin:/bin:$PATH\n",
    );
    let home = dir.to_string_lossy().into_owned();

    let outcome = run_with_env(
        &["-l", "-c", "echo \"sentinel=$ASH_LOGIN_SENTINEL\""],
        &[("HOME", &home)],
    );
    assert_eq!(outcome.status, 0, "stderr: {}", outcome.stderr);
    assert_eq!(outcome.stdout, "sentinel=from-profile\n");

    // Without -l the profile is not read.
    let outcome = run_with_env(
        &["-c", "echo \"sentinel=$ASH_LOGIN_SENTINEL\""],
        &[("HOME", &home)],
    );
    assert_eq!(outcome.stdout, "sentinel=\n");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn unsupported_substitutions_only_fail_where_they_run() {
    // A bash-only construct inside a function that is never called must not
    // stop the file from being read — login profiles are full of them.
    let outcome = shell("f() { echo ${x//a/b}; }; echo defined");
    assert_eq!(outcome.stdout, "defined\n", "stderr: {}", outcome.stderr);
    assert_eq!(outcome.status, 0);
    assert!(outcome.stderr.is_empty(), "stderr: {}", outcome.stderr);

    // Where it does run, it is a POSIX expansion error: reported, and fatal
    // for a non-interactive shell.
    let outcome = shell("echo start; echo ${x!}; echo after");
    assert_eq!(outcome.stdout, "start\n");
    assert_eq!(outcome.status, 1);
    assert!(outcome.stderr.contains("bad substitution"), "{}", outcome.stderr);
}

#[test]
fn sourced_files_share_the_shell_and_can_return_early() {
    let dir = temp_dir("source");
    let library = write_script(
        &dir,
        "lib.sh",
        "echo \"lib sees $1\"\nreturn 6\necho never\n",
    );
    let line = format!(". {} arg; echo rc=$?", library.display());
    let outcome = shell(&line);
    assert_eq!(outcome.stdout, "lib sees arg\nrc=6\n");
    assert_eq!(outcome.status, 0);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn pipelines_sequencing_and_redirection_work_in_one_liners() {
    assert_eq!(
        shell("false && echo skipped || echo ran").stdout,
        "ran\n"
    );
    assert_eq!(shell("echo hi | cat").stdout, "hi\n");
    assert_eq!(shell("true | false").status, 1);
    assert_eq!(shell("echo one > /tmp/ash-cli-out.txt && cat < /tmp/ash-cli-out.txt").stdout, "one\n");
    fs::remove_file("/tmp/ash-cli-out.txt").ok();
}

#[test]
fn a_script_is_not_changed_by_the_configs_aliases() {
    // A personal alias named `x` used to reach `-c` and scripts, where it was
    // expanded inside a `case` pattern and made the shell refuse to parse.
    let dir = temp_dir("alias-isolation");
    let home = dir.to_string_lossy().into_owned();
    // Define the alias the way a user would, so the config is the real thing.
    let outcome = run_with_env(&["-c", "alias x='echo hi'"], &[("HOME", &home)]);
    assert_eq!(outcome.status, 0, "stderr: {}", outcome.stderr);
    let config = dir.join(".aeroshell").join("config.json");
    assert!(
        fs::read_to_string(&config).unwrap().contains("\"x\""),
        "the alias was not persisted"
    );

    // `-c`: the pattern stays a pattern, and the alias is not a command.
    let outcome = run_with_env(&["-c", "case x in a) :;; x) echo X;; esac"], &[("HOME", &home)]);
    assert_eq!(outcome.stdout, "X\n", "stderr: {}", outcome.stderr);
    assert_eq!(outcome.status, 0);
    let outcome = run_with_env(&["-c", "x"], &[("HOME", &home)]);
    assert_eq!(outcome.status, 127, "stderr: {}", outcome.stderr);

    // The same lines from a script file, and from stdin.
    let script = write_script(
        &dir,
        "alias_script.sh",
        "case x in a) :;; x) echo script-X;; esac\nx\necho \"last=$?\"\n",
    );
    let path = script.to_string_lossy().into_owned();
    let outcome = run_with_env(&[&path], &[("HOME", &home)]);
    assert_eq!(outcome.stdout, "script-X\nlast=127\n", "stderr: {}", outcome.stderr);

    let mut command = Command::new(env!("CARGO_BIN_EXE_aeroshell"));
    command
        .env("HOME", &home)
        .stdin(fs::File::open(&script).unwrap());
    let output = command.output().expect("run ash with stdin");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The REPL prints its banner before reading; the alias must not expand.
    assert!(stdout.ends_with("script-X\nlast=127\n"), "stdout: {stdout}");
    assert!(!stdout.contains("hi\n"), "the alias body ran: {stdout}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn config_aliases_are_used_only_by_an_interactive_shell() {
    // The config is the interactive session's store: a scripted shell starts
    // without those aliases, which is what keeps `-c` and files predictable.
    let mut config = aeroshell::config::Config::default_template();
    config
        .aliases
        .insert("ash-test-interactive".into(), "echo hi".into());
    aeroshell::config::set_config(config);

    let interactive = aeroshell::shell::Shell::new(true);
    assert!(interactive.aliases().contains_key("ash-test-interactive"));
    let scripted = aeroshell::shell::Shell::new(false);
    assert!(scripted.aliases().is_empty());
}
