//! Command registry and console helpers.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::config;

// Shell command handler type.
pub type CommandFn = fn(&[&str]) -> anyhow::Result<()>;

pub static COMMANDS: LazyLock<Mutex<HashMap<String, CommandFn>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn register(name: &str, fn_: CommandFn) {
    let mut map = COMMANDS.lock().expect("command registry poisoned");
    map.insert(name.to_string(), fn_);
}

pub fn resolve(name: &str) -> Option<CommandFn> {
    let map = COMMANDS.lock().expect("command registry poisoned");
    map.get(name).copied()
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
