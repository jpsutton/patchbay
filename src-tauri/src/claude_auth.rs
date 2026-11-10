//! Claude Code subscription OAuth for the Chat tab and Patchbay's agent.
//!
//! Reads a Claude Code config directory's login: `<dir>/.credentials.json` (what Claude Code
//! writes on Linux and Windows), or on macOS the Keychain item Claude Code keeps for that
//! directory. Refreshes the access token when it is near expiry, and writes rotated tokens
//! back to the same place so Claude Code stays signed in.
//!
//! Each provider may name its own directory (another `CLAUDE_CONFIG_DIR`), so one machine can
//! use several Claude accounts side by side.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

/// Claude Code's public OAuth client id (from the Claude Code binary).
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Refresh when fewer than this many ms remain until expiry.
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;

struct Cached {
    token: String,
    expires_at_ms: i64,
}

/// Tokens by config directory: one entry per Claude account in use.
static CACHE: RwLock<BTreeMap<PathBuf, Cached>> = RwLock::new(BTreeMap::new());
static REFRESH_LOCK: Mutex<()> = Mutex::const_new(());

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn token_endpoint() -> String {
    std::env::var("PATCHBAY_CLAUDE_TOKEN_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com/v1/oauth/token".into())
}

#[cfg(test)]
pub(crate) static ENV_LOCK: Mutex<()> = Mutex::const_new(());
#[cfg(test)]
static TEST_HOME: RwLock<Option<PathBuf>> = RwLock::new(None);
#[cfg(test)]
static TEST_CONFIG_DIR: RwLock<Option<String>> = RwLock::new(None);

#[cfg(test)]
pub(crate) fn set_test_env(home: Option<PathBuf>, config_dir: Option<String>) {
    *TEST_HOME.write().unwrap() = home;
    *TEST_CONFIG_DIR.write().unwrap() = config_dir;
}

fn raw_config_dir_env() -> Option<String> {
    #[cfg(test)]
    if let Some(dir) = TEST_CONFIG_DIR.read().unwrap().clone() {
        return Some(dir);
    }
    std::env::var("CLAUDE_CONFIG_DIR").ok()
}

pub(crate) fn home() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(h) = TEST_HOME.read().unwrap().clone() {
        return Ok(h);
    }
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot find home directory"))
}

/// Directory Claude Code stores config in (`$CLAUDE_CONFIG_DIR` or `~/.claude`).
pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = raw_config_dir_env() {
        let d = dir.trim();
        if !d.is_empty() {
            let p: PathBuf = if d == "~" {
                home()?
            } else if let Some(rest) = d.strip_prefix("~/") {
                home()?.join(rest)
            } else {
                PathBuf::from(d)
            }
            .components()
            .collect();
            if !p.as_os_str().is_empty() {
                return Ok(p);
            }
        }
    }
    Ok(home()?.join(".claude").components().collect())
}

/// A provider's Claude config directory (`~/` expanded), or the default when it names none.
pub fn resolve_dir(dir: Option<&str>) -> Result<PathBuf> {
    let raw = match dir.map(str::trim).filter(|d| !d.is_empty()) {
        None => return config_dir(),
        Some("~") => home()?,
        Some(d) => match d.strip_prefix("~/") {
            Some(rest) => home()?.join(rest),
            None => PathBuf::from(d),
        },
    };
    Ok(raw.components().collect())
}

/// Normalize `path` for directory-identity comparisons: strip trailing slashes and `.`
/// components, and resolve symlinks on the path (or its longest existing ancestor when the
/// leaf directory has not been created yet).
pub fn canonical_dir(path: &Path) -> PathBuf {
    fn resolve(p: &Path, depth: u8) -> PathBuf {
        let cleaned: PathBuf = p.components().collect();
        if cleaned.as_os_str().is_empty() {
            return PathBuf::new();
        }
        if let Ok(c) = std::fs::canonicalize(&cleaned) {
            return c;
        }
        if depth < 8
            && let Ok(target) = std::fs::read_link(&cleaned)
        {
            let next = if target.is_absolute() {
                target
            } else {
                cleaned.parent().unwrap_or_else(|| Path::new("")).join(target)
            };
            return resolve(&next, depth + 1);
        }
        let mut cur = cleaned.as_path();
        let mut tail = Vec::new();
        while let Some(parent) = cur.parent() {
            if let Some(name) = cur.file_name() {
                tail.push(name.to_os_string());
            }
            if parent.as_os_str().is_empty() {
                break;
            }
            if let Ok(mut base) = std::fs::canonicalize(parent) {
                for part in tail.into_iter().rev() {
                    base.push(part);
                }
                return base;
            }
            if depth < 8
                && let Ok(target) = std::fs::read_link(parent)
            {
                let mut next = if target.is_absolute() {
                    target
                } else {
                    parent.parent().unwrap_or_else(|| Path::new("")).join(target)
                };
                for part in tail.into_iter().rev() {
                    next.push(part);
                }
                return resolve(&next, depth + 1);
            }
            cur = parent;
        }
        cleaned
    }
    resolve(path, 0)
}

/// True when `a` and `b` refer to the same directory after resolving symlinks and trailing slashes.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    canonical_dir(a) == canonical_dir(b)
}

/// True when `key` is a config-directory path fallback, not Claude Code's portable
/// `<accountUuid>:<organizationUuid>`. Path keys are machine-local: the same login on another
/// computer has a different path, so the mesh must not keep them as separate accounts.
/// A Windows drive path counts even on unix receivers, where `Path::is_absolute` says no.
pub fn is_path_account_key(key: &str) -> bool {
    let b = key.as_bytes();
    let drive = b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    key.starts_with("~/") || Path::new(key).is_absolute() || drive
}

/// The Claude account a config directory is signed in to, as a key that is the same on every
/// computer (`<accountUuid>:<organizationUuid>`, from Claude Code's `.claude.json`), and the
/// organization's name. Falls back to the directory's path when Claude Code hasn't recorded one.
///
/// The profile alone is not enough to name the account: the usage a session reports comes from
/// the login in the credentials store (file or Keychain), and after a re-sign-in the two can
/// disagree — `.claude.json` still describing the previous login. Keying a report by that stale
/// profile records one account's numbers under another account's key, and every session on that
/// key (the usage guard included) then counts against it. When the stored login's rate-limit
/// tier matches neither of the profile's tiers, the directory's own path is used instead: the
/// account is left unidentified rather than misidentified, and the logins stay apart until the
/// profile catches up (the portable key returns, and `fold_plan_accounts` merges the history on
/// the next load).
pub fn account(dir: &Path) -> (String, Option<String>) {
    let guess = home().ok().and_then(|h| tier_setting_guess(dir, raw_config_dir_env().as_deref(), &h));
    account_with(dir, guess.as_deref())
}

