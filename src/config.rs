use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

// Standard ANSI names usable in `<tag>` prompt templates.
fn standard_ansi(name: &str) -> Option<&'static str> {
    Some(match name {
        "black" => "\x1b[30m",
        "red" => "\x1b[31m",
        "green" => "\x1b[32m",
        "yellow" => "\x1b[33m",
        "blue" => "\x1b[34m",
        "magenta" => "\x1b[35m",
        "cyan" | "aqua" => "\x1b[36m",
        "white" => "\x1b[37m",
        "bright_black" => "\x1b[90m",
        "bright_red" => "\x1b[91m",
        "bright_green" => "\x1b[92m",
        "bright_yellow" => "\x1b[93m",
        "bright_blue" => "\x1b[94m",
        "bright_magenta" => "\x1b[95m",
        "bright_cyan" => "\x1b[96m",
        "bright_white" => "\x1b[97m",
        "bold" => "\x1b[1m",
        "italic" => "\x1b[3m",
        "underline" => "\x1b[4m",
        "strikethrough" => "\x1b[9m",
        "dim" => "\x1b[2m",
        "highlight" => "\x1b[7m",
        _ => return None,
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub color: bool,
    pub username: String,
    pub time_format: String,
    pub active_theme: String,
    pub prompt_template: String,
    #[serde(default)]
    pub colors: HashMap<String, String>,
    // Persistent aliases. An interactive session expands them where a command
    // name can appear; `-c` and script runs start without them.
    #[serde(default)]
    pub aliases: HashMap<String, String>,
}

impl Config {
    pub fn default_template() -> Self {
        let mut colors = HashMap::new();
        colors.insert("info".into(), "\x1b[33m".into());
        colors.insert("error".into(), "\x1b[31m".into());
        colors.insert("success".into(), "\x1b[32m".into());
        colors.insert("warning".into(), "\x1b[33m".into());
        colors.insert("header".into(), "\x1b[1;36m".into());
        colors.insert("subheader".into(), "\x1b[1;33m".into());
        colors.insert("data_primary".into(), "\x1b[36m".into());
        colors.insert("data_value".into(), "\x1b[37m".into());
        colors.insert("data_key".into(), "\x1b[33m".into());
        colors.insert("file".into(), "\x1b[37m".into());
        colors.insert("directory".into(), "\x1b[32m".into());
        colors.insert("hidden".into(), "\x1b[37m".into());
        colors.insert("reset".into(), "\x1b[0m".into());
        colors.insert("ls_dir".into(), "\x1b[32m".into());
        colors.insert("ls_hidden".into(), "\x1b[37m".into());
        colors.insert("ls_file".into(), "\x1b[37m".into());
        colors.insert("ls_script".into(), "\x1b[34m".into());
        colors.insert("bright_black".into(), "\x1b[90m".into());
        colors.insert("bright_red".into(), "\x1b[91m".into());
        colors.insert("bright_green".into(), "\x1b[92m".into());
        colors.insert("bright_yellow".into(), "\x1b[93m".into());
        colors.insert("bright_blue".into(), "\x1b[94m".into());
        colors.insert("bright_magenta".into(), "\x1b[95m".into());
        colors.insert("bright_cyan".into(), "\x1b[96m".into());
        colors.insert("bright_white".into(), "\x1b[97m".into());
        colors.insert("black".into(), "\x1b[30m".into());
        colors.insert("red".into(), "\x1b[31m".into());
        colors.insert("green".into(), "\x1b[32m".into());
        colors.insert("yellow".into(), "\x1b[33m".into());
        colors.insert("blue".into(), "\x1b[34m".into());
        colors.insert("magenta".into(), "\x1b[35m".into());
        colors.insert("cyan".into(), "\x1b[36m".into());
        colors.insert("white".into(), "\x1b[37m".into());
        colors.insert("aqua".into(), "\x1b[36m".into());
        colors.insert("bold".into(), "\x1b[1m".into());
        colors.insert("italic".into(), "\x1b[3m".into());
        colors.insert("underline".into(), "\x1b[4m".into());
        colors.insert("strikethrough".into(), "\x1b[9m".into());
        colors.insert("dim".into(), "\x1b[2m".into());
        colors.insert("highlight".into(), "\x1b[7m".into());
        colors.insert("autosuggestion".into(), "\x1b[38;5;244m".into());
        colors.insert("syntax_command".into(), "\x1b[96m".into());
        colors.insert("syntax_argument".into(), "\x1b[37m".into());
        colors.insert("syntax_redirect".into(), "\x1b[33m".into());
        colors.insert("syntax_pipe".into(), "\x1b[95m".into());
        colors.insert("syntax_quote".into(), "\x1b[92m".into());
        colors.insert("orange".into(), "\x1b[38;5;208m".into());
        colors.insert("purple".into(), "\x1b[38;5;213m".into());
        colors.insert("brown".into(), "\x1b[38;5;130m".into());
        colors.insert("slate".into(), "\x1b[38;5;244m".into());
        colors.insert("pink".into(), "\x1b[38;5;218m".into());
        colors.insert("peach".into(), "\x1b[38;5;216m".into());
        colors.insert("cream".into(), "\x1b[38;5;230m".into());
        colors.insert("lime".into(), "\x1b[38;5;140m".into());
        colors.insert("teal".into(), "\x1b[38;5;44m".into());
        colors.insert("coral".into(), "\x1b[38;5;210m".into());
        colors.insert("gold".into(), "\x1b[38;5;214m".into());
        colors.insert("bronze".into(), "\x1b[38;5;172m".into());
        colors.insert("silver".into(), "\x1b[38;5;250m".into());
        colors.insert("charcoal".into(), "\x1b[38;5;240m".into());
        colors.insert("steel".into(), "\x1b[38;5;245m".into());
        colors.insert("rust".into(), "\x1b[38;5;131m".into());
        colors.insert("forest".into(), "\x1b[38;5;34m".into());
        colors.insert("sky".into(), "\x1b[38;5;39m".into());
        colors.insert("lavender".into(), "\x1b[38;5;183m".into());
        colors.insert("coffee".into(), "\x1b[38;5;156m".into());
        colors.insert("turquoise".into(), "\x1b[38;5;41m".into());
        colors.insert("crimson".into(), "\x1b[38;5;211m".into());
        colors.insert("sand".into(), "\x1b[38;5;186m".into());
        colors.insert("mustard".into(), "\x1b[38;5;198m".into());
        colors.insert("navy".into(), "\x1b[38;5;17m".into());
        colors.insert("violet".into(), "\x1b[38;5;99m".into());
        colors.insert("host".into(), "\x1b[38;5;200m".into());
        colors.insert("tomato".into(), "\x1b[38;5;204m".into());
        colors.insert("mint".into(), "\x1b[38;5;46m".into());
        colors.insert("sage".into(), "\x1b[38;5;148m".into());
        colors.insert("lilac".into(), "\x1b[38;5;155m".into());
        colors.insert("plum".into(), "\x1b[38;5;145m".into());
        colors.insert("iris".into(), "\x1b[38;5;108m".into());

        // ls palette: dirs green, scripts blue, files white.

        Self {
            color: true,
            username: "Aero-User".into(),
            time_format: "24".into(),
            active_theme: "default".into(),
            prompt_template:
                "<green>➜</green> <blue>{hostname}</blue> <yellow>{pwd}</yellow> <green>➜</green> "
                    .into(),
            colors,
            aliases: HashMap::new(),
        }
    }
}

// Path of the persistent history file.
pub fn history_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("history.txt"))
}

