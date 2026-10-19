//!
//! Package manifests, the installed-package index, and package loading.
//!
//! A package is a directory containing `ash-pkg.toml` plus Lua files. Installing
//! copies the tree into the package store and records it in `index.json`; the
//! shell loads every enabled package at startup. Package commands are regular
//! shell commands backed by Lua, and packages can contribute colors, aliases,
//! config settings, prompt templates/pieces and command completions.
//!
//! Contributions are applied to the in-memory config only, so disabling or
//! removing a package takes effect on the next reload without rewriting the
//! user's config file.

pub mod lua_api;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::{config, registry};

pub const MANIFEST_FILE: &str = "ash-pkg.toml";

/// Config as it was before any package contributions were applied. Reloading
/// starts from this snapshot so disabling a package removes its changes.
static BASE_CONFIG: LazyLock<Mutex<Option<config::Config>>> = LazyLock::new(|| Mutex::new(None));

/// Commands registered by packages in this session. Reloading unregisters them.
static LOADED_COMMANDS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub language: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub priority: u32,
    #[serde(default)]
    pub commands: Vec<CommandEntry>,
    #[serde(default)]
    pub completions: Vec<CompletionEntry>,
    #[serde(default)]
    pub init: Option<InitEntry>,
    #[serde(default)]
    pub contributions: Contributions,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandEntry {
    pub name: String,
    pub entry: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompletionEntry {
    pub command: String,
    pub definition: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitEntry {
    pub entrypoint: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Contributions {
    #[serde(default)]
    pub colors: Option<ColorsContribution>,
    #[serde(default)]
    pub config: Option<ConfigContribution>,
    #[serde(default)]
    pub aliases: Option<AliasesContribution>,
    #[serde(default)]
    pub prompt: Option<PromptContribution>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColorsContribution {
    #[serde(default)]
    pub palette: Option<String>,
    #[serde(default)]
    pub colors: HashMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigContribution {
    #[serde(default)]
    pub settings: HashMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AliasesContribution {
    #[serde(default)]
    pub aliases: HashMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptContribution {
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub pieces: Vec<PromptPiece>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptPiece {
    pub name: String,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
}

// ---------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PackageIndex {
    #[serde(default)]
    pub packages: HashMap<String, PackageRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackageRecord {
    pub name: String,
    pub version: String,
    pub language: String,
    #[serde(default)]
    pub description: String,
    pub priority: u32,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub resolved: String,
    #[serde(default)]
    pub installed_at: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub completions: Vec<String>,
    #[serde(default)]
    pub colors_palette: Option<String>,
    #[serde(default)]
    pub config_settings: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    #[serde(default)]
    pub prompt_template: Option<String>,
}

impl PackageIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self).context("serialize index")?;
        std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    /// Enabled packages in load order: lower priority first, so higher-priority
    /// packages are applied last and win on overrides.
    pub fn active_packages(&self) -> Vec<&PackageRecord> {
        let mut active: Vec<&PackageRecord> = self
            .packages
            .values()
            .filter(|record| record.enabled)
            .collect();
        active.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.name.cmp(&b.name))
        });
        active
    }
}

// ---------------------------------------------------------------------------
// Manifest helpers
// ---------------------------------------------------------------------------

pub fn package_file(package_root: &Path, entry: &str) -> PathBuf {
    package_root.join(entry)
}

pub fn read_manifest(root: &Path) -> Result<Manifest> {
    let path = root.join(MANIFEST_FILE);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Validate that a manifest's declared files exist and that names are safe to
/// use as directory and command names.
pub fn validate_manifest(manifest: &Manifest, root: &Path) -> Result<()> {
    if !is_valid_package_name(&manifest.name) {
        anyhow::bail!(
            "invalid package name '{}' (use letters, digits, '-', '_' and '.')",
            manifest.name
        );
    }
    if manifest.version.trim().is_empty() {
        anyhow::bail!("package '{}' has an empty version", manifest.name);
    }
    if manifest.language != "lua" {
        anyhow::bail!(
            "package '{}' uses unsupported language '{}' (only 'lua' is supported)",
            manifest.name,
            manifest.language
        );
    }

    let check = |entry: &str, label: &str| -> Result<()> {
        let relative = Path::new(entry);
        if relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            anyhow::bail!(
                "package '{}' has an unsafe entry path '{}' for {}",
                manifest.name,
                entry,
                label
            );
        }
        let path = package_file(root, entry);
        if !path.is_file() {
            anyhow::bail!("manifest references missing {}: {}", label, path.display());
        }
        Ok(())
    };

    for command in &manifest.commands {
        if command.name.trim().is_empty() || command.name.contains(char::is_whitespace) {
            anyhow::bail!("package '{}' has an invalid command name", manifest.name);
        }
        check(&command.entry, &format!("command entry '{}'", command.name))?;
    }
    if let Some(init) = &manifest.init {
        check(&init.entrypoint, "init entrypoint")?;
    }
    for completion in &manifest.completions {
        check(
            &completion.definition,
            &format!("completion entry '{}'", completion.command),
        )?;
    }
    Ok(())
}

pub fn is_valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

/// Create the Lua runtime and load all enabled packages. Called once at shell
/// startup after the config is loaded.
pub fn init() -> Result<()> {
    let cfg = config::active_config()?;
    BASE_CONFIG
        .lock()
        .expect("base config lock poisoned")
        .get_or_insert(cfg);
    lua_api::reset_runtime()?;
    load_active()
}

/// Re-apply all enabled packages after an install/remove/enable/disable.
pub fn reload() -> Result<()> {
    if let Some(base) = BASE_CONFIG
        .lock()
        .expect("base config lock poisoned")
        .clone()
    {
        config::set_config(base);
    }

    let stale: Vec<String> = {
        let mut loaded = LOADED_COMMANDS
            .lock()
            .expect("loaded commands lock poisoned");
        loaded.drain().collect()
    };
    for name in stale {
        registry::unregister(&name);
    }
    config::set_prompt_pieces(Vec::new());
    lua_api::reset_completions();
    lua_api::reset_runtime()?;
    load_active()
}

fn load_active() -> Result<()> {
    let index = PackageIndex::load(&config::package_index_path()?)?;
    for record in index.active_packages() {
        if let Err(error) = load_package(record) {
            registry::print_colored(
                &format!("ash: package '{}' failed to load: {error:#}", record.name),
                "warning",
            );
        }
    }
    Ok(())
}

fn load_package(record: &PackageRecord) -> Result<()> {
    let root = PathBuf::from(&record.resolved);
    if record.resolved.trim().is_empty() || !root.is_dir() {
        anyhow::bail!("package directory is missing: {}", root.display());
    }

    let manifest = read_manifest(&root)?;
    if manifest.name != record.name {
        anyhow::bail!(
            "manifest name '{}' does not match installed name '{}'",
            manifest.name,
            record.name
        );
    }
    validate_manifest(&manifest, &root)?;

    apply_contributions(&manifest)?;

    if let Some(init) = &manifest.init {
        lua_api::run_package_file(&root, &package_file(&root, &init.entrypoint))
            .with_context(|| format!("init {}", init.entrypoint))?;
    }
    for completion in &manifest.completions {
        lua_api::run_package_file(&root, &package_file(&root, &completion.definition))
            .with_context(|| format!("completion {}", completion.definition))?;
    }
    for command in &manifest.commands {
        if registry::resolve(&command.name).is_some()
            && !LOADED_COMMANDS
                .lock()
                .expect("loaded commands lock poisoned")
                .contains(&command.name)
        {
            registry::print_colored(
                &format!(
                    "ash: package '{}' cannot replace builtin '{}'",
                    manifest.name, command.name
                ),
                "warning",
            );
            continue;
        }

        let entry = package_file(&root, &command.entry);
        lua_api::load_command(&manifest.name, &command.name, &root, &entry)
            .with_context(|| format!("command '{}' from {}", command.name, entry.display()))?;

        let root_for_run = root.clone();
        let name_for_run = command.name.clone();
        registry::register_handler(&command.name, move |args| {
            lua_api::run_command(&root_for_run, &name_for_run, args)
        });
        LOADED_COMMANDS
            .lock()
            .expect("loaded commands lock poisoned")
            .insert(command.name.clone());
    }

    Ok(())
}

fn apply_contributions(manifest: &Manifest) -> Result<()> {
    let mut cfg = config::active_config()?;

    if let Some(colors) = &manifest.contributions.colors {
        for (name, value) in &colors.colors {
            cfg.colors.insert(name.clone(), value.clone());
        }
    }
    if let Some(settings) = &manifest.contributions.config {
        for (key, value) in &settings.settings {
            apply_setting(&mut cfg, key, value);
        }
    }
    if let Some(aliases) = &manifest.contributions.aliases {
        for (name, value) in &aliases.aliases {
            cfg.aliases.insert(name.clone(), value.clone());
        }
    }
    if let Some(prompt) = &manifest.contributions.prompt {
        if let Some(template) = &prompt.template {
            cfg.prompt_template = template.clone();
        }
        for piece in &prompt.pieces {
            if let Some(template) = &piece.template {
                config::add_prompt_piece(&piece.name, template);
            }
        }
    }

    config::set_config(cfg);
    Ok(())
}

fn apply_setting(cfg: &mut config::Config, key: &str, value: &serde_json::Value) {
    match key {
        "color" => {
            if let Some(enabled) = value.as_bool() {
                cfg.color = enabled;
            }
        }
        "username" => {
            if let Some(username) = value.as_str() {
                cfg.username = username.to_string();
            }
        }
        "time_format" => {
            if let Some(format) = value.as_str() {
                cfg.time_format = format.to_string();
            }
        }
        "active_theme" => {
            if let Some(theme) = value.as_str() {
                cfg.active_theme = theme.to_string();
            }
        }
        "prompt_template" => {
            if let Some(template) = value.as_str() {
                cfg.prompt_template = template.to_string();
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Install / remove / enable / disable
// ---------------------------------------------------------------------------

pub fn installed() -> Result<Vec<PackageRecord>> {
    let index = PackageIndex::load(&config::package_index_path()?)?;
    let mut records: Vec<PackageRecord> = index.packages.into_values().collect();
    records.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(records)
}

pub fn get(name: &str) -> Result<Option<PackageRecord>> {
    Ok(PackageIndex::load(&config::package_index_path()?)?
        .packages
        .remove(name))
}

/// Install a package and reload enabled packages.
///
/// `spec` may be a local package directory, a local archive, a git repository
/// URL (optionally with `#subdir` or a GitHub `/tree/<ref>/<path>` URL), or an
/// HTTP(S) archive link (`.tar.gz`, `.tgz`, `.tar`, `.zip`, or a GitHub
/// `/archive/` URL).
pub fn install(spec: &str) -> Result<PackageRecord> {
    let spec = spec.trim();
    if spec.is_empty() {
        anyhow::bail!("aepkg install: missing package source");
    }

    let local = Path::new(spec);
    if local.exists() {
        let source = local
            .canonicalize()
            .with_context(|| format!("package source '{}'", local.display()))?;
        if source.is_dir() {
            return install_from_dir(&source, &source.to_string_lossy());
        }
        if source.is_file() {
            let temp = TempDir::new("local-archive")?;
            let extracted = temp.path.join("extracted");
            std::fs::create_dir_all(&extracted)
                .with_context(|| format!("create {}", extracted.display()))?;
            extract_archive(&source, &extracted)?;
            let root = find_package_root(&extracted)?;
            return install_from_dir(&root, &source.to_string_lossy());
        }
        anyhow::bail!(
            "package source is not a file or directory: {}",
            source.display()
        );
    }

    // `/owner/repo` (and `/owner/repo/tree/<ref>/<path>`) works as GitHub
    // shorthand. An existing local path always wins, so absolute paths keep
    // working.
    let origin = spec;
    let remote = github_shorthand(spec).unwrap_or_else(|| spec.to_string());
    if !is_remote_source(&remote) {
        anyhow::bail!("package source not found: {spec}");
    }

    let temp = TempDir::new("remote")?;
    let materialized =
        materialize_remote(&remote, &temp.path).with_context(|| format!("install {origin}"))?;
    let root = find_package_root(&materialized)?;
    install_from_dir(&root, origin)
}

/// Resolve GitHub shorthand such as `/Aero-Shell/AshPackageTest` or
/// `/Aero-Shell/AshPackageTest/tree/main/packages/hello`.
fn github_shorthand(spec: &str) -> Option<String> {
    let rest = spec.strip_prefix('/')?;
    if rest.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();

    match parts.as_slice() {
        [owner, repo] => Some(format!("https://github.com/{owner}/{repo}")),
        [owner, repo, "tree", git_ref] => {
            Some(format!("https://github.com/{owner}/{repo}/tree/{git_ref}"))
        }
        [owner, repo, "tree", git_ref, subdir @ ..] if !subdir.is_empty() => Some(format!(
            "https://github.com/{owner}/{repo}/tree/{git_ref}/{}",
            subdir.join("/")
        )),
        _ => None,
    }
}

fn install_from_dir(source: &Path, origin: &str) -> Result<PackageRecord> {
    let manifest = read_manifest(source)?;
    validate_manifest(&manifest, source)?;

    let store = config::package_store_dir()?;
    std::fs::create_dir_all(&store).with_context(|| format!("create {}", store.display()))?;
    let index_path = config::package_index_path()?;
    let mut index = PackageIndex::load(&index_path)?;

    let mut record = install_into(source, &manifest, &store)?;
    record.source = origin.to_string();
    index.packages.insert(record.name.clone(), record.clone());
    index.save(&index_path)?;

    reload()?;
    Ok(record)
}

// ---------------------------------------------------------------------------
// Remote sources
// ---------------------------------------------------------------------------

/// Downloaded archives and clones are capped so a bad link can't fill the
/// package store.
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXTRACT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_MANIFEST_SEARCH_DEPTH: usize = 6;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("ash-pkg-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn is_remote_source(spec: &str) -> bool {
    let lower = spec.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("git://")
        || lower.starts_with("ssh://")
        || lower.starts_with("git@")
        || lower.starts_with("file://")
}

fn is_archive_url(url: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    path.ends_with(".tar.gz")
        || path.ends_with(".tgz")
        || path.ends_with(".tar")
        || path.ends_with(".zip")
        || path.contains("/archive/")
}

fn materialize_remote(spec: &str, temp: &Path) -> Result<PathBuf> {
    if is_archive_url(spec) {
        let archive = temp.join("package-download");
        download(spec, &archive)?;
        let extracted = temp.join("extracted");
        std::fs::create_dir_all(&extracted)
            .with_context(|| format!("create {}", extracted.display()))?;
        extract_archive(&archive, &extracted)?;
        return Ok(extracted);
    }

    let source = parse_git_source(spec)?;
    let clone = temp.join("clone");
    git_clone(&source.url, source.git_ref.as_deref(), &clone)?;

    match source.subdir {
        Some(subdir) => {
            if !is_safe_relative(Path::new(&subdir)) {
                anyhow::bail!("unsafe package subdirectory: {subdir}");
            }
            let joined = clone.join(&subdir);
            let clone_root = clone
                .canonicalize()
                .with_context(|| format!("resolve {}", clone.display()))?;
            let root = joined
                .canonicalize()
                .with_context(|| format!("subdirectory '{subdir}' in {spec}"))?;
            if !root.starts_with(&clone_root) {
                anyhow::bail!("package subdirectory escapes the repository: {subdir}");
            }
            Ok(root)
        }
        None => Ok(clone),
    }
}

struct GitSource {
    url: String,
    git_ref: Option<String>,
    subdir: Option<String>,
}

fn parse_git_source(spec: &str) -> Result<GitSource> {
    let (base, fragment) = match spec.split_once('#') {
        Some((base, fragment)) => (base, Some(fragment.trim().to_string())),
        None => (spec, None),
    };
    let fragment = fragment.filter(|value| !value.is_empty());
    let mut subdir = fragment;
    let mut git_ref = None;

    let mut url = base.to_string();
    if let Some(path) = github_web_path(base) {
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        if parts.len() < 2 {
            anyhow::bail!("invalid GitHub URL: {spec}");
        }
        let repo = parts[1].trim_end_matches(".git");
        if repo.is_empty() {
            anyhow::bail!("invalid GitHub repository URL: {spec}");
        }
        url = format!("https://github.com/{}/{repo}.git", parts[0]);

        if parts.len() >= 4 && parts[2] == "tree" {
            git_ref = Some(parts[3].to_string());
            if parts.len() > 4 {
                subdir = Some(parts[4..].join("/"));
            }
        } else if parts.len() > 2 {
            anyhow::bail!(
                "unsupported GitHub URL: {spec} (use the repository URL or /tree/<branch>/<path>)"
            );
        }
    }

    if url.trim().is_empty() {
        anyhow::bail!("invalid git source: {spec}");
    }
    Ok(GitSource {
        url,
        git_ref,
        subdir,
    })
}

fn github_web_path(url: &str) -> Option<&str> {
    for prefix in [
        "https://github.com/",
        "http://github.com/",
        "https://www.github.com/",
        "http://www.github.com/",
    ] {
        if let Some(head) = url.get(..prefix.len())
            && head.eq_ignore_ascii_case(prefix)
        {
            return Some(url[prefix.len()..].trim_end_matches('/'));
        }
    }
    None
}

fn git_clone(url: &str, git_ref: Option<&str>, destination: &Path) -> Result<()> {
    let mut command = std::process::Command::new("git");
    command.args(["clone", "--depth", "1", "--quiet"]);
    if let Some(git_ref) = git_ref {
        command.args(["--branch", git_ref]);
    }
    command.arg(url).arg(destination);
    // Never block a shell command on an interactive username/password prompt.
    command.env("GIT_TERMINAL_PROMPT", "0");
    if (url.starts_with("git@") || url.starts_with("ssh://"))
        && std::env::var_os("GIT_SSH_COMMAND").is_none()
    {
        command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }

    let output = command
        .output()
        .with_context(|| "run git (is git installed?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git clone failed: {}", stderr.trim());
    }
    Ok(())
}

fn download(url: &str, destination: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(format!("aeroshell/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("create HTTP client")?;
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("download {url}"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("download {url} failed: HTTP {status}");
    }
    if let Some(length) = response.content_length()
        && length > MAX_DOWNLOAD_BYTES
    {
        anyhow::bail!(
            "download {url} is too large ({length} bytes, limit {} MiB)",
            MAX_DOWNLOAD_BYTES / (1024 * 1024)
        );
    }

    let mut file = std::fs::File::create(destination)
        .with_context(|| format!("create {}", destination.display()))?;
    let mut reader = response.take(MAX_DOWNLOAD_BYTES + 1);
    let written = std::io::copy(&mut reader, &mut file)
        .with_context(|| format!("write downloaded archive {}", destination.display()))?;
    if written > MAX_DOWNLOAD_BYTES {
        anyhow::bail!(
            "download {url} exceeded the {} MiB limit",
            MAX_DOWNLOAD_BYTES / (1024 * 1024)
        );
    }
    if written == 0 {
        anyhow::bail!("download {url} returned an empty body");
    }
    Ok(())
}

fn extract_archive(archive: &Path, destination: &Path) -> Result<()> {
    let mut header = [0u8; 4];
    let read = std::fs::File::open(archive)
        .with_context(|| format!("open {}", archive.display()))?
        .read(&mut header)
        .with_context(|| format!("read {}", archive.display()))?;

    if read >= 2 && header[..2] == [0x1f, 0x8b] {
        let file =
            std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
        extract_tar(flate2::read::GzDecoder::new(file), destination)
    } else if read >= 4 && &header[..4] == b"PK\x03\x04" {
        extract_zip(archive, destination)
    } else {
        let file =
            std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
        extract_tar(file, destination)
    }
}

fn extract_tar<R: Read>(reader: R, destination: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let mut total = 0u64;

    for entry in archive.entries().context("read tar archive")? {
        let mut entry = entry.context("read tar entry")?;
        let path = entry.path().context("read tar entry path")?.into_owned();
        if !is_safe_relative(&path) {
            anyhow::bail!("archive contains unsafe path: {}", path.display());
        }
        let entry_type = entry.header().entry_type();
        // Only regular files and directories are installed; links and special
        // files could point or write outside the package tree.
        if !(entry_type.is_file() || entry_type.is_dir()) {
            continue;
        }

        total = total.saturating_add(entry.header().size().unwrap_or(0));
        if total > MAX_EXTRACT_BYTES {
            anyhow::bail!(
                "archive expands beyond the {} MiB limit",
                MAX_EXTRACT_BYTES / (1024 * 1024)
            );
        }
        if !entry
            .unpack_in(destination)
            .with_context(|| format!("extract {}", path.display()))?
        {
            anyhow::bail!("archive entry escapes destination: {}", path.display());
        }
    }
    Ok(())
}

fn extract_zip(archive: &Path, destination: &Path) -> Result<()> {
    let file =
        std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("read zip archive")?;
    let mut total = 0u64;

    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).context("read zip entry")?;
        let Some(relative) = entry.enclosed_name() else {
            let name = entry
                .name()
                .map(|name| name.into_owned())
                .unwrap_or_else(|_| "<invalid name>".to_string());
            anyhow::bail!("archive contains unsafe path: {name}");
        };
        total = total.saturating_add(entry.size());
        if total > MAX_EXTRACT_BYTES {
            anyhow::bail!(
                "archive expands beyond the {} MiB limit",
                MAX_EXTRACT_BYTES / (1024 * 1024)
            );
        }

        let target = destination.join(&relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("create {}", target.display()))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let mut output = std::fs::File::create(&target)
            .with_context(|| format!("create {}", target.display()))?;
        std::io::copy(&mut entry, &mut output)
            .with_context(|| format!("extract {}", relative.display()))?;
    }
    Ok(())
}

fn is_safe_relative(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// Locate the package inside a clone or extracted archive. A package at the
/// archive root wins; otherwise exactly one manifest may exist below it.
fn find_package_root(root: &Path) -> Result<PathBuf> {
    if root.join(MANIFEST_FILE).is_file() {
        return Ok(root.to_path_buf());
    }

    let mut found = Vec::new();
    collect_manifests(root, 0, &mut found);
    match found.as_slice() {
        [] => anyhow::bail!("no {MANIFEST_FILE} found in {}", root.display()),
        [one] => Ok(one.clone()),
        many => {
            let paths: Vec<String> = many
                .iter()
                .map(|path| {
                    path.strip_prefix(root)
                        .unwrap_or(path)
                        .display()
                        .to_string()
                })
                .collect();
            anyhow::bail!(
                "found {} packages ({}); install a subdirectory URL or append #<path>",
                many.len(),
                paths.join(", ")
            )
        }
    }
}

fn collect_manifests(directory: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > MAX_MANIFEST_SEARCH_DEPTH {
        return;
    }
    if directory.join(MANIFEST_FILE).is_file() {
        found.push(directory.to_path_buf());
        return;
    }

    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            ".git" | "target" | "node_modules" | ".aeroshell"
        ) {
            continue;
        }
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            collect_manifests(&entry.path(), depth + 1, found);
        }
    }
}

/// Copy a package tree into `store/<name>` and build its index record.
fn install_into(source: &Path, manifest: &Manifest, store: &Path) -> Result<PackageRecord> {
    let destination = store.join(&manifest.name);
    let temp = store.join(format!(".install-{}-{}", manifest.name, std::process::id()));
    if temp.exists() {
        std::fs::remove_dir_all(&temp)
            .with_context(|| format!("remove stale {}", temp.display()))?;
    }

    copy_tree(source, &temp)?;

    if destination.exists() {
        std::fs::remove_dir_all(&destination)
            .with_context(|| format!("remove old {}", destination.display()))?;
    }
    std::fs::rename(&temp, &destination)
        .with_context(|| format!("move installed package into {}", destination.display()))?;

    let prompt_template = manifest
        .contributions
        .prompt
        .as_ref()
        .and_then(|prompt| prompt.template.clone());

    Ok(PackageRecord {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        language: manifest.language.clone(),
        description: manifest.description.clone(),
        priority: manifest.priority,
        source: source.to_string_lossy().into_owned(),
        resolved: destination.to_string_lossy().into_owned(),
        installed_at: chrono::Local::now().to_rfc3339(),
        enabled: true,
        commands: manifest
            .commands
            .iter()
            .map(|command| command.name.clone())
            .collect(),
        completions: manifest
            .completions
            .iter()
            .map(|completion| completion.command.clone())
            .collect(),
        colors_palette: manifest
            .contributions
            .colors
            .as_ref()
            .and_then(|colors| colors.palette.clone()),
        config_settings: manifest
            .contributions
            .config
            .as_ref()
            .map(|config| config.settings.clone())
            .unwrap_or_default(),
        aliases: manifest
            .contributions
            .aliases
            .as_ref()
            .map(|aliases| aliases.aliases.clone())
            .unwrap_or_default(),
        prompt_template,
    })
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)
        .with_context(|| format!("create {}", destination.display()))?;
    for entry in std::fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let file_type = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target).with_context(|| {
                format!("copy {} to {}", entry.path().display(), target.display())
            })?;
        }
        // Symlinks are skipped: an installed package shouldn't be able to
        // point outside its own tree.
    }
    Ok(())
}

