//! Command registry and console helpers.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use crate::config;

// Shell command handler type. Builtins are plain functions; package commands
// need to capture their package/command names, so handlers are stored as
// shared closures.
pub type CommandHandler = Arc<dyn Fn(&[&str]) -> anyhow::Result<()> + Send + Sync>;
pub type CommandFn = fn(&[&str]) -> anyhow::Result<()>;

pub static COMMANDS: LazyLock<Mutex<HashMap<String, CommandHandler>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn register(name: &str, fn_: CommandFn) {
    register_handler(name, fn_);
}

pub fn register_handler<F>(name: &str, handler: F)
where
    F: Fn(&[&str]) -> anyhow::Result<()> + Send + Sync + 'static,
{
    let mut map = COMMANDS.lock().expect("command registry poisoned");
    map.insert(name.to_string(), Arc::new(handler));
}

// Remove a command. Used when reloading packages so disabled packages no
// longer shadow anything.
pub fn unregister(name: &str) {
    let mut map = COMMANDS.lock().expect("command registry poisoned");
    map.remove(name);
}

pub fn resolve(name: &str) -> Option<CommandHandler> {
    let map = COMMANDS.lock().expect("command registry poisoned");
    map.get(name).cloned()
}

// All registered command names, sorted.
pub fn names() -> Vec<String> {
    let map = COMMANDS.lock().expect("command registry poisoned");
    let mut v: Vec<String> = map.keys().cloned().collect();
    v.sort();
    v
}

// Print colored output.
pub fn print_colored(text: &str, key: &str) {
    config::print_colored(text, key);
}

// Colorize text.
pub fn colorize(text: &str, key: &str) -> String {
    config::colorize(text, key)
}

// Currently active config.
pub fn active_config() -> anyhow::Result<config::Config> {
    config::active_config()
}