// Short git branch (or short sha when detached) for the {git} prompt.
pub fn git_branch() -> String {
    let Some(mut dir) = std::env::current_dir().ok() else {
        return String::new();
    };
    loop {
        let git_dir = dir.join(".git");
        if git_dir.is_dir() {
            let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) else {
                break;
            };
            let head = head.trim();
            if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
                return branch.to_string();
            }
            return head.chars().take(7).collect();
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    String::new()
}

pub static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

pub fn default_config() -> Config {
    Config::default_template()
}

pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::home_dir().context("cannot resolve home directory")?;
    env_proxy_dir(&base)
}

fn env_proxy_dir(home: &Path) -> Result<PathBuf> {
    let candidates = [
        home.join(".aeroshell"),
        home.join(".aero"),
        home.join("aero"),
    ];

    for p in &candidates {
        if p.join("config.json").exists() {
            return Ok(p.clone());
        }
    }

    Ok(candidates[0].clone())
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

// Root directory for installed packages.
pub fn packages_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("packages"))
}

// Where unpacked package trees live, one directory per package name.
pub fn package_store_dir() -> Result<PathBuf> {
    Ok(packages_dir()?.join("store"))
}

// JSON index of installed packages.
pub fn package_index_path() -> Result<PathBuf> {
    Ok(packages_dir()?.join("index.json"))
}

