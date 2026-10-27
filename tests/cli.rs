//! End-to-end tests for the surfaces a script, a session manager or a login
//! profile drives: `ash -c '…'`, `ash script.sh …` and `ash -l -c '…'`.
//!
//! The unit tests inside `src/shell` cover the evaluator in-process; builtin
//! output cannot be captured through a redirected descriptor there, because
//! the test harness claims `println!`. These tests run the real binary and
//! read its actual stdout, stderr and exit status.

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
    assert!(outcome.stderr.contains("not found"), "{}", outcome.stderr);
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
    let profile = write_script(
        &dir,
        ".profile",
        "export ASH_LOGIN_SENTINEL=from-profile\nPATH=/usr/bin:/bin:$PATH\n",
    );
    assert!(profile.exists());
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