/// Remove a package and its files. Returns false when it is not installed.
pub fn remove(name: &str) -> Result<bool> {
    let index_path = config::package_index_path()?;
    let mut index = PackageIndex::load(&index_path)?;
    let Some(record) = index.packages.remove(name) else {
        return Ok(false);
    };

    let store = config::package_store_dir()?;
    if !record.resolved.trim().is_empty() {
        let resolved = PathBuf::from(&record.resolved);
        if resolved.starts_with(&store) && resolved.exists() {
            std::fs::remove_dir_all(&resolved)
                .with_context(|| format!("remove {}", resolved.display()))?;
        }
    }
    index.save(&index_path)?;
    reload()?;
    Ok(true)
}

/// Enable or disable an installed package.
pub fn set_enabled(name: &str, enabled: bool) -> Result<bool> {
    let index_path = config::package_index_path()?;
    let mut index = PackageIndex::load(&index_path)?;
    let Some(record) = index.packages.get_mut(name) else {
        return Ok(false);
    };
    if record.enabled == enabled {
        return Ok(true);
    }
    record.enabled = enabled;
    index.save(&index_path)?;
    reload()?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ash-pkg-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_package(root: &Path) -> Manifest {
        std::fs::create_dir_all(root.join("commands")).unwrap();
        std::fs::write(
            root.join("commands/hello.lua"),
            "return function(args) return 0 end\n",
        )
        .unwrap();
        let manifest_text = r#"
name = "hello"
version = "1.2.3"
language = "lua"
description = "test package"
priority = 7

[[commands]]
name = "hello"
entry = "commands/hello.lua"

[contributions.aliases.aliases]
hi = "hello"

[contributions.prompt]
template = "{username} > "
"#;
        std::fs::write(root.join(MANIFEST_FILE), manifest_text).unwrap();
        toml::from_str(manifest_text).unwrap()
    }

    #[test]
    fn manifest_round_trips_from_toml() {
        let dir = temp_dir("manifest");
        let manifest = write_package(&dir);
        assert_eq!(manifest.name, "hello");
        assert_eq!(manifest.priority, 7);
        assert_eq!(manifest.commands.len(), 1);
        assert_eq!(manifest.commands[0].entry, "commands/hello.lua");
        assert_eq!(
            manifest
                .contributions
                .aliases
                .as_ref()
                .unwrap()
                .aliases
                .get("hi")
                .map(String::as_str),
            Some("hello")
        );
        validate_manifest(&manifest, &dir).unwrap();
    }

    #[test]
    fn invalid_package_names_are_rejected() {
        for name in ["", "..", ".hidden", "a/b", "with space"] {
            assert!(!is_valid_package_name(name), "{name} should be invalid");
        }
        for name in ["hello", "hello-world", "hello_world", "pkg.v2"] {
            assert!(is_valid_package_name(name), "{name} should be valid");
        }
    }

    #[test]
    fn missing_command_entry_is_reported() {
        let dir = temp_dir("missing-entry");
        let mut manifest = write_package(&dir);
        manifest.commands[0].entry = "commands/nope.lua".into();
        let error = validate_manifest(&manifest, &dir).unwrap_err().to_string();
        assert!(error.contains("commands/nope.lua"), "{error}");
    }

    #[test]
    fn unsafe_entry_paths_are_rejected() {
        let dir = temp_dir("unsafe-entry");
        for entry in ["../escape.lua", "/etc/passwd"] {
            let mut manifest = write_package(&dir);
            manifest.commands[0].entry = entry.into();
            let error = validate_manifest(&manifest, &dir).unwrap_err().to_string();
            assert!(error.contains("unsafe"), "{error}");
        }
    }

    #[test]
    fn install_copies_tree_and_indexes_it() {
        let source = temp_dir("source");
        let store = temp_dir("store");
        let manifest = write_package(&source);

        let record = install_into(&source, &manifest, &store).unwrap();
        assert_eq!(record.name, "hello");
        assert_eq!(record.enabled, true);
        assert_eq!(record.commands, vec!["hello".to_string()]);
        assert_eq!(record.aliases.get("hi").map(String::as_str), Some("hello"));

        let installed = PathBuf::from(&record.resolved);
        assert!(installed.starts_with(&store));
        assert!(installed.join(MANIFEST_FILE).is_file());
        assert!(installed.join("commands/hello.lua").is_file());

        // Reinstalling replaces the tree without leaving temp directories.
        let record = install_into(&source, &manifest, &store).unwrap();
        assert!(
            PathBuf::from(&record.resolved)
                .join(MANIFEST_FILE)
                .is_file()
        );
        let leftovers: Vec<_> = std::fs::read_dir(&store)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".install-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn index_round_trips_and_sorts_by_priority() {
        let dir = temp_dir("index");
        let path = dir.join("index.json");
        let mut index = PackageIndex::new();
        for (name, priority, enabled) in [("high", 20, true), ("low", 1, true), ("off", 0, false)] {
            index.packages.insert(
                name.into(),
                PackageRecord {
                    name: name.into(),
                    version: "1.0.0".into(),
                    language: "lua".into(),
                    description: String::new(),
                    priority,
                    source: String::new(),
                    resolved: String::new(),
                    installed_at: String::new(),
                    enabled,
                    commands: Vec::new(),
                    completions: Vec::new(),
                    colors_palette: None,
                    config_settings: HashMap::new(),
                    aliases: HashMap::new(),
                    prompt_template: None,
                },
            );
        }
        index.save(&path).unwrap();

        let loaded = PackageIndex::load(&path).unwrap();
        let order: Vec<&str> = loaded
            .active_packages()
            .iter()
            .map(|record| record.name.as_str())
            .collect();
        assert_eq!(order, vec!["low", "high"]);
    }

    #[test]
    fn remote_source_detection_and_archive_urls() {
        assert!(is_remote_source("https://github.com/a/b"));
        assert!(is_remote_source("git@github.com:a/b.git"));
        assert!(!is_remote_source("./hello"));

        assert!(is_archive_url("https://example.com/hello.tar.gz"));
        assert!(is_archive_url("https://example.com/hello.TGZ?x=1"));
        assert!(is_archive_url("https://example.com/hello.zip#frag"));
        assert!(is_archive_url(
            "https://github.com/a/b/archive/refs/heads/main"
        ));
        assert!(!is_archive_url("https://github.com/a/b.git"));
    }

    #[test]
    fn github_shorthand_resolves_owner_repo_and_tree_urls() {
        assert_eq!(
            github_shorthand("/Aero-Shell/AshPackageTest").as_deref(),
            Some("https://github.com/Aero-Shell/AshPackageTest")
        );
        assert_eq!(
            github_shorthand("/Aero-Shell/AshPackageTest/tree/main/packages/extra").as_deref(),
            Some("https://github.com/Aero-Shell/AshPackageTest/tree/main/packages/extra")
        );
        assert_eq!(
            github_shorthand("/Aero-Shell/AshPackageTest/tree/main").as_deref(),
            Some("https://github.com/Aero-Shell/AshPackageTest/tree/main")
        );
        assert_eq!(github_shorthand("Aero-Shell/AshPackageTest"), None);
        // Ordinary absolute paths with three or more segments are not
        // shorthand; two-segment paths only become shorthand when they don't
        // exist locally.
        assert_eq!(github_shorthand("/home/user/package"), None);
    }

    #[test]
    fn github_urls_resolve_to_clone_urls_and_subdirectories() {
        let repo = parse_git_source("https://github.com/Aero-Shell/Shell").unwrap();
        assert_eq!(repo.url, "https://github.com/Aero-Shell/Shell.git");
        assert!(repo.git_ref.is_none());
        assert!(repo.subdir.is_none());

        let tree = parse_git_source(
            "https://github.com/Aero-Shell/Shell/tree/main/examples/packages/hello",
        )
        .unwrap();
        assert_eq!(tree.url, "https://github.com/Aero-Shell/Shell.git");
        assert_eq!(tree.git_ref.as_deref(), Some("main"));
        assert_eq!(tree.subdir.as_deref(), Some("examples/packages/hello"));

        let fragment = parse_git_source("https://gitlab.com/a/b.git#pkg/hello").unwrap();
        assert_eq!(fragment.url, "https://gitlab.com/a/b.git");
        assert_eq!(fragment.subdir.as_deref(), Some("pkg/hello"));

        assert!(parse_git_source("https://github.com/a/b/issues").is_err());
    }

    #[test]
    fn package_root_is_discovered_one_level_down() {
        let dir = temp_dir("root-discovery");
        std::fs::create_dir_all(dir.join("repo/examples/hello")).unwrap();
        std::fs::write(dir.join("repo/examples/hello/ash-pkg.toml"), "").unwrap();
        let root = find_package_root(&dir.join("repo")).unwrap();
        assert_eq!(root, dir.join("repo/examples/hello"));

        // A manifest at the root always wins over nested packages.
        std::fs::write(dir.join("repo/ash-pkg.toml"), "").unwrap();
        assert_eq!(
            find_package_root(&dir.join("repo")).unwrap(),
            dir.join("repo")
        );
    }

    #[test]
    fn multiple_packages_without_a_root_manifest_are_rejected() {
        let dir = temp_dir("many-packages");
        for name in ["one", "two"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join(MANIFEST_FILE), "").unwrap();
        }
        let error = find_package_root(&dir).unwrap_err().to_string();
        assert!(error.contains("found 2 packages"), "{error}");
    }

    #[test]
    fn missing_index_loads_as_empty() {
        let dir = temp_dir("empty-index");
        let index = PackageIndex::load(&dir.join("index.json")).unwrap();
        assert!(index.packages.is_empty());
    }
}