pub fn load_config() -> Result<Config> {
    let path = config_path()?;
    let default = default_config();

    if !path.exists() {
        // A shell still has to come up when its state directory cannot be
        // written — a read-only home, a container without one, `HOME` set to
        // a path that does not exist. The defaults run commands fine.
        if let Err(error) = save_config(&default) {
            eprintln!(
                "\x1b[33mWarning: cannot save default settings to {}: {error}\x1b[0m",
                path.display()
            );
        }
        return Ok(default);
    }

    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut config: Config = serde_json::from_str(&text).unwrap_or_else(|e| {
        eprintln!(
            "\x1b[31mError: {} contains invalid JSON ({e}). Using default settings.\x1b[0m",
            path.display()
        );
        default
    });

    // Migrate the pre-1.0 prompt template.
    let legacy_prompt =
        "<green>{username}</green>@<blue>{hostname}</blue> <yellow>{short_pwd}</yellow> ❯ ";
    if config.prompt_template == legacy_prompt {
        config.prompt_template = default_prompt_template();
    }

    let default_colors = default_config().colors;
    for (key, value) in &default_colors {
        config
            .colors
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }

    if config
        .colors
        .get("autosuggestion")
        .is_some_and(|color| color == "\x1b[2m")
    {
        config
            .colors
            .insert("autosuggestion".into(), "\x1b[38;5;244m".into());
    }

    // Migrate old ls palettes to the current one.
    let ls_migrations = [
        ("ls_dir", "\x1b[31m", "\x1b[32m"),
        ("ls_dir", "\x1b[38;5;208m", "\x1b[32m"),
        ("ls_hidden", "\x1b[90m", "\x1b[37m"),
        ("ls_hidden", "\x1b[38;5;244m", "\x1b[37m"),
        ("ls_file", "\x1b[32m", "\x1b[37m"),
        ("ls_file", "\x1b[38;5;244m", "\x1b[37m"),
        ("directory", "\x1b[31m", "\x1b[32m"),
        ("directory", "\x1b[38;5;208m", "\x1b[32m"),
        ("file", "\x1b[32m", "\x1b[37m"),
        ("file", "\x1b[38;5;244m", "\x1b[37m"),
        ("hidden", "\x1b[90m", "\x1b[37m"),
        ("hidden", "\x1b[38;5;244m", "\x1b[37m"),
    ];
    for (key, old, new) in ls_migrations {
        if config.colors.get(key).is_some_and(|v| v == old) {
            config.colors.insert(key.into(), new.into());
        }
    }
    config
        .colors
        .entry("ls_script".into())
        .or_insert_with(|| "\x1b[34m".into());

    Ok(config)
}

