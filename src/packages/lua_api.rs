//! Lua runtime and package-facing API.
//!
//! Packages share one Lua runtime per shell session, created and used on the
//! main shell thread only. `mlua::Lua` is not `Send`, so the runtime lives in a
//! thread-local slot instead of a global. Command handlers registered with the
//! shell remain plain `Send + Sync` closures; they only capture strings and
//! dispatch into the thread-local runtime when invoked.
//!
//! Package entry files are executed in a fresh environment whose `__index` is
//! the real globals table, so packages can read the API but cannot leak local
//! globals into each other. Each command entry must either return a function or
//! define a global `run` in its environment; that function is stored in a
//! session table and called with `{ cmd = "...", [1] = "arg", ... }`.
//!
//! Every `mlua::Error` is converted into `anyhow` at the Rust boundary, since
//! `mlua::Error` is not `Send`/`Sync` and cannot be stored in `anyhow` directly.

use anyhow::{Context, Result};
use mlua::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use crate::config;

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn lua_err<T>(result: LuaResult<T>) -> Result<T> {
    result.map_err(|e| anyhow::anyhow!("lua error: {e}"))
}

fn to_lua_error(e: anyhow::Error) -> mlua::Error {
    mlua::Error::runtime(format!("{e:#}"))
}

// ---------------------------------------------------------------------------
// Shared Lua runtime
// ---------------------------------------------------------------------------

struct PackageRuntime {
    lua: Lua,
}

impl PackageRuntime {
    fn new() -> Result<Self> {
        let lua = Lua::new();
        bake_api(&lua)?;
        Ok(Self { lua })
    }

    /// Execute a package file in a private environment. Returns the chunk's
    /// value plus the environment so callers can look up `run`.
    fn run_file(&self, root: &Path, path: &Path) -> Result<(LuaValue, LuaTable)> {
        let env = lua_err(self.lua.create_table())?;
        let meta = lua_err(self.lua.create_table())?;
        lua_err(meta.set("__index", self.lua.globals()))?;
        lua_err(env.set_metatable(Some(meta)))?;
        lua_err(env.set("__package_root", root.to_string_lossy().into_owned()))?;

        let chunk = self
            .lua
            .load(path)
            .set_name(path.to_string_lossy().into_owned())
            .set_environment(env.clone());

        let previous = CURRENT_ROOT.with(|slot| slot.borrow_mut().replace(root.to_path_buf()));
        let value = chunk.eval::<LuaValue>();
        CURRENT_ROOT.with(|slot| *slot.borrow_mut() = previous);
        Ok((lua_err(value)?, env))
    }

    fn command_functions(&self) -> Result<LuaTable> {
        lua_err(self.lua.globals().get("__ash_cmd_fns"))
    }

    fn hook_functions(&self, name: &str) -> Result<Vec<LuaFunction>> {
        let hooks: LuaTable = lua_err(self.lua.globals().get("__ash_hooks"))?;
        let Some(list) = lua_err(hooks.get::<Option<LuaTable>>(name))? else {
            return Ok(Vec::new());
        };
        let mut functions = Vec::new();
        for value in list.sequence_values::<LuaValue>() {
            match lua_err(value)? {
                LuaValue::Function(f) => functions.push(f),
                other => anyhow::bail!(
                    "hook '{name}' contains a non-function ({})",
                    other.type_name()
                ),
            }
        }
        Ok(functions)
    }
}

thread_local! {
    static RUNTIME: RefCell<Option<PackageRuntime>> = const { RefCell::new(None) };
    // Root of the package currently being loaded or executed.
    static CURRENT_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    // Exit code of the most recent package command, for the shell's
    // command_done hooks.
    static LAST_EXIT_STATUS: Cell<Option<i32>> = const { Cell::new(None) };
}

pub fn clear_last_exit_status() {
    LAST_EXIT_STATUS.with(|slot| slot.set(None));
}

pub fn take_last_exit_status() -> Option<i32> {
    LAST_EXIT_STATUS.with(Cell::take)
}