/// `account` for a directory setting as given (`None`: the ambient one): the setting reaches the
/// Keychain lookup untouched, so the tier is read from the item `access_token` would read.
pub fn account_for(setting: Option<&str>) -> Result<(String, Option<String>)> {
    Ok(account_with(&resolve_dir(setting)?, setting))
}

/// The setting a resolved path stands for, for callers that only have the path (a path-keyed
/// plan report, the default directory): the path itself, except the default directory when no
/// variable is set, which is `None`. Everything else that has the setting passes it (`account_for`).
fn tier_setting_guess(dir: &Path, ambient: Option<&str>, home: &Path) -> Option<String> {
    let unset = ambient.is_none_or(str::is_empty);
    (!(unset && same_dir(dir, &home.join(".claude")))).then(|| dir.to_string_lossy().into_owned())
}

fn account_with(dir: &Path, setting: Option<&str>) -> (String, Option<String>) {
    #[derive(Clone)]
    struct Profile {
        key: String,
        name: Option<String>,
        tiers: Vec<String>,
    }
    #[derive(Clone, Default)]
    struct Seen {
        modified: Option<SystemTime>,
        current: Option<Profile>,
        verified: Option<Profile>,
    }
    fn parse_profile(path: &Path) -> Option<Profile> {
        let v = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())?;
        let a = &v["oauthAccount"];
        let (user, org) = (a["accountUuid"].as_str()?, a["organizationUuid"].as_str()?);
        let key = format!("{user}:{org}");
        let name = a["organizationName"].as_str().map(str::to_owned);
        let tiers = [a["userRateLimitTier"].as_str(), a["organizationRateLimitTier"].as_str()]
            .into_iter()
            .flatten()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        Some(Profile { key, name, tiers })
    }
    static SEEN: std::sync::Mutex<BTreeMap<PathBuf, Seen>> = std::sync::Mutex::new(BTreeMap::new());
    let fallback = || (dir.to_string_lossy().into_owned(), None);
    // `~/.claude` keeps it beside the directory; any other directory, inside it.
    let file = match home() {
        Ok(h) if same_dir(dir, &h.join(".claude")) => canonical_dir(&h).join(".claude.json"),
        _ => canonical_dir(dir).join(".claude.json"),
    };
    let tier = login_plan_tier(dir, setting);
    let resolve = |seen: &mut Seen| -> (String, Option<String>) {
        let Some(cur) = seen.current.clone() else {
            return fallback();
        };
        if let Some(ref t) = tier
            && !cur.tiers.is_empty()
        {
            if cur.tiers.iter().any(|x| x == t) {
                seen.verified = Some(cur.clone());
                return (cur.key, cur.name);
            }
            if let Some(ref v) = seen.verified
                && v.tiers.iter().any(|x| x == t)
            {
                return (v.key.clone(), v.name.clone());
            }
            return fallback();
        }
        (cur.key, cur.name)
    };
    let Ok(modified) = std::fs::metadata(&file).and_then(|m| m.modified()) else {
        let mut guard = SEEN.lock().unwrap();
        return guard.get_mut(&file).map_or_else(fallback, resolve);
    };
    // The file can run to megabytes, so read it again only when it changes.
    if let Some(seen) = SEEN.lock().unwrap().get_mut(&file)
        && seen.modified == Some(modified)
    {
        return resolve(seen);
    }
    let Some(profile) = parse_profile(&file) else {
        // Claude Code rewrites the file often: a read that lands mid-write keeps the last account seen.
        let mut guard = SEEN.lock().unwrap();
        return guard.get_mut(&file).map_or_else(fallback, resolve);
    };
    let mut guard = SEEN.lock().unwrap();
    let seen = guard.entry(file).or_default();
    seen.modified = Some(modified);
    seen.current = Some(profile);
    resolve(seen)
}

/// The rate-limit tier the login stored for `dir` reports: from the credentials file when it
/// holds a Claude login, otherwise (macOS) Claude Code's Keychain item. A file that exists but
/// holds only MCP logins is not this directory's login — on macOS that one lives in the
/// Keychain. Cached for a minute: `account` asks on every plan report and prompt, and a
/// Keychain read is not free (nor free of a one-time macOS authorization ask for a login this
/// app has never read).
fn login_plan_tier(dir: &Path, setting: Option<&str>) -> Option<String> {
    static TIERS: std::sync::RwLock<BTreeMap<(PathBuf, Option<String>), (std::time::Instant, Option<SystemTime>, Option<String>)>> =
        std::sync::RwLock::new(BTreeMap::new());
    const TTL: std::time::Duration = std::time::Duration::from_secs(60);
    let file = dir.join(".credentials.json");
    // The file's mtime is the re-sign-in signal; the Keychain has none, hence the time bound.
    let mtime = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
    let now = std::time::Instant::now();
    if let Ok(g) = TIERS.read()
        && let Some((at, fm, tier)) = g.get(&(dir.to_owned(), setting.map(str::to_owned)))
        && *fm == mtime
        && now.duration_since(*at) < TTL
    {
        return tier.clone();
    }
    let tier = read_login_tier(dir, setting);
    if let Ok(mut g) = TIERS.write() {
        g.insert((dir.to_owned(), setting.map(str::to_owned)), (now, mtime, tier.clone()));
    }
    tier
}

fn read_login_tier(dir: &Path, setting: Option<&str>) -> Option<String> {
    let file = dir.join(".credentials.json");
    if let Ok(raw) = std::fs::read_to_string(&file)
        && let Ok(v) = serde_json::from_str::<Value>(&raw)
        && let Some(o) = v.get("claudeAiOauth")
    {
        // The file holds this directory's login: its tier is the answer, present or not.
        return o.get("rateLimitTier").and_then(Value::as_str).map(str::to_owned);
    }
    keychain_login_tier(setting)
}

/// The login Claude Code keeps in the Keychain (macOS), where the default directory keeps its
/// Claude login even when a credentials file exists holding only MCP logins. The same setting
/// and the same resolver as `access_token` (`Store::locate`), so both read one item; a miss reads
/// as "no login tier" and the profile's key stands. Test builds read the in-memory fake.
#[cfg(target_os = "macos")]
fn keychain_login_tier(setting: Option<&str>) -> Option<String> {
    let account = std::env::var("USER").ok()?;
    keychain_tier_with(setting, raw_config_dir_env().as_deref(), &home().ok()?, &|svc| keychain_get(svc, &account).ok())
}

