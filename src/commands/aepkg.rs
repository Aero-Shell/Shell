//! `aepkg` — install and manage Ash packages.

use anyhow::Result;

use crate::packages;
use crate::registry;

pub fn aepkg(args: &[&str]) -> Result<()> {
    match args {
        [] | ["list"] => list(),
        ["install"] => {
            registry::print_colored("Usage: aepkg install <dir | git-url | archive-url>", "info");
            Ok(())
        }
        ["install", rest @ ..] => install(&rest.join(" ")),
        ["remove", name] | ["rm", name] => remove(name),
        ["enable", name] => set_enabled(name, true),
        ["disable", name] => set_enabled(name, false),
        ["info", name] => info(name),
        ["reload"] => reload(),
        ["help"] | ["--help"] | ["-h"] => {
            help();
            Ok(())
        }
        [subcommand, ..] => {
            registry::print_colored(&format!("aepkg: unknown subcommand: {subcommand}"), "error");
            help();
            Ok(())
        }
    }
}

fn help() {
    registry::print_colored("Package manager", "header");
    println!("  aepkg install <source>    Directory, git repo, /owner/repo, or archive URL");
    println!("                            Full GitHub URLs and git URLs with #<path> work");
    println!("  aepkg list                List installed packages");
    println!("  aepkg remove <name>       Remove an installed package");
    println!("  aepkg enable <name>       Enable a package");
    println!("  aepkg disable <name>      Disable a package");
    println!("  aepkg info <name>         Show package details");
    println!("  aepkg reload              Reload enabled packages");
}

fn list() -> Result<()> {
    let packages = packages::installed()?;
    if packages.is_empty() {
        registry::print_colored(
            "No packages installed. Try `aepkg install <dir|url>`.",
            "info",
        );
        return Ok(());
    }

    registry::print_colored("Installed packages:", "header");
    for record in packages {
        let status = if record.enabled {
            "enabled"
        } else {
            "disabled"
        };
        let status_key = if record.enabled { "success" } else { "warning" };
        let state = registry::colorize(status, status_key);
        println!(
            "  {:<16} {:<10} {:<8} {}",
            record.name, record.version, state, record.description
        );
        if !record.commands.is_empty() {
            println!("      commands: {}", record.commands.join(", "));
        }
    }
    Ok(())
}

fn install(source: &str) -> Result<()> {
    match packages::install(source) {
        Ok(record) => {
            registry::print_colored(
                &format!("Installed {} {}", record.name, record.version),
                "success",
            );
            if record.commands.is_empty() {
                registry::print_colored("  (package adds no commands)", "info");
            } else {
                println!("  commands: {}", record.commands.join(", "));
            }
            println!("  location: {}", record.resolved);
            Ok(())
        }
        Err(error) => {
            registry::print_colored(&format!("aepkg install: {error:#}"), "error");
            Ok(())
        }
    }
}

fn remove(name: &str) -> Result<()> {
    match packages::remove(name) {
        Ok(true) => {
            registry::print_colored(&format!("Removed {name}"), "success");
            Ok(())
        }
        Ok(false) => {
            registry::print_colored(&format!("aepkg: not installed: {name}"), "warning");
            Ok(())
        }
        Err(error) => {
            registry::print_colored(&format!("aepkg remove: {error:#}"), "error");
            Ok(())
        }
    }
}

fn set_enabled(name: &str, enabled: bool) -> Result<()> {
    match packages::set_enabled(name, enabled) {
        Ok(true) => {
            let action = if enabled { "Enabled" } else { "Disabled" };
            registry::print_colored(&format!("{action} {name}"), "success");
            Ok(())
        }
        Ok(false) => {
            registry::print_colored(&format!("aepkg: not installed: {name}"), "warning");
            Ok(())
        }
        Err(error) => {
            registry::print_colored(&format!("aepkg: {error:#}"), "error");
            Ok(())
        }
    }
}

fn reload() -> Result<()> {
    packages::reload()?;
    registry::print_colored("Reloaded packages", "success");
    Ok(())
}

fn info(name: &str) -> Result<()> {
    let Some(record) = packages::get(name)? else {
        registry::print_colored(&format!("aepkg: not installed: {name}"), "warning");
        return Ok(());
    };

    registry::print_colored(&format!("{} {}", record.name, record.version), "header");
    if !record.description.is_empty() {
        println!("  description: {}", record.description);
    }
    println!("  language:    {}", record.language);
    println!("  priority:    {}", record.priority);
    println!("  enabled:     {}", record.enabled);
    println!("  installed:   {}", record.installed_at);
    println!("  source:      {}", record.source);
    println!("  location:    {}", record.resolved);
    if !record.commands.is_empty() {
        println!("  commands:    {}", record.commands.join(", "));
    }
    if !record.completions.is_empty() {
        println!("  completions: {}", record.completions.join(", "));
    }
    if let Some(palette) = &record.colors_palette {
        println!("  palette:     {palette}");
    }
    if !record.aliases.is_empty() {
        let mut aliases: Vec<_> = record.aliases.iter().collect();
        aliases.sort_by_key(|(name, _)| *name);
        let values: Vec<String> = aliases
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        println!("  aliases:     {}", values.join(", "));
    }
    if let Some(template) = &record.prompt_template {
        println!("  prompt:      {template}");
    }
    Ok(())
}