fn with_runtime<T>(f: impl FnOnce(&PackageRuntime) -> Result<T>) -> Result<T> {
    RUNTIME.with(|slot| {
        let borrow = slot.borrow();
        let runtime = borrow.as_ref().context("package runtime not initialized")?;
        f(runtime)
    })
}

/// Create a fresh runtime, discarding any previously registered packages and
/// hooks. Called at startup and on `aepkg` reloads.
pub fn reset_runtime() -> Result<()> {
    let runtime = PackageRuntime::new()?;
    CURRENT_ROOT.with(|slot| *slot.borrow_mut() = None);
    RUNTIME.with(|slot| *slot.borrow_mut() = Some(runtime));
    Ok(())
}

pub fn runtime_ready() -> bool {
    RUNTIME.with(|slot| slot.borrow().is_some())
}

/// Run a package init or completion definition file for its side effects.
pub fn run_package_file(root: &Path, path: &Path) -> Result<()> {
    with_runtime(|runtime| {
        runtime.run_file(root, path)?;
        Ok(())
    })
}

/// Execute a command entry file and remember its handler function under
/// `command`. Accepts a returned function or a global `run` defined by the
/// chunk.
pub fn load_command(package: &str, command: &str, root: &Path, entry: &Path) -> Result<()> {
    with_runtime(|runtime| {
        let (value, env) = runtime.run_file(root, entry)?;
        let function = match value {
            LuaValue::Function(f) => f,
            LuaValue::Nil => lua_err(env.get::<Option<LuaFunction>>("run"))?.with_context(|| {
                format!(
                    "package '{package}' command '{command}' must return a function or define run()"
                )
            })?,
            other => anyhow::bail!(
                "package '{package}' command '{command}' returned {} instead of a function",
                other.type_name()
            ),
        };

        let functions = runtime.command_functions()?;
        lua_err(functions.set(command, function))?;
        Ok(())
    })
}

fn command_args_table(lua: &Lua, cmd: &str, args: &[&str]) -> Result<LuaTable> {
    let table = lua_err(lua.create_table())?;
    lua_err(table.set("cmd", cmd.to_string()))?;
    for (index, arg) in args.iter().enumerate() {
        lua_err(table.set(index + 1, arg.to_string()))?;
    }
    Ok(table)
}

/// Invoke a package command. A numeric return value is the exit code; nil means
/// success.
pub fn run_command(root: &Path, command: &str, args: &[&str]) -> Result<()> {
    let code = run_command_code(root, command, args)?;
    LAST_EXIT_STATUS.with(|slot| slot.set(Some(code)));
    if code != 0 {
        crate::registry::print_colored(
            &format!("ash: package command '{command}' exited with {code}"),
            "warning",
        );
    }
    Ok(())
}