#[cfg(not(target_os = "macos"))]
fn keychain_login_tier(_setting: Option<&str>) -> Option<String> {
    None
}

/// The tier stored in the item `chosen_service` picks.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn keychain_tier_with(setting: Option<&str>, ambient: Option<&str>, home: &Path, get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let service = chosen_service(setting, ambient, home, get).ok()?;
    let (root, _) = parse_oauth(&get(&service)?, &service).ok()?;
    root.get("claudeAiOauth")
        .and_then(|o| o.get("rateLimitTier"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Where a config directory's login lives.
enum Store {
    File(PathBuf),
    #[cfg(target_os = "macos")]
    Keychain { service: String, account: String },
}

/// Whether `file` holds a `claudeAiOauth` login (as opposed to being absent or MCP-only).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn has_file_login(file: &Path) -> bool {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .is_some_and(|v| v.get("claudeAiOauth").is_some())
}

impl Store {
    /// The credentials file when it holds a Claude login; on macOS, otherwise Claude Code's Keychain
    /// item, found by `pick_service`. `setting` is the directory setting as given, `None` for the
    /// ambient `CLAUDE_CONFIG_DIR`. The one place a Keychain service is chosen: `access_token` and
    /// `keychain_login_tier` both come through here.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn locate(dir: &Path, setting: Option<&str>) -> Result<Store> {
        let file = dir.join(".credentials.json");
        #[cfg(target_os = "macos")]
        if !has_file_login(&file) {
            let account = std::env::var("USER").context("USER is not set")?;
            let ambient = raw_config_dir_env();
            let service = chosen_service(setting, ambient.as_deref(), &home()?, &|svc| keychain_get(svc, &account).ok())?;
            return Ok(Store::Keychain { service, account });
        }
        Ok(Store::File(file))
    }

    fn read(&self) -> Result<(Value, ClaudeAiOauth)> {
        match self {
            Store::File(path) => read_oauth(path),
            #[cfg(target_os = "macos")]
            Store::Keychain { service, account } => {
                let raw = keychain_get(service, account).with_context(|| {
                    format!("Claude Code login not found in the Keychain ({service}) — run `claude` and sign in")
                })?;
                parse_oauth(&raw, &format!("Keychain item {service}"))
            }
        }
    }

    fn write(&self, root: Value, access_token: &str, refresh_token: &str, expires_at_ms: i64) -> Result<()> {
        match self {
            Store::File(path) => write_oauth(path, root, access_token, refresh_token, expires_at_ms),
            #[cfg(target_os = "macos")]
            Store::Keychain { service, account } => {
                let root = set_oauth(root, access_token, refresh_token, expires_at_ms)?;
                keychain_put(service, account, &serde_json::to_string(&root)?)
                    .with_context(|| format!("update Keychain item {service}"))
            }
        }
    }
}

#[cfg(all(target_os = "macos", not(test)))]
fn keychain_get(service: &str, account: &str) -> Result<String> {
    Ok(keyring::Entry::new(service, account)?.get_password()?)
}

#[cfg(all(target_os = "macos", not(test)))]
fn keychain_put(service: &str, account: &str, secret: &str) -> Result<()> {
    Ok(keyring::Entry::new(service, account)?.set_password(secret)?)
}

/// Test builds never reach the real Keychain: items live here, by service name.
#[cfg(all(test, target_os = "macos"))]
pub(crate) static FAKE_KEYCHAIN: std::sync::Mutex<BTreeMap<String, String>> = std::sync::Mutex::new(BTreeMap::new());

#[cfg(all(target_os = "macos", test))]
fn keychain_get(service: &str, _account: &str) -> Result<String> {
    FAKE_KEYCHAIN.lock().unwrap().get(service).cloned().ok_or_else(|| anyhow!("no such item"))
}

#[cfg(all(target_os = "macos", test))]
fn keychain_put(service: &str, _account: &str, secret: &str) -> Result<()> {
    FAKE_KEYCHAIN.lock().unwrap().insert(service.to_owned(), secret.to_owned());
    Ok(())
}

/// The Keychain items Claude Code could be using for one config-directory setting.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, PartialEq)]
struct Candidates {
    primary: String,
    fallback: Option<String>,
}

/// The `CLAUDE_CONFIG_DIR` Claude Code would run with, and so the items it would read. `setting` is
/// the provider's directory (trimmed and `~/` expanded, as a shell would before Claude Code sees
/// it; trimming is deliberate, it is typed into a form); with none, `ambient` is Patchbay's own
/// environment value, which Claude Code inherits untouched, so only an EMPTY one counts as unset
/// (whitespace is truthy and hashed as-is). No variable: the plain item, and nothing else is a
/// candidate. A variable: the item named for exactly that string, and, only when it names the
/// default directory, the plain item as a fallback (a login made without the variable lives
/// there). Never a fallback for any other directory: the plain item is another account's.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn keychain_candidates(setting: Option<&str>, ambient: Option<&str>, home: &Path) -> Result<Candidates> {
    let setting = setting.map(str::trim).filter(|d| !d.is_empty());
    let effective = match setting {
        Some("~") => Some(home.to_string_lossy().into_owned()),
        Some(d) => Some(match d.strip_prefix("~/") {
            Some(rest) => home.join(rest).to_string_lossy().into_owned(),
            None => d.to_owned(),
        }),
        None => ambient.filter(|e| !e.is_empty()).map(str::to_owned),
    };
    let Some(env) = effective else {
        return Ok(Candidates { primary: keychain_service(None), fallback: None });
    };
    let fallback = same_dir(Path::new(&env), &home.join(".claude")).then(|| keychain_service(None));
    Ok(Candidates { primary: keychain_service(Some(&env)), fallback })
}

/// The one place a service is chosen: both `Store::locate` (token) and `keychain_tier_with` (tier).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn chosen_service(setting: Option<&str>, ambient: Option<&str>, home: &Path, get: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    Ok(pick_service(&keychain_candidates(setting, ambient, home)?, get))
}

/// Claude Code's Keychain service name (claude-code 2.1.304 `uB`): plain when run with no
/// `CLAUDE_CONFIG_DIR`, else suffixed with the first 8 hex digits of the SHA-256 of that
/// variable's value, NFC-normalized. Not keyed on the path being `~/.claude`: an explicit
/// `CLAUDE_CONFIG_DIR=~/.claude` is suffixed too.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn keychain_service(hash_input: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    use unicode_normalization::UnicodeNormalization;
    let Some(input) = hash_input else {
        return "Claude Code-credentials".into();
    };
    let nfc: String = input.nfc().collect();
    let hash = hex::encode(Sha256::digest(nfc.as_bytes()));
    format!("Claude Code-credentials-{}", &hash[..8])
}