pub fn save_config(config: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let text = serde_json::to_string_pretty(config).context("serialize config")?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn active_config() -> anyhow::Result<Config> {
    let lock = CONFIG
        .lock()
        .map_err(|_e| anyhow::anyhow!("config lock poisoned"))?;
    lock.clone().context("config not loaded")
}

pub fn set_config(config: Config) {
    let mut lock = CONFIG.lock().expect("config lock poisoned");
    *lock = Some(config);
}

pub fn get_config(key: &str) -> anyhow::Result<Option<String>> {
    let cfg = active_config()?;
    Ok(match key {
        "color" => Some(if cfg.color { "on".into() } else { "off".into() }),
        "username" => Some(cfg.username.clone()),
        "time_format" => Some(cfg.time_format.clone()),
        "active_theme" => Some(cfg.active_theme.clone()),
        "prompt_template" => Some(cfg.prompt_template.clone()),
        _ => None,
    })
}

pub fn get_color(key: &str) -> String {
    let cfg = match active_config() {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    if !cfg.color {
        return String::new();
    }
    cfg.colors.get(key).cloned().unwrap_or_default()
}

pub fn colorize(text: &str, key: &str) -> String {
    let cfg = match active_config() {
        Ok(c) => c,
        Err(_) => return text.to_string(),
    };
    if !cfg.color {
        return text.to_string();
    }
    let color = cfg.colors.get(key).cloned().unwrap_or_default();
    let reset = cfg
        .colors
        .get("reset")
        .cloned()
        .unwrap_or_else(|| "\x1b[0m".into());
    if color.is_empty() && reset.is_empty() {
        return text.to_string();
    }
    format!("{}{}{}", color, text, reset)
}

// Print a coloured message. Diagnostics go to stderr, so a script's captured
// stdout stays data even when a command complains; everything else (results,
// confirmations) goes to stdout. Nothing is coloured when the stream it goes
// to is not a terminal — a redirected `pwd` must print a bare path, not
// escape codes.
pub fn print_colored(text: &str, key: &str) {
    if matches!(key, "error" | "warning") {
        if std::io::stderr().is_terminal() {
            eprintln!("{}", colorize(text, key));
        } else {
            eprintln!("{text}");
        }
    } else if std::io::stdout().is_terminal() {
        println!("{}", colorize(text, key));
    } else {
        println!("{text}");
    }
}

pub fn ansi_for_color_key(key: &str) -> String {
    get_color(key)
}

// Named prompt snippets contributed by packages. They are referenced from a
// prompt template as `{{name}}`; repeat registrations replace the old snippet.
pub static PROMPT_PIECES: LazyLock<Mutex<Vec<(String, String)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

pub fn set_prompt_pieces(pieces: Vec<(String, String)>) {
    *PROMPT_PIECES.lock().expect("prompt piece lock poisoned") = pieces;
}

pub fn add_prompt_piece(name: &str, template: &str) {
    if name.trim().is_empty() || template.trim().is_empty() {
        return;
    }
    let mut pieces = PROMPT_PIECES.lock().expect("prompt piece lock poisoned");
    if let Some(existing) = pieces.iter_mut().find(|(n, _)| n == name) {
        existing.1 = template.to_string();
    } else {
        pieces.push((name.to_string(), template.to_string()));
    }
}

pub fn prompt_pieces() -> Vec<(String, String)> {
    PROMPT_PIECES
        .lock()
        .expect("prompt piece lock poisoned")
        .clone()
}

struct PromptContext {
    username: String,
    hostname: String,
    pwd: String,
    short_pwd: String,
    time: String,
    date: String,
    battery: String,
    git: String,
    colors: HashMap<String, String>,
    reset: String,
}

impl PromptContext {
    fn new(cfg: &Config) -> Self {
        let hostname = crate::hostname::gethostname().unwrap_or_else(|| "localhost".into());
        let pwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".into());
        let short_pwd = std::path::Path::new(&pwd)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".into());
        let now = chrono::Local::now();
        let time = match cfg.time_format.as_str() {
            "12" => now.format("%Y-%m-%d %I:%M:%S %p").to_string(),
            _ => now.format("%Y-%m-%d %H:%M:%S").to_string(),
        };
        let date = now.format("%Y-%m-%d").to_string();
        let reset = cfg
            .colors
            .get("reset")
            .cloned()
            .unwrap_or_else(|| "\x1b[0m".into());

        Self {
            username: cfg.username.clone(),
            hostname,
            pwd,
            short_pwd,
            time,
            date,
            battery: crate::sysinfo::battery_percent(),
            git: git_branch(),
            colors: cfg.colors.clone(),
            reset,
        }
    }

    // Color tags first (palette keys override standard names), then values.
    fn render(&self, template: &str) -> String {
        let mut output = template.to_string();
        for (key, code) in &self.colors {
            output = output.replace(&format!("<{key}>"), code);
            output = output.replace(&format!("</{key}>"), &self.reset);
        }
        for name in [
            "black",
            "red",
            "green",
            "yellow",
            "blue",
            "magenta",
            "cyan",
            "aqua",
            "white",
            "bright_black",
            "bright_red",
            "bright_green",
            "bright_yellow",
            "bright_blue",
            "bright_magenta",
            "bright_cyan",
            "bright_white",
            "bold",
            "italic",
            "underline",
            "strikethrough",
            "dim",
            "highlight",
        ] {
            if self.colors.contains_key(name) {
                continue;
            }
            if let Some(code) = standard_ansi(name) {
                output = output.replace(&format!("<{name}>"), code);
                output = output.replace(&format!("</{name}>"), &self.reset);
            }
        }

        let replace_map = [
            ("{username}", self.username.as_str()),
            ("{hostname}", self.hostname.as_str()),
            ("{pwd}", self.pwd.as_str()),
            ("{short_pwd}", self.short_pwd.as_str()),
            ("{time}", self.time.as_str()),
            ("{date}", self.date.as_str()),
            ("{battery}", self.battery.as_str()),
            ("{git}", self.git.as_str()),
        ];
        for (placeholder, value) in &replace_map {
            output = output.replace(placeholder, value);
        }
        output
    }
}

pub fn format_prompt(template: &str) -> String {
    let cfg = match active_config() {
        Ok(c) => c,
        Err(_) => return template.to_string(),
    };
    let ctx = PromptContext::new(&cfg);
    let mut output = ctx.render(template);

    // Expand `{{name}}` prompt pieces contributed by packages. Piece contents
    // are rendered once, so a piece cannot recurse into another piece.
    for (name, piece_template) in prompt_pieces() {
        let placeholder = format!("{{{{{name}}}}}");
        if output.contains(&placeholder) {
            let rendered = ctx.render(&piece_template);
            output = output.replace(&placeholder, &rendered);
        }
    }

    output
}

pub fn get_palette() -> Vec<(String, Vec<String>)> {
    vec![
        (
            "core".into(),
            vec![
                "prompt".into(),
                "info".into(),
                "error".into(),
                "success".into(),
                "warning".into(),
                "reset".into(),
            ],
        ),
        (
            "data".into(),
            vec![
                "data_primary".into(),
                "data_secondary".into(),
                "data_value".into(),
                "data_key".into(),
            ],
        ),
        (
            "format".into(),
            vec![
                "bold".into(),
                "italic".into(),
                "underline".into(),
                "strikethrough".into(),
                "dim".into(),
                "highlight".into(),
            ],
        ),
    ]
}

pub fn default_prompt_template() -> String {
    "<green>➜</green> <blue>{hostname}</blue> <yellow>{pwd}</yellow> <green>➜</green> ".into()
}

pub fn aero_version() -> String {
    env!("CARGO_PKG_VERSION").into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_ansi_knows_names_aliases_and_unknowns() {
        assert_eq!(standard_ansi("red"), Some("\x1b[31m"));
        assert_eq!(standard_ansi("aqua"), standard_ansi("cyan"));
        assert_eq!(standard_ansi("bold"), Some("\x1b[1m"));
        assert_eq!(standard_ansi("no-such-color"), None);
    }

    #[test]
    fn default_config_has_colors_a_prompt_and_no_aliases() {
        let cfg = Config::default_template();
        assert!(cfg.color);
        assert_eq!(cfg.time_format, "24");
        assert_eq!(cfg.active_theme, "default");
        assert!(cfg.aliases.is_empty());
        for key in [
            "reset",
            "error",
            "success",
            "warning",
            "header",
            "ls_dir",
            "ls_file",
            "ls_script",
            "bold",
            "autosuggestion",
            "syntax_command",
            "syntax_argument",
            "syntax_redirect",
            "syntax_pipe",
            "syntax_quote",
        ] {
            assert!(cfg.colors.contains_key(key), "missing color {key}");
        }
        assert_eq!(cfg.colors["autosuggestion"], "\x1b[38;5;244m");
        assert!(cfg.prompt_template.contains("{hostname}"));
        assert!(cfg.prompt_template.contains("{pwd}"));
        assert_eq!(cfg.prompt_template, default_prompt_template());
    }

    #[test]
    fn prompt_context_replaces_color_tags_with_codes() {
        let ctx = PromptContext::new(&Config::default_template());
        assert_eq!(ctx.render("<red>X</red>"), "\x1b[31mX\x1b[0m");
    }

    #[test]
    fn prompt_context_falls_back_to_standard_ansi() {
        let mut cfg = Config::default_template();
        cfg.colors.remove("red");
        let ctx = PromptContext::new(&cfg);
        assert_eq!(ctx.render("<red>X</red>"), "\x1b[31mX\x1b[0m");
    }

    #[test]
    fn prompt_context_leaves_unknown_tags_alone() {
        let ctx = PromptContext::new(&Config::default_template());
        assert_eq!(
            ctx.render("<notacolor>x</notacolor>"),
            "<notacolor>x</notacolor>"
        );
    }

    #[test]
    fn prompt_context_substitutes_placeholders() {
        let ctx = PromptContext::new(&Config::default_template());
        let rendered = ctx.render("{username}@{hostname}");
        assert!(rendered.starts_with("Aero-User@"), "{rendered:?}");
        assert!(!rendered.contains("{username}"));
        if let Ok(pwd) = std::env::current_dir() {
            assert_eq!(ctx.render("{pwd}"), pwd.to_string_lossy());
        }
    }

    #[test]
    fn palette_groups_are_stable() {
        let palette = get_palette();
        let names: Vec<&str> = palette.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["core", "data", "format"]);
        assert!(palette[0].1.contains(&"reset".to_string()));
        assert!(palette[2].1.contains(&"bold".to_string()));
    }

    #[test]
    fn data_dir_prefers_an_existing_config() {
        let base = std::env::temp_dir().join(format!("ash-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        assert_eq!(env_proxy_dir(&base).unwrap(), base.join(".aeroshell"));

        std::fs::create_dir_all(base.join(".aero")).unwrap();
        std::fs::write(base.join(".aero/config.json"), "{}").unwrap();
        assert_eq!(env_proxy_dir(&base).unwrap(), base.join(".aero"));

        std::fs::create_dir_all(base.join(".aeroshell")).unwrap();
        std::fs::write(base.join(".aeroshell/config.json"), "{}").unwrap();
        assert_eq!(env_proxy_dir(&base).unwrap(), base.join(".aeroshell"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