fn run_command_code(root: &Path, command: &str, args: &[&str]) -> Result<i32> {
    with_runtime(|runtime| {
        let functions = runtime.command_functions()?;
        let function: LuaFunction = lua_err(functions.get(command))?;
        let table = command_args_table(&runtime.lua, command, args)?;

        let previous = CURRENT_ROOT.with(|slot| slot.borrow_mut().replace(root.to_path_buf()));
        let result = function.call::<LuaValue>(table);
        CURRENT_ROOT.with(|slot| *slot.borrow_mut() = previous);

        match lua_err(result)? {
            LuaValue::Nil => Ok(0),
            LuaValue::Integer(code) => Ok(code as i32),
            other => anyhow::bail!(
                "package command '{command}' returned {} (expected an exit code)",
                other.type_name()
            ),
        }
    })
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

const HOOK_NAMES: [&str; 3] = ["prompt", "command", "command_done"];

pub fn run_prompt_hooks() -> Result<()> {
    if !runtime_ready() {
        return Ok(());
    }
    with_runtime(|runtime| {
        for function in runtime.hook_functions("prompt")? {
            lua_err(function.call::<LuaValue>(()))?;
        }
        Ok(())
    })
}

pub fn run_command_hooks(cmd: &str, args: &[&str]) -> Result<()> {
    if !runtime_ready() {
        return Ok(());
    }
    with_runtime(|runtime| {
        if runtime.hook_functions("command")?.is_empty() {
            return Ok(());
        }
        let table = command_args_table(&runtime.lua, cmd, args)?;
        for function in runtime.hook_functions("command")? {
            lua_err(function.call::<LuaValue>(table.clone()))?;
        }
        Ok(())
    })
}

pub fn run_command_done_hooks(cmd: &str, args: &[&str], status: i32) -> Result<()> {
    if !runtime_ready() {
        return Ok(());
    }
    with_runtime(|runtime| {
        if runtime.hook_functions("command_done")?.is_empty() {
            return Ok(());
        }
        let table = command_args_table(&runtime.lua, cmd, args)?;
        lua_err(table.set("status", status))?;
        for function in runtime.hook_functions("command_done")? {
            lua_err(function.call::<LuaValue>(table.clone()))?;
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Completions contributed by packages
// ---------------------------------------------------------------------------

pub static COMPLETIONS: LazyLock<Mutex<HashMap<String, Vec<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn completions_for(command: &str) -> Vec<String> {
    COMPLETIONS
        .lock()
        .expect("completion lock poisoned")
        .get(command)
        .cloned()
        .unwrap_or_default()
}

pub fn all_completions() -> Vec<(String, Vec<String>)> {
    let completions = COMPLETIONS.lock().expect("completion lock poisoned");
    let mut entries: Vec<(String, Vec<String>)> = completions
        .iter()
        .map(|(command, candidates)| (command.clone(), candidates.clone()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

pub fn reset_completions() {
    COMPLETIONS
        .lock()
        .expect("completion lock poisoned")
        .clear();
}

// ---------------------------------------------------------------------------
// API baking
// ---------------------------------------------------------------------------

fn bake_api(lua: &Lua) -> Result<()> {
    let globals = lua.globals();
    lua_err(globals.set("ash", ash_table(lua)?))?;
    lua_err(globals.set("colors", colors_table(lua)?))?;
    lua_err(globals.set("prompt", prompt_table(lua)?))?;
    lua_err(globals.set("completion", completion_table(lua)?))?;
    lua_err(globals.set("hooks", hooks_table(lua)?))?;
    lua_err(globals.set("env", env_table(lua)?))?;

    // Augment the standard `os` table instead of replacing it so os.date,
    // os.time and friends keep working.
    let os: LuaTable = lua_err(globals.get("os"))?;
    lua_err(os.set("execute", lua_err(lua.create_function(os_execute))?))?;
    lua_err(os.set("getenv", lua_err(lua.create_function(os_getenv))?))?;
    lua_err(os.set("setenv", lua_err(lua.create_function(os_setenv))?))?;
    lua_err(os.set("removeenv", lua_err(lua.create_function(os_removeenv))?))?;
    lua_err(os.set("read_file", lua_err(lua.create_function(os_read_file))?))?;
    lua_err(os.set("write_file", lua_err(lua.create_function(os_write_file))?))?;
    lua_err(os.set("read_dir", lua_err(lua.create_function(os_read_dir))?))?;

    // Internal session tables, not part of the documented API.
    lua_err(globals.set("__ash_cmd_fns", lua_err(lua.create_table())?))?;
    lua_err(globals.set("__ash_hooks", lua_err(lua.create_table())?))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ash.* — core shell access
// ---------------------------------------------------------------------------

fn ash_table(lua: &Lua) -> Result<LuaTable> {
    let table = lua_err(lua.create_table())?;
    lua_err(table.set("version", env!("CARGO_PKG_VERSION")))?;
    lua_err(table.set("config", lua_err(lua.create_function(ash_config))?))?;
    lua_err(table.set("theme", lua_err(lua.create_function(ash_theme))?))?;
    lua_err(table.set(
        "package_path",
        lua_err(lua.create_function(ash_package_path))?,
    ))?;
    lua_err(table.set("dir", lua_err(lua.create_function(ash_dir))?))?;
    Ok(table)
}

fn ash_config(lua: &Lua, _args: ()) -> LuaResult<LuaTable> {
    let cfg = crate::registry::active_config().map_err(to_lua_error)?;
    let table = lua.create_table()?;
    table.set("color", cfg.color)?;
    table.set("username", cfg.username)?;
    table.set("time_format", cfg.time_format)?;
    table.set("active_theme", cfg.active_theme)?;
    table.set("prompt_template", cfg.prompt_template)?;

    let colors = lua.create_table()?;
    for (key, value) in &cfg.colors {
        colors.set(key.clone(), value.clone())?;
    }
    table.set("colors", colors)?;
    Ok(table)
}

fn ash_theme(_lua: &Lua, theme: String) -> LuaResult<()> {
    if theme.trim().is_empty() {
        return Ok(());
    }
    let mut cfg = crate::registry::active_config().map_err(to_lua_error)?;
    cfg.active_theme = theme;
    config::save_config(&cfg).map_err(to_lua_error)?;
    config::set_config(cfg);
    Ok(())
}

fn package_root() -> Option<PathBuf> {
    CURRENT_ROOT.with(|slot| slot.borrow().clone())
}

fn ash_package_path(_lua: &Lua, sub: Option<String>) -> LuaResult<String> {
    let base = package_root()
        .or_else(|| config::package_store_dir().ok())
        .unwrap_or_default();
    let path = match sub {
        Some(sub) if !sub.trim().is_empty() => base.join(sub),
        _ => base,
    };
    Ok(path.to_string_lossy().into_owned())
}

fn ash_dir(_lua: &Lua, _args: ()) -> LuaResult<String> {
    Ok(package_root()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------
// env.* — session environment helpers
// ---------------------------------------------------------------------------

fn env_table(lua: &Lua) -> Result<LuaTable> {
    let table = lua_err(lua.create_table())?;
    lua_err(table.set("get", lua_err(lua.create_function(env_get))?))?;
    lua_err(table.set("set", lua_err(lua.create_function(env_set))?))?;
    lua_err(table.set("unset", lua_err(lua.create_function(env_unset))?))?;
    Ok(table)
}

fn env_get(_lua: &Lua, name: String) -> LuaResult<Option<String>> {
    Ok(std::env::var(&name).ok())
}

fn env_set(_lua: &Lua, (name, value): (String, String)) -> LuaResult<()> {
    if name.trim().is_empty() {
        return Ok(());
    }
    unsafe { std::env::set_var(&name, &value) };
    Ok(())
}

fn env_unset(_lua: &Lua, name: String) -> LuaResult<()> {
    if name.trim().is_empty() {
        return Ok(());
    }
    unsafe { std::env::remove_var(&name) };
    Ok(())
}

// ---------------------------------------------------------------------------
// os.* additions
// ---------------------------------------------------------------------------

fn os_execute(_lua: &Lua, cmd: String) -> LuaResult<i32> {
    if cmd.trim().is_empty() {
        return Ok(0);
    }
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .status()
        .map_err(|e| mlua::Error::runtime(format!("os.execute: {e}")))?;
    Ok(status.code().unwrap_or(-1))
}

fn os_getenv(_lua: &Lua, name: String) -> LuaResult<Option<String>> {
    Ok(std::env::var(&name).ok())
}

fn os_setenv(_lua: &Lua, (name, value): (String, String)) -> LuaResult<()> {
    if name.trim().is_empty() {
        return Ok(());
    }
    unsafe { std::env::set_var(&name, &value) };
    Ok(())
}

fn os_removeenv(_lua: &Lua, name: String) -> LuaResult<()> {
    if name.trim().is_empty() {
        return Ok(());
    }
    unsafe { std::env::remove_var(&name) };
    Ok(())
}

fn os_read_file(_lua: &Lua, path: String) -> LuaResult<Option<String>> {
    if path.trim().is_empty() {
        return Ok(None);
    }
    std::fs::read_to_string(&path)
        .map(Some)
        .map_err(|e| mlua::Error::runtime(format!("os.read_file: {path}: {e}")))
}

fn os_write_file(_lua: &Lua, (path, contents): (String, String)) -> LuaResult<()> {
    if path.trim().is_empty() {
        return Err(mlua::Error::runtime("os.write_file: empty path"));
    }
    std::fs::write(&path, contents)
        .map_err(|e| mlua::Error::runtime(format!("os.write_file: {path}: {e}")))
}

fn os_read_dir(_lua: &Lua, path: String) -> LuaResult<Vec<String>> {
    if path.trim().is_empty() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(&path)
        .map_err(|e| mlua::Error::runtime(format!("os.read_dir: {path}: {e}")))?;
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}

// ---------------------------------------------------------------------------
// colors.* — color manipulation
// ---------------------------------------------------------------------------

fn colors_table(lua: &Lua) -> Result<LuaTable> {
    let table = lua_err(lua.create_table())?;
    lua_err(table.set("get", lua_err(lua.create_function(colors_get))?))?;
    lua_err(table.set("set", lua_err(lua.create_function(colors_set))?))?;
    lua_err(table.set("reset", lua_err(lua.create_function(colors_reset))?))?;
    Ok(table)
}

fn colors_get(_lua: &Lua, name: String) -> LuaResult<Option<String>> {
    if name.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(config::get_color(&name)).filter(|value| !value.is_empty()))
}

fn colors_set(_lua: &Lua, (name, value): (String, String)) -> LuaResult<()> {
    if name.trim().is_empty() {
        return Ok(());
    }
    let mut cfg = crate::registry::active_config().map_err(to_lua_error)?;
    cfg.colors.insert(name, value);
    config::save_config(&cfg).map_err(to_lua_error)?;
    config::set_config(cfg);
    Ok(())
}

fn colors_reset(_lua: &Lua, _args: ()) -> LuaResult<()> {
    let default = config::default_config();
    let mut cfg = crate::registry::active_config().map_err(to_lua_error)?;
    cfg.colors = default.colors;
    config::save_config(&cfg).map_err(to_lua_error)?;
    config::set_config(cfg);
    Ok(())
}

// ---------------------------------------------------------------------------
// prompt.* — prompt manipulation
// ---------------------------------------------------------------------------

fn prompt_table(lua: &Lua) -> Result<LuaTable> {
    let table = lua_err(lua.create_table())?;
    lua_err(table.set("add_piece", lua_err(lua.create_function(prompt_add_piece))?))?;
    lua_err(table.set(
        "set_template",
        lua_err(lua.create_function(prompt_set_template))?,
    ))?;
    lua_err(table.set("color", lua_err(lua.create_function(prompt_color))?))?;
    Ok(table)
}

fn prompt_add_piece(_lua: &Lua, (name, template): (String, String)) -> LuaResult<()> {
    config::add_prompt_piece(&name, &template);
    Ok(())
}

fn prompt_set_template(_lua: &Lua, template: String) -> LuaResult<()> {
    if template.trim().is_empty() {
        return Ok(());
    }
    let mut cfg = crate::registry::active_config().map_err(to_lua_error)?;
    cfg.prompt_template = template;
    config::save_config(&cfg).map_err(to_lua_error)?;
    config::set_config(cfg);
    Ok(())
}

fn prompt_color(_lua: &Lua, (name, value): (String, String)) -> LuaResult<()> {
    if name.trim().is_empty() {
        return Ok(());
    }
    let mut cfg = crate::registry::active_config().map_err(to_lua_error)?;
    cfg.colors.insert(name, value);
    config::save_config(&cfg).map_err(to_lua_error)?;
    config::set_config(cfg);
    Ok(())
}

// ---------------------------------------------------------------------------
// completion.* — register argument completions
// ---------------------------------------------------------------------------

fn completion_table(lua: &Lua) -> Result<LuaTable> {
    let table = lua_err(lua.create_table())?;
    lua_err(table.set("add", lua_err(lua.create_function(completion_add))?))?;
    Ok(table)
}

fn completion_add(_lua: &Lua, (command, candidates): (String, LuaTable)) -> LuaResult<()> {
    if command.trim().is_empty() {
        return Ok(());
    }
    let mut values = Vec::new();
    for value in candidates.sequence_values::<String>() {
        values.push(value?);
    }
    COMPLETIONS
        .lock()
        .expect("completion lock poisoned")
        .insert(command, values);
    Ok(())
}

// ---------------------------------------------------------------------------
// hooks.* — hook registration
// ---------------------------------------------------------------------------

fn hooks_table(lua: &Lua) -> Result<LuaTable> {
    let table = lua_err(lua.create_table())?;
    lua_err(table.set("register", lua_err(lua.create_function(hook_register))?))?;
    Ok(table)
}

fn hook_register(lua: &Lua, (name, function): (String, LuaFunction)) -> LuaResult<()> {
    if !HOOK_NAMES.contains(&name.as_str()) {
        return Err(mlua::Error::runtime(format!(
            "hooks.register: unknown hook '{name}' (expected one of {})",
            HOOK_NAMES.join(", ")
        )));
    }
    let hooks: LuaTable = lua.globals().get("__ash_hooks")?;
    let list = match hooks.get::<Option<LuaTable>>(name.clone())? {
        Some(list) => list,
        None => {
            let list = lua.create_table()?;
            hooks.set(name.clone(), list.clone())?;
            list
        }
    };
    list.raw_set(list.raw_len() + 1, function)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ash-lua-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn command_entry_receives_args_and_returns_exit_code() {
        reset_runtime().unwrap();
        let dir = temp_dir("command");
        let entry = dir.join("greet.lua");
        std::fs::write(
            &entry,
            r#"
return function(args)
    if args.cmd ~= "greet" then return 9 end
    if #args ~= 2 then return 8 end
    if args[1] ~= "hi" or args[2] ~= "there" then return 7 end
    return 0
end
"#,
        )
        .unwrap();

        load_command("demo", "greet", &dir, &entry).unwrap();
        assert_eq!(
            run_command_code(&dir, "greet", &["hi", "there"]).unwrap(),
            0
        );
        assert_eq!(run_command_code(&dir, "greet", &["hi"]).unwrap(), 8);
        assert!(run_command_code(&dir, "missing", &[]).is_err());
    }

    #[test]
    fn entries_can_define_run_instead_of_returning_it() {
        reset_runtime().unwrap();
        let dir = temp_dir("run-global");
        let entry = dir.join("run.lua");
        std::fs::write(&entry, "function run(args) return 4 end\n").unwrap();

        load_command("demo", "run", &dir, &entry).unwrap();
        assert_eq!(run_command_code(&dir, "run", &[]).unwrap(), 4);
    }

    #[test]
    fn completion_definitions_populate_candidates() {
        reset_runtime().unwrap();
        reset_completions();
        let dir = temp_dir("completion");
        let entry = dir.join("completion.lua");
        std::fs::write(&entry, r#"completion.add("greet", { "alpha", "beta" })"#).unwrap();

        run_package_file(&dir, &entry).unwrap();
        assert_eq!(completions_for("greet"), vec!["alpha", "beta"]);
        assert!(completions_for("other").is_empty());
    }

    #[test]
    fn invalid_hook_names_are_rejected() {
        reset_runtime().unwrap();
        let dir = temp_dir("hook");
        let entry = dir.join("init.lua");
        std::fs::write(&entry, r#"hooks.register("nope", function() end)"#).unwrap();

        let error = run_package_file(&dir, &entry).unwrap_err().to_string();
        assert!(error.contains("unknown hook"), "{error}");
    }
}