#[cfg(test)]
pub(crate) static KEYCHAIN_CHOICE_LOGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Log once per distinct outcome which item was used, so a fallback or a conflict is visible
/// without a line per lookup. Never logs item contents.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn note_keychain_choice(service: &str, outcome: &str) {
    static SEEN: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let line = format!("claude keychain: using {service} ({outcome})");
    if SEEN.lock().unwrap().insert(line.clone()) {
        tracing::info!("{line}");
        #[cfg(test)]
        KEYCHAIN_CHOICE_LOGS.lock().unwrap().push(line);
    }
}

/// The item to read: the primary when it exists (also when the fallback exists too and holds
/// something else: that conflict is logged, the primary still wins). The fallback only when the
/// primary is ABSENT, never when it is present with a different account's login. `get` returns an
/// item's contents, `None` when it is absent.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn pick_service(c: &Candidates, get: &dyn Fn(&str) -> Option<String>) -> String {
    let primary = get(&c.primary);
    let fallback = c.fallback.as_deref().map(|f| (f, get(f)));
    match (primary, fallback) {
        (Some(p), Some((f, Some(other)))) if p != other => {
            note_keychain_choice(&c.primary, &format!("primary; conflict: {f} holds a different login"));
            c.primary.clone()
        }
        (Some(_), _) => {
            note_keychain_choice(&c.primary, "primary");
            c.primary.clone()
        }
        (None, Some((f, Some(_)))) => {
            note_keychain_choice(f, "fallback; primary absent");
            f.to_owned()
        }
        (None, _) => c.primary.clone(),
    }
}

/// Claude Code also honours `CLAUDE_SECURESTORAGE_CONFIG_DIR` (its Keychain name hashes that
/// instead). Not modelled here; say so rather than miss silently.
pub(crate) const SECURESTORAGE_WARNING: &str =
    "CLAUDE_SECURESTORAGE_CONFIG_DIR set; unmodelled — Keychain lookups may miss";

fn securestorage_warning(set: bool) -> Option<&'static str> {
    set.then_some(SECURESTORAGE_WARNING)
}

#[cfg(test)]
thread_local! {
    /// Times `warn_unmodelled_env` ran on this thread, so a test can prove startup calls it.
    pub(crate) static STARTUP_ENV_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn warn_unmodelled_env() {
    #[cfg(test)]
    STARTUP_ENV_CHECKS.with(|c| c.set(c.get() + 1));
    if let Some(w) = securestorage_warning(std::env::var_os("CLAUDE_SECURESTORAGE_CONFIG_DIR").is_some()) {
        tracing::warn!("{w}");
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeAiOauth {
    access_token: String,
    refresh_token: String,
    /// Unix epoch milliseconds.
    expires_at: i64,
}

fn read_oauth(path: &Path) -> Result<(Value, ClaudeAiOauth)> {
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "Claude Code login not found at {} — run `claude` and sign in",
            path.display()
        )
    })?;
    parse_oauth(&raw, &path.display().to_string())
}

/// `place` names where `raw` came from, for error messages.
fn parse_oauth(raw: &str, place: &str) -> Result<(Value, ClaudeAiOauth)> {
    let root: Value = serde_json::from_str(raw)
        .with_context(|| format!("invalid Claude credentials at {place}"))?;
    let oauth_val = root
        .get("claudeAiOauth")
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "no Claude Code login in {} — run `claude` and sign in",
                place
            )
        })?;
    let oauth: ClaudeAiOauth = serde_json::from_value(oauth_val).with_context(|| {
        format!(
            "Claude credentials at {} are missing access/refresh tokens",
            place
        )
    })?;
    if oauth.access_token.is_empty() || oauth.refresh_token.is_empty() {
        return Err(anyhow!(
            "Claude Code login at {} is incomplete — run `claude` and sign in",
            place
        ));
    }
    Ok((root, oauth))
}

/// `root` with its `claudeAiOauth` tokens replaced, sibling keys kept.
fn set_oauth(mut root: Value, access_token: &str, refresh_token: &str, expires_at_ms: i64) -> Result<Value> {
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("credentials root must be a JSON object"))?;
    let entry = obj
        .entry("claudeAiOauth".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let oauth = entry
        .as_object_mut()
        .ok_or_else(|| anyhow!("claudeAiOauth must be a JSON object"))?;
    oauth.insert("accessToken".into(), Value::String(access_token.to_owned()));
    oauth.insert("refreshToken".into(), Value::String(refresh_token.to_owned()));
    oauth.insert("expiresAt".into(), Value::Number(expires_at_ms.into()));
    Ok(root)
}

/// Atomically rewrite `path`, preserving sibling top-level JSON keys.
fn write_oauth(path: &Path, root: Value, access_token: &str, refresh_token: &str, expires_at_ms: i64) -> Result<()> {
    let root = set_oauth(root, access_token, refresh_token, expires_at_ms)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("credentials path has no parent directory"))?;
    let tmp = parent.join(format!(
        ".credentials.{}.tmp",
        std::process::id()
    ));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&root)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

async fn refresh_http(refresh_token: &str) -> Result<TokenResponse> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let url = token_endpoint();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("anthropic-beta", OAUTH_BETA)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
        }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let clipped: String = body.chars().take(300).collect();
        return Err(anyhow!(
            "Claude Code token refresh failed ({status}): {clipped}"
        ));
    }
    resp.json::<TokenResponse>()
        .await
        .context("parse Claude Code token refresh response")
}

fn cache_get(dir: &Path) -> Option<String> {
    let guard = CACHE.read().ok()?;
    let c = guard.get(dir)?;
    if c.expires_at_ms - now_ms() > REFRESH_SKEW_MS {
        Some(c.token.clone())
    } else {
        None
    }
}

fn cache_set(dir: &Path, token: String, expires_at_ms: i64) {
    if let Ok(mut g) = CACHE.write() {
        g.insert(dir.to_owned(), Cached { token, expires_at_ms });
    }
}

/// The environment variable Claude Code reads a long-lived token from (`claude setup-token`).
const ENV_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The token to use instead of a login: `env` when the provider names no config directory.
/// Another account's directory keeps its own login. The token has no refresh token, so it is
/// never renewed: it works until it expires or is revoked.
fn env_token(dir: Option<&str>, env: Option<String>) -> Option<String> {
    if dir.map(str::trim).is_some_and(|d| !d.is_empty()) {
        return None;
    }
    env.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
}

/// Return a usable access token for the Claude login in config directory `dir` (`None`: the
/// default one), refreshing and writing rotated tokens back when needed. For the default login,
/// `CLAUDE_CODE_OAUTH_TOKEN` takes precedence over the credentials file, as in Claude Code.
pub async fn access_token(dir: Option<&str>) -> Result<String> {
    if let Some(t) = env_token(dir, std::env::var(ENV_TOKEN).ok()) {
        return Ok(t);
    }
    let dir_setting = dir;
    let dir = resolve_dir(dir)?;
    if let Some(t) = cache_get(&dir) {
        return Ok(t);
    }
    let _guard = REFRESH_LOCK.lock().await;
    // Another waiter may have refreshed while we queued.
    if let Some(t) = cache_get(&dir) {
        return Ok(t);
    }
    token_from(&dir, &store_for(&dir, dir_setting)?).await
}

/// The store holding the login for config directory `dir`, given the setting it came from.
fn store_for(dir: &Path, dir_setting: Option<&str>) -> Result<Store> {
    Store::locate(dir, dir_setting)
}

async fn token_from(dir: &Path, store: &Store) -> Result<String> {
    let (root, oauth) = store.read()?;
    if oauth.expires_at - now_ms() > REFRESH_SKEW_MS {
        cache_set(dir, oauth.access_token.clone(), oauth.expires_at);
        return Ok(oauth.access_token);
    }

    let tokens = refresh_http(&oauth.refresh_token).await?;
    let expires_at_ms = now_ms() + tokens.expires_in.saturating_mul(1000);
    let refresh = tokens
        .refresh_token
        .as_deref()
        .unwrap_or(&oauth.refresh_token);
    store.write(root, &tokens.access_token, refresh, expires_at_ms)?;
    cache_set(dir, tokens.access_token.clone(), expires_at_ms);
    Ok(tokens.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex as StdMutex;

    /// Serialize tests that touch process-wide cache / env.
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    fn clear_cache() {
        if let Ok(mut g) = CACHE.write() {
            g.clear();
        }
    }

    fn write_creds(path: &Path, access: &str, refresh: &str, expires_at: i64) {
        let v = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": expires_at,
                "scopes": ["user:inference"],
                "subscriptionType": "max"
            },
            "otherKey": "keep-me"
        });
        std::fs::write(path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    }

    #[test]
    fn env_token_serves_only_the_default_login() {
        let t = || Some("  sk-ant-oat01-abc\n".to_owned());
        assert_eq!(env_token(None, t()).as_deref(), Some("sk-ant-oat01-abc"));
        assert_eq!(env_token(Some(" "), t()).as_deref(), Some("sk-ant-oat01-abc"));
        assert_eq!(env_token(Some("~/.claude-team"), t()), None);
        assert_eq!(env_token(None, Some("  ".into())), None);
        assert_eq!(env_token(None, None), None);
    }

    #[test]
    fn preserves_sibling_keys_on_write() {
        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        write_creds(&path, "old-access", "old-refresh", 1);
        let root = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        write_oauth(&path, root, "new-access", "new-refresh", 99).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["otherKey"], "keep-me");
        assert_eq!(after["claudeAiOauth"]["accessToken"], "new-access");
        assert_eq!(after["claudeAiOauth"]["refreshToken"], "new-refresh");
        assert_eq!(after["claudeAiOauth"]["expiresAt"], 99);
        assert_eq!(after["claudeAiOauth"]["scopes"][0], "user:inference");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Service name for a `CLAUDE_CONFIG_DIR` setting as Claude Code 2.1.304 derives it: `uB()` in
    /// the binary hashes `(process.env.CLAUDE_CONFIG_DIR ?? join(homedir(), ".claude")).normalize("NFC")`
    /// untouched, and is plain only when the variable is unset or empty. Expected hex is
    /// `sha256(NFC(value))[:8]`, computed outside this code (python hashlib).
    fn service_for(setting: Option<&str>, ambient: Option<&str>) -> String {
        keychain_candidates(setting, ambient, Path::new("/Users/u")).unwrap().primary
    }

    #[test]
    fn keychain_service_matches_claude_code() {
        // No variable: the plain item.
        assert_eq!(service_for(None, None), "Claude Code-credentials");
        // A real Claude Code item for a secondary directory.
        assert_eq!(service_for(Some("/Users/dfrazer/.claudetvg"), None), "Claude Code-credentials-986364fc");
        // `~/` is expanded the way a shell does before Claude Code sees it.
        assert_eq!(service_for(Some("~/.claudetvg"), None), service_for(Some("/Users/u/.claudetvg"), None));
        // Claude Code hashes the string as given: slash, `.` and `//` variants are different items.
        assert_eq!(service_for(Some("/Users/u/.claudetvg"), None), "Claude Code-credentials-facf62bf");
        assert_eq!(service_for(Some("/Users/u/.claudetvg/"), None), "Claude Code-credentials-23e8fe31");
        assert_eq!(service_for(Some("/Users/u/./.claudetvg"), None), "Claude Code-credentials-12b52fb7");
        assert_eq!(service_for(Some("/Users/u//.claudetvg"), None), "Claude Code-credentials-be26a373");
        // Set to the default path (or a symlink to it) it is still a hashed item, not the plain one.
        assert_eq!(service_for(Some("/Users/u/.claude"), None), "Claude Code-credentials-ff2684fe");
        // The ambient variable stands in when the setting names none.
        assert_eq!(service_for(None, Some("/Users/u/.claudetvg/")), "Claude Code-credentials-23e8fe31");
        // NFC: a decomposed name hashes as its composed form (`caf\u{e9}`).
        assert_eq!(service_for(Some("/Users/u/cafe\u{301}"), None), "Claude Code-credentials-27bcdf80");
    }

    /// The ambient variable is never trimmed: only an empty one counts as unset (Claude Code's falsy
    /// test), whitespace is truthy and hashed as-is. The provider setting IS trimmed (typed into a
    /// form): a deliberate difference.
    #[test]
    fn ambient_variable_is_never_trimmed_but_the_setting_is() {
        assert_eq!(service_for(None, Some("")), "Claude Code-credentials");
        // sha256(" ")[:8], and not the plain item a trim would produce.
        assert_eq!(service_for(None, Some(" ")), "Claude Code-credentials-36a9e7f1");
        assert_eq!(service_for(None, Some("/Users/u/.claudetvg ")), "Claude Code-credentials-c73157f9");
        assert_ne!(service_for(None, Some("/Users/u/.claudetvg ")), service_for(None, Some("/Users/u/.claudetvg")));
        assert_eq!(service_for(Some("  /Users/u/.claudetvg  "), None), "Claude Code-credentials-facf62bf");
    }

    /// Only the default directory has a second candidate: the plain item. Another directory's
    /// fallback would be a different account's login.
    #[test]
    fn only_the_default_directory_has_a_fallback() {
        let home = Path::new("/Users/u");
        let c = |s: Option<&str>, a: Option<&str>| keychain_candidates(s, a, home).unwrap();
        assert_eq!(c(Some("~/.claude"), None).fallback.as_deref(), Some("Claude Code-credentials"));
        assert_eq!(c(None, Some("/Users/u/.claude/")).fallback.as_deref(), Some("Claude Code-credentials"));
        assert_eq!(c(Some("/Users/u/.claudetvg"), None).fallback, None);
        assert_eq!(c(None, None).fallback, None);
    }

    /// The resolver, against a fake store (never the real Keychain): variable set => its hashed item;
    /// unset => plain; primary absent => the fallback; both present => the primary, conflict logged.
    #[test]
    fn keychain_resolver_matrix() {
        let home = Path::new("/Users/u");
        let items = |pairs: &[(&str, &str)]| {
            let m: BTreeMap<String, String> = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
            move |svc: &str| m.get(svc).cloned()
        };
        let plain = "Claude Code-credentials";
        let hashed_default = "Claude Code-credentials-ff2684fe"; // sha256("/Users/u/.claude")
        let tvg = "Claude Code-credentials-facf62bf";
        // env set: its hashed item, even though the plain one exists too (another account's).
        let c = keychain_candidates(Some("/Users/u/.claudetvg"), None, home).unwrap();
        assert_eq!(pick_service(&c, &items(&[(tvg, "tvg"), (plain, "default")])), tvg);
        // env unset: plain.
        let c = keychain_candidates(None, Some(""), home).unwrap();
        assert_eq!(pick_service(&c, &items(&[(plain, "default"), (hashed_default, "x")])), plain);
        // A non-default directory whose item is absent does NOT borrow the plain item.
        let c = keychain_candidates(Some("/Users/u/.claudetvg"), None, home).unwrap();
        assert_eq!(pick_service(&c, &items(&[(plain, "default")])), tvg);
        // Default directory named by the variable, primary absent => fallback to plain, logged.
        let c = keychain_candidates(Some("~/.claude"), None, home).unwrap();
        assert_eq!(pick_service(&c, &items(&[(plain, "default")])), plain);
        assert!(KEYCHAIN_CHOICE_LOGS.lock().unwrap().iter().any(|l| l == &format!("claude keychain: using {plain} (fallback; primary absent)")));
        // Both present and different: the primary wins and the conflict is logged.
        assert_eq!(pick_service(&c, &items(&[(plain, "default"), (hashed_default, "other")])), hashed_default);
        let logs = KEYCHAIN_CHOICE_LOGS.lock().unwrap().clone();
        assert!(logs.iter().any(|l| l.starts_with(&format!("claude keychain: using {hashed_default} (primary; conflict"))), "{logs:?}");
        // Logged once, not per lookup.
        let n = logs.len();
        pick_service(&c, &items(&[(plain, "default"), (hashed_default, "other")]));
        assert_eq!(KEYCHAIN_CHOICE_LOGS.lock().unwrap().len(), n);
        // Both present and the same: just the primary.
        assert_eq!(pick_service(&c, &items(&[(plain, "same"), (hashed_default, "same")])), hashed_default);
    }

    #[test]
    fn securestorage_variable_is_warned_about() {
        assert_eq!(securestorage_warning(true), Some("CLAUDE_SECURESTORAGE_CONFIG_DIR set; unmodelled — Keychain lookups may miss"));
        assert_eq!(securestorage_warning(false), None);
    }

    /// `access_token` threads the directory setting to the item lookup: a trailing-slash setting
    /// reads the `…-23e8fe31` item, not the one for the normalized path. Fake Keychain only.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn access_token_reads_the_item_named_by_the_setting() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_cache();
        let _env = ENV_LOCK.lock().await;
        set_test_env(Some(PathBuf::from("/Users/u")), Some(String::new()));
        let creds = |t: &str| format!(r#"{{"claudeAiOauth":{{"accessToken":"{t}","refreshToken":"r","expiresAt":{}}}}}"#, now_ms() + 3_600_000);
        *FAKE_KEYCHAIN.lock().unwrap() = BTreeMap::from([
            ("Claude Code-credentials-23e8fe31".to_owned(), creds("tok-slash")),
            ("Claude Code-credentials-facf62bf".to_owned(), creds("tok-normalized")),
        ]);
        let got = access_token(Some("/Users/u/.claudetvg/")).await.unwrap();
        FAKE_KEYCHAIN.lock().unwrap().clear();
        set_test_env(None, None);
        clear_cache();
        assert_eq!(got, "tok-slash");
    }

    /// The tier lookup reads the item the token lookup reads, for every class where the two used to
    /// disagree (a tier-side guess from the resolved path). Same raw setting in, same item out.
    /// Fake store only.
    #[test]
    fn tier_and_token_lookups_pick_the_same_item() {
        let home = Path::new("/Users/u");
        let creds = |tier: &str| format!(r#"{{"claudeAiOauth":{{"accessToken":"t","refreshToken":"r","expiresAt":1,"rateLimitTier":"{tier}"}}}}"#);
        // (setting, ambient, the service Claude Code uses, so the one holding "right")
        let classes: [(Option<&str>, Option<&str>, String); 6] = [
            (Some("~/.claudetvg/"), None, service_for(Some("/Users/u/.claudetvg/"), None)),
            (Some("/Users/u/.claudetvg/"), None, "Claude Code-credentials-23e8fe31".into()),
            (Some("~/.claude"), None, "Claude Code-credentials-ff2684fe".into()),
            (Some("/Users/u/.claude/"), None, "Claude Code-credentials-e28f4e3b".into()),
            (None, Some("/Users/u/.claudetvg "), "Claude Code-credentials-c73157f9".into()),
            (None, Some("/Users/u/.claudetvg"), "Claude Code-credentials-facf62bf".into()),
        ];
        for (setting, ambient, right) in classes {
            // The item Claude Code reads holds "right"; the normalized-path item holds "wrong".
            let store: BTreeMap<String, String> = BTreeMap::from([
                ("Claude Code-credentials-facf62bf".to_owned(), creds("wrong")),
                ("Claude Code-credentials".to_owned(), creds("wrong")),
            ])
            .into_iter()
            .chain([(right.clone(), creds("right"))])
            .collect();
            let get = |svc: &str| store.get(svc).cloned();
            let chosen = chosen_service(setting, ambient, home, &get).unwrap();
            assert_eq!(chosen, right, "token side: {setting:?} {ambient:?}");
            assert_eq!(keychain_tier_with(setting, ambient, home, &get).as_deref(), Some("right"), "tier side: {setting:?} {ambient:?}");
        }
    }

    /// `account_for` carries the setting to the tier lookup: with a trailing-slash setting the
    /// profile's tier matches the item named for that exact string; reading the normalized path's
    /// item or the plain one instead would contradict the profile and leave the account unidentified.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn account_for_reads_the_tier_from_the_item_the_setting_names() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _env = ENV_LOCK.lock().await;
        let base = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        let dir = base.join(".claudetvg");
        std::fs::create_dir_all(&dir).unwrap();
        let profile = serde_json::json!({ "oauthAccount": {
            "accountUuid": "u1", "organizationUuid": "o1", "userRateLimitTier": "tier-right"
        }});
        std::fs::write(dir.join(".claude.json"), profile.to_string()).unwrap();
        set_test_env(Some(base.clone()), Some(String::new()));
        let setting = format!("{}/", dir.to_string_lossy());
        let creds = |tier: &str| format!(r#"{{"claudeAiOauth":{{"accessToken":"t","refreshToken":"r","expiresAt":1,"rateLimitTier":"{tier}"}}}}"#);
        *FAKE_KEYCHAIN.lock().unwrap() = BTreeMap::from([
            (keychain_service(Some(&setting)), creds("tier-right")),
            (keychain_service(Some(&dir.to_string_lossy())), creds("tier-wrong")),
            (keychain_service(None), creds("tier-wrong")),
        ]);
        let got = account_for(Some(&setting)).unwrap();
        FAKE_KEYCHAIN.lock().unwrap().clear();
        set_test_env(None, None);
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(got.0, "u1:o1");
    }

    #[test]
    fn tier_setting_guess_only_for_callers_with_a_bare_path() {
        let home = Path::new("/Users/u");
        let g = |d: &str, a: Option<&str>| tier_setting_guess(Path::new(d), a, home);
        assert_eq!(g("/Users/u/.claude", None), None);
        assert_eq!(g("/Users/u/.claude", Some("")), None);
        assert_eq!(g("/Users/u/.claude", Some("/Users/u/.claude")), Some("/Users/u/.claude".into()));
        assert_eq!(g("/Users/u/.claudetvg", None), Some("/Users/u/.claudetvg".into()));
    }

    #[test]
    fn account_comes_from_claude_json() {
        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(account(&dir), (dir.to_string_lossy().into_owned(), None), "no record: the path");
        let v = serde_json::json!({ "oauthAccount": { "accountUuid": "u1", "organizationUuid": "o1", "organizationName": "Team" } });
        std::fs::write(dir.join(".claude.json"), v.to_string()).unwrap();
        assert_eq!(account(&dir), ("u1:o1".into(), Some("Team".into())));
        // Caught mid-write: still the same account.
        std::fs::write(dir.join(".claude.json"), "{\"oauthAcc").unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(dir.join(".claude.json")).unwrap().set_modified(later).unwrap();
        assert_eq!(account(&dir), ("u1:o1".into(), Some("Team".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_account_key_detects_config_dir_fallbacks() {
        assert!(is_path_account_key("/home/molly/.claude"));
        assert!(is_path_account_key("~/foo/.claude"));
        // A Windows member's path key counts as machine-local on a unix receiver too.
        assert!(is_path_account_key(r"C:\Users\DavidFrazer\.claude"));
        assert!(!is_path_account_key("d6f72690-3451-4e35-afa6-d653c2ce62ad:25c4ff7e-72e8-4c05-b845-c761306a53cc"));
        assert!(!is_path_account_key("u2:o2"));
    }

    /// A stored login whose rate-limit tier the profile doesn't describe is a different
    /// account than the profile claims: the directory's path is used instead, so reports
    /// from that login never land under the profile's account key.
    #[test]
    fn a_login_the_profile_does_not_describe_stays_unidentified() {
        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let profile = serde_json::json!({ "oauthAccount": {
            "accountUuid": "u1", "organizationUuid": "o1", "organizationName": "Team",
            "userRateLimitTier": "default_claude_max_5x", "organizationRateLimitTier": "default_raven"
        }});
        let login = |tier: &str| {
            serde_json::json!({ "claudeAiOauth": {
                "accessToken": "t", "refreshToken": "r", "expiresAt": 1, "rateLimitTier": tier
            }})
            .to_string()
        };
        std::fs::write(dir.join(".claude.json"), profile.to_string()).unwrap();
        std::fs::write(dir.join(".credentials.json"), login("default_claude_max_5x")).unwrap();
        assert_eq!(account(&dir).0, "u1:o1", "a matching tier: the portable account");
        assert_eq!(account(&dir).1.as_deref(), Some("Team"));
        // Re-signed into another plan; `.claude.json` still describes the old login. The
        // credentials' mtime is the re-sign-in signal, so move it explicitly (writes in a
        // test can share a timestamp on coarse filesystems).
        let bump = |at: std::time::SystemTime| {
            let f = std::fs::File::options().write(true).open(dir.join(".credentials.json")).unwrap();
            f.set_modified(at).unwrap();
        };
        std::fs::write(dir.join(".credentials.json"), login("default_claude_ai")).unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(10);
        bump(later);
        assert_eq!(
            account(&dir),
            (dir.to_string_lossy().into_owned(), None),
            "a login the profile doesn't describe: never the profile's key"
        );
        // The login catches up with the profile: the portable key returns.
        std::fs::write(dir.join(".credentials.json"), login("default_claude_max_5x")).unwrap();
        bump(later + std::time::Duration::from_secs(10));
        assert_eq!(account(&dir).0, "u1:o1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A long-running `claude` process on another account can flush its stale in-memory
    /// `oauthAccount` into `.claude.json` even though this directory's credentials have not
    /// changed. Once `account` has verified a profile against this directory's login tier, a
    /// contradicted profile overwrite keeps the verified portable key instead of flipping
    /// accounts or dropping to a path key.
    #[test]
    fn stale_profile_overwrite_keeps_verified_account() {
        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pro_profile = serde_json::json!({ "oauthAccount": {
            "accountUuid": "u1", "organizationUuid": "o-pro", "organizationName": "Personal",
            "organizationRateLimitTier": "default_claude_ai"
        }});
        let team_profile = serde_json::json!({ "oauthAccount": {
            "accountUuid": "u1", "organizationUuid": "o-team", "organizationName": "Vizius",
            "userRateLimitTier": "default_claude_max_5x", "organizationRateLimitTier": "default_raven"
        }});
        let login = serde_json::json!({ "claudeAiOauth": {
            "accessToken": "t", "refreshToken": "r", "expiresAt": 1, "rateLimitTier": "default_claude_ai"
        }});
        std::fs::write(dir.join(".claude.json"), pro_profile.to_string()).unwrap();
        std::fs::write(dir.join(".credentials.json"), login.to_string()).unwrap();
        assert_eq!(account(&dir), ("u1:o-pro".to_owned(), Some("Personal".to_owned())));

        // Stale writer overwrites `.claude.json` with `o-team`, while `.credentials.json` stays on `default_claude_ai`.
        std::fs::write(dir.join(".claude.json"), team_profile.to_string()).unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(dir.join(".claude.json"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(
            account(&dir),
            ("u1:o-pro".to_owned(), Some("Personal".to_owned())),
            "contradicted overwrite must keep the verified account for this login tier"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_only_credentials_file_is_not_a_claude_login() {
        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(".credentials.json");
        assert!(!has_file_login(&file));
        std::fs::write(&file, r#"{"mcpOAuth":{"srv":{"accessToken":"x"}}}"#).unwrap();
        assert!(!has_file_login(&file));
        #[cfg(target_os = "macos")]
        assert!(matches!(Store::locate(&dir, None).unwrap(), Store::Keychain { .. }));
        std::fs::write(&file, r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":1}}"#).unwrap();
        assert!(has_file_login(&file));
        assert!(matches!(Store::locate(&dir, None).unwrap(), Store::File(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No credentials at all (or a profile without tiers) leaves the profile's key alone:
    /// there is nothing to contradict it.
    #[test]
    fn without_a_readable_login_the_profile_key_stands() {
        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let profile = serde_json::json!({ "oauthAccount": {
            "accountUuid": "u1", "organizationUuid": "o1", "userRateLimitTier": "default_claude_max_5x"
        }});
        std::fs::write(dir.join(".claude.json"), profile.to_string()).unwrap();
        assert_eq!(account(&dir).0, "u1:o1");
        // A profile that names no tiers cannot be contradicted.
        let bare = serde_json::json!({ "oauthAccount": { "accountUuid": "u2", "organizationUuid": "o2" } });
        let dir2 = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(dir2.join(".claude.json"), bare.to_string()).unwrap();
        std::fs::write(
            dir2.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":1,"rateLimitTier":"pro"}}"#,
        )
        .unwrap();
        assert_eq!(account(&dir2).0, "u2:o2");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn resolve_dir_expands_home() {
        let _env = ENV_LOCK.blocking_lock();
        let home = home().unwrap();
        assert_eq!(resolve_dir(Some(" ~/.claudetvg ")).unwrap(), home.join(".claudetvg"));
        assert_eq!(resolve_dir(Some("/x/y")).unwrap(), PathBuf::from("/x/y"));
        assert_eq!(resolve_dir(Some("")).unwrap(), config_dir().unwrap());
    }

    /// Two directories are two accounts: neither's cached token answers for the other.
    #[tokio::test]
    async fn caches_each_account_separately() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_cache();
        let base = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        let (a, b) = (base.join("a"), base.join("b"));
        for (d, t) in [(&a, "token-a"), (&b, "token-b")] {
            std::fs::create_dir_all(d).unwrap();
            write_creds(&d.join(".credentials.json"), t, "r", now_ms() + 60 * 60 * 1000);
        }
        assert_eq!(access_token(a.to_str()).await.unwrap(), "token-a");
        assert_eq!(access_token(b.to_str()).await.unwrap(), "token-b");
        assert_eq!(access_token(a.to_str()).await.unwrap(), "token-a");
        clear_cache();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn read_oauth_requires_login() {
        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        std::fs::write(&path, r#"{"foo":1}"#).unwrap();
        let err = read_oauth(&path).unwrap_err().to_string();
        assert!(err.contains("no Claude Code login"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn returns_fresh_token_without_http() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_cache();
        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        write_creds(&path, "fresh-token", "r", now_ms() + 60 * 60 * 1000);
        let t = access_token(dir.to_str()).await.unwrap();
        assert_eq!(t, "fresh-token");
        clear_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live check against Anthropic with the machine's Claude Code login.
    /// `cargo test --lib live_claude_login -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_claude_login_models_and_chat() {
        let token = access_token(None).await.expect("Claude Code login");
        let models = crate::providers::discover_models(
            &crate::db::ProviderKind::Anthropic,
            "https://api.anthropic.com/v1",
            Some(&token),
            crate::providers::Auth::ClaudeLogin,
        )
        .await
        .expect("discover models");
        assert!(!models.is_empty(), "expected at least one model");
        eprintln!("discovered {} models; first={}", models.len(), models[0].id);

        let client = reqwest::Client::new();
        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .bearer_auth(&token)
            .header("anthropic-version", crate::providers::ANTHROPIC_VERSION)
            .header("anthropic-beta", OAUTH_BETA)
            .json(&serde_json::json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 32,
                "messages": [{"role": "user", "content": "Reply with exactly: ok"}]
            }))
            .send()
            .await
            .expect("messages request");
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "messages {status}: {body}");
        assert!(body.contains("ok") || body.contains("OK") || body.contains("text"), "{body}");
        eprintln!("chat reply ok ({status})");
    }

    #[tokio::test]
    async fn refreshes_expired_token_and_rewrites_file() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_cache();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.contains("refresh_token"));
            assert!(req.contains(CLIENT_ID));
            let body = r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });

        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        write_creds(&path, "stale", "old-refresh", now_ms() - 1000);

        // SAFETY: single-threaded under TEST_LOCK; restored below.
        unsafe {
            std::env::set_var(
                "PATCHBAY_CLAUDE_TOKEN_URL",
                format!("http://{addr}/v1/oauth/token"),
            );
        }
        let t = access_token(dir.to_str()).await.unwrap();
        unsafe {
            std::env::remove_var("PATCHBAY_CLAUDE_TOKEN_URL");
        }
        assert_eq!(t, "rotated-access");
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["claudeAiOauth"]["accessToken"], "rotated-access");
        assert_eq!(after["claudeAiOauth"]["refreshToken"], "rotated-refresh");
        assert_eq!(after["otherKey"], "keep-me");
        server.join().unwrap();
        clear_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
