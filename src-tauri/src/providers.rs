//! Provider configuration: secrets, model discovery, mapping to `patchbay_llm`'s `Config`.

use anyhow::{Context, Result, anyhow, bail};
use patchbay_llm::{AuthScheme, SystemBlock, WireFormat};
use serde::{Deserialize, Serialize};

use crate::db::{AuthMode, Db, Model, Provider, ProviderKind};

const KEYRING_SERVICE: &str = "dev.patchbay.app";

/// Set once per process from the data folder (`open_locked_state`): None for the app's default
/// folder, which keeps the unscoped names existing installs use; otherwise a tag so another
/// instance (`patchbayd --data-dir`, a scratch build) cannot read or overwrite this computer's
/// provider, MCP and web-search secrets. The app and daemon open exactly one folder.
static KEYRING_SCOPE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// What `set_keyring_scope` was given: None until some state has been opened in this process.
#[cfg(test)]
pub fn keyring_scope_for_tests() -> Option<Option<String>> {
    KEYRING_SCOPE.get().cloned()
}

/// Record this process's scope. A second call with a DIFFERENT scope is an error, never a silent
/// keep-the-first: two data folders in one process would share one set of keychain names. Tests open
/// many folders in one process, so under `cfg(test)` the first scope silently wins instead.
pub fn set_keyring_scope(scope: Option<String>) -> Result<()> {
    let r = set_scope_in(&KEYRING_SCOPE, scope);
    if cfg!(test) { Ok(()) } else { r }
}

fn set_scope_in(cell: &std::sync::OnceLock<Option<String>>, scope: Option<String>) -> Result<()> {
    let first = cell.get_or_init(|| scope.clone());
    if *first == scope {
        Ok(())
    } else {
        bail!("this process already scopes its keychain names as {first:?}; refusing {scope:?}: one data folder per process")
    }
}

const COPIED_SETTING: &str = "keyring.unscoped_copied";

/// Every keyring account this instance's database refers to: each provider's key and extras, each
/// MCP secret, and the web-search key. The only accounts a migration may look at. An error reading
/// the database is an error, not an empty list: it must not read as "nothing to copy".
fn referenced_accounts(db: &Db) -> Result<Vec<String>> {
    let mut out = Vec::new();
    // `set_web_search_key` always writes the inline setting (blank when the key is in the keychain),
    // so its presence is this database saying it has a web-search key.
    if db.get_setting(WEB_SEARCH_INLINE)?.is_some() {
        out.push(WEB_SEARCH_ACCOUNT.to_owned());
    }
    for (p, _) in db.list_providers()? {
        out.push(extras_account(&p.id));
        out.push(p.id);
    }
    for s in db.list_mcp_servers()? {
        let Ok(cfg) = serde_json::from_value::<crate::code::mcp::McpConfig>(s.config) else { continue };
        for (section, list) in [("env", &cfg.env), ("header", &cfg.headers)] {
            out.extend(list.iter().filter(|kv| kv.secret).map(|kv| crate::code::mcp::secret_account(&s.id, section, &kv.name)));
        }
    }
    Ok(out)
}

/// One-time COPY of the unscoped keychain entries a non-default data folder stored before keychain
/// accounts were scoped (`patchbayd --data-dir` next to the app). For each account this database
/// refers to that has no scoped entry yet, the unscoped one is copied to the scoped name. Never
/// deletes the original (the default app may own it, and synced provider ids can share it), never
/// reads an account the database does not refer to, never writes an unscoped name. Runs once per
/// instance: recorded in its settings only when nothing failed, so a keychain or database error
/// (as opposed to a missing entry) is retried at the next start and never overwrites a scoped
/// entry it could not read. `get` and `set` take raw account names. Returns how many were copied.
fn copy_unscoped_with(
    db: &Db,
    scope: Option<&str>,
    get: &dyn Fn(&str) -> Result<Option<String>, String>,
    set: &dyn Fn(&str, &str) -> bool,
) -> usize {
    let Some(scope) = scope else { return 0 };
    if matches!(db.get_setting(COPIED_SETTING), Ok(Some(_))) {
        return 0;
    }
    let accounts = match referenced_accounts(db) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("keychain copy skipped, could not read the database: {e:#}");
            return 0;
        }
    };
    let (mut copied, mut failed) = (0, false);
    for account in accounts {
        let scoped = scoped_with(Some(scope), &account);
        match get(&scoped) {
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("keychain copy: could not read {scoped}: {e}");
                failed = true;
                continue;
            }
        }
        match get(&account) {
            Ok(Some(value)) if set(&scoped, &value) => copied += 1,
            Ok(Some(_)) => failed = true,
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("keychain copy: could not read {account}: {e}");
                failed = true;
            }
        }
    }
    if !failed {
        let _ = db.set_setting(COPIED_SETTING, "1");
    }
    copied
}

/// `copy_unscoped_with` against the OS keychain, for this process's scope. Called once at startup
/// (not in tests, which open states against the real keychain).
#[cfg_attr(test, allow(dead_code))]
pub fn copy_unscoped_keys(db: &Db) {
    let raw_get = |a: &str| match keyring::Entry::new(KEYRING_SERVICE, a).and_then(|e| e.get_password()) {
        Ok(v) => Ok(Some(v).filter(|v| !v.is_empty())),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    };
    let raw_set = |a: &str, v: &str| keyring::Entry::new(KEYRING_SERVICE, a).and_then(|e| e.set_password(v)).is_ok();
    let scope = KEYRING_SCOPE.get().and_then(|s| s.as_deref());
    let n = copy_unscoped_with(db, scope, &raw_get, &raw_set);
    if n > 0 {
        tracing::info!("copied {n} keychain entries from the unscoped names to this data folder's; the originals stay");
    }
}

/// The keyring account for `account` under this process's scope. `mesh:` accounts arrive already
/// scoped by `OsKeyStore`, so they pass through (scoping twice would orphan existing daemons' keys).
fn scoped(account: &str) -> String {
    scoped_with(KEYRING_SCOPE.get().and_then(|s| s.as_deref()), account)
}

fn scoped_with(scope: Option<&str>, account: &str) -> String {
    match scope {
        Some(s) if !account.starts_with("mesh:") => format!("{account}#{s}"),
        _ => account.to_owned(),
    }
}
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";

/// Store a provider API key in the OS keyring. Falls back to the DB when no keyring service is
/// available (e.g. headless Linux without a Secret Service daemon).
pub fn store_key(db: &Db, provider: &Provider, key: &str) -> Result<()> {
    let stored = keyring::Entry::new(KEYRING_SERVICE, &scoped(&provider.id))
        .and_then(|e| e.set_password(key))
        .is_ok();
    if stored {
        db.clear_inline_key(&provider.id)?;
        db.upsert_provider(provider, None)?;
    } else {
        tracing::warn!(provider = %provider.id, "keyring unavailable; storing key in database");
        db.upsert_provider(provider, Some(key))?;
    }
    Ok(())
}

pub fn delete_key(provider_id: &str) {
    if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, &scoped(provider_id)) {
        let _ = e.delete_credential();
    }
}

pub fn load_key(provider_id: &str, inline: Option<&str>) -> Option<String> {
    if let Some(k) = inline.filter(|k| !k.is_empty()) {
        return Some(k.to_owned());
    }
    keyring::Entry::new(KEYRING_SERVICE, &scoped(provider_id))
        .and_then(|e| e.get_password())
        .ok()
        .filter(|k| !k.is_empty())
}

/// Store a secret under `account`; false when no keyring is available.
pub fn keyring_set(account: &str, value: &str) -> bool {
    keyring::Entry::new(KEYRING_SERVICE, &scoped(account))
        .and_then(|e| e.set_password(value))
        .is_ok()
}

pub fn keyring_get(account: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, &scoped(account))
        .and_then(|e| e.get_password())
        .ok()
        .filter(|v| !v.is_empty())
}

pub fn keyring_delete(account: &str) {
    if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, &scoped(account)) {
        let _ = e.delete_credential();
    }
}

const WEB_SEARCH_ACCOUNT: &str = "web-search.brave";
const WEB_SEARCH_INLINE: &str = "web_search.inline_key";

/// The Brave Search API key and where it came from: the `BRAVE_API_KEY` environment variable,
/// else the keychain, else the database (when no keychain is available).
pub fn web_search_key(db: &Db) -> Option<(String, &'static str)> {
    let env = std::env::var("BRAVE_API_KEY").ok().map(|k| k.trim().to_owned()).filter(|k| !k.is_empty());
    if let Some(k) = env {
        return Some((k, "env"));
    }
    if let Some(k) = keyring_get(WEB_SEARCH_ACCOUNT) {
        return Some((k, "keychain"));
    }
    db.get_setting(WEB_SEARCH_INLINE).ok().flatten().filter(|k| !k.is_empty()).map(|k| (k, "database"))
}

pub fn set_web_search_key(db: &Db, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        return Err(anyhow!("the key is empty"));
    }
    if keyring_set(WEB_SEARCH_ACCOUNT, key) {
        db.set_setting(WEB_SEARCH_INLINE, "")?;
    } else {
        tracing::warn!("keyring unavailable; storing the web search key in the database");
        db.set_setting(WEB_SEARCH_INLINE, key)?;
    }
    Ok(())
}

pub fn clear_web_search_key(db: &Db) -> Result<()> {
    keyring_delete(WEB_SEARCH_ACCOUNT);
    db.set_setting(WEB_SEARCH_INLINE, "")?;
    Ok(())
}

pub fn list_with_key_flag(db: &Db) -> Result<Vec<Provider>> {
    Ok(db
        .list_providers()?
        .into_iter()
        .map(|(mut p, inline)| {
            p.has_key = load_key(&p.id, inline.as_deref()).is_some();
            p
        })
        .collect())
}

pub fn normalize_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_owned()
}

pub fn wire_format(kind: &ProviderKind) -> WireFormat {
    match kind {
        ProviderKind::OpenaiChat => WireFormat::ChatCompletions,
        ProviderKind::OpenaiResponses => WireFormat::Responses,
        ProviderKind::Anthropic => WireFormat::Anthropic,
        ProviderKind::Gemini => WireFormat::Gemini,
    }
}

/// Whether the provider's own web search can be offered to this model: only the vendors' own
/// endpoints have one, not other servers that speak the same wire format.
pub fn native_web_search(r: &Resolved) -> bool {
    let url = r.provider.base_url.to_ascii_lowercase();
    match r.provider.kind {
        ProviderKind::Anthropic => r.auth == Auth::ClaudeLogin || url.contains("api.anthropic.com"),
        ProviderKind::OpenaiResponses => url.contains("api.openai.com"),
        ProviderKind::Gemini => url.contains("generativelanguage.googleapis.com"),
        ProviderKind::OpenaiChat => false,
    }
}

/// How a resolved provider authenticates for sampling / discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    Key,
    ClaudeLogin,
    GrokLogin,
}

/// A provider's extra request headers and query parameters, with their values. The values are
/// secrets (an `api-key`, a gateway token): they live only in the keyring (`extras_account`), never
/// in the database, the mesh sync or a log line, and `Debug` shows names only.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Extras {
    pub headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
}

impl std::fmt::Debug for Extras {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = |l: &[(String, String)]| l.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
        f.debug_struct("Extras").field("headers", &names(&self.headers)).field("query", &names(&self.query)).finish()
    }
}

impl Extras {
    pub fn names(&self) -> crate::db::ExtraNames {
        crate::db::ExtraNames {
            headers: self.headers.iter().map(|(n, _)| n.clone()).collect(),
            query: self.query.iter().map(|(n, _)| n.clone()).collect(),
        }
    }
}

/// One entry typed in Settings. `value` `None` (or empty) keeps the value already stored under the
/// same name, so a provider can be edited without typing its secrets again.
#[derive(Debug, Clone, Deserialize)]
pub struct ExtraEntry {
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
}

/// The keyring account that holds a provider's extras (JSON). Not an account `reseal` syncs.
pub fn extras_account(provider_id: &str) -> String {
    format!("extras:{provider_id}")
}

/// Headers Patchbay sets itself, for every provider. `authorization` is the bearer the key goes
/// out under (Claude login too): a second one would reach the server next to the real one.
const RESERVED_HEADERS: [&str; 7] =
    ["host", "content-length", "content-type", "accept", "transfer-encoding", "connection", "authorization"];

/// Whether `name` (any case) is a header Patchbay sets for this kind of provider. The key header
/// differs by kind (`AuthScheme` in `patchbay-llm` `Config::new`: Anthropic `x-api-key`, Gemini
/// `x-goog-api-key`, everything else a bearer), so only that kind's own key header is reserved.
/// OpenAI-compatible providers, Azure OpenAI included, get nothing but a bearer from their auth
/// mode, so Azure's `api-key` (and `x-api-key` for a gateway) can only arrive through extras.
fn reserved_header(kind: &ProviderKind, name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    RESERVED_HEADERS.contains(&n.as_str())
        || match kind {
            ProviderKind::Anthropic => n == "x-api-key" || n.starts_with("anthropic-"),
            ProviderKind::Gemini => n == "x-goog-api-key",
            _ => false,
        }
}
const MAX_NAME: usize = 128;
const MAX_VALUE: usize = 4096;

fn valid_header_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= MAX_NAME && n.chars().all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+.^_`|~-".contains(c))
}

fn valid_value(v: &str) -> bool {
    v.len() <= MAX_VALUE && !v.chars().any(|c| c.is_control())
}

/// Merge typed entries with what is stored: a new value replaces, no value keeps.
fn merge(kind: &ProviderKind, entries: &[ExtraEntry], old: &[(String, String)], what: &str, header: bool) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for e in entries {
        let name = e.name.trim();
        if header {
            if !valid_header_name(name) {
                bail!("{name:?} is not a valid header name");
            }
            if reserved_header(kind, name) {
                bail!("the {name} header is set by Patchbay and can't be replaced");
            }
        } else if name.is_empty() || name.len() > MAX_NAME || name.chars().any(|c| c.is_control()) {
            bail!("{name:?} is not a valid query parameter name");
        }
        if out.iter().any(|(n, _)| if header { n.eq_ignore_ascii_case(name) } else { n == name }) {
            bail!("the {what} {name} is listed twice");
        }
        let value = match e.value.as_deref().filter(|v| !v.is_empty()) {
            Some(v) => v.to_owned(),
            None => old
                .iter()
                .find(|(n, _)| if header { n.eq_ignore_ascii_case(name) } else { n == name })
                .map(|(_, v)| v.clone())
                .ok_or_else(|| anyhow!("the {what} {name} has no value yet"))?,
        };
        if !valid_value(&value) {
            bail!("the value of the {what} {name} is too long or has control characters");
        }
        out.push((name.to_owned(), value));
    }
    Ok(out)
}

/// A keyring with no mesh scoping: provider keys and extras are named by their (synced) provider id.
pub struct Keyring;

impl crate::mesh::keys::KeyStore for Keyring {
    fn get(&self, account: &str) -> Option<String> {
        keyring_get(account)
    }
    fn set(&self, account: &str, value: &str) -> bool {
        keyring_set(account, value)
    }
    fn delete(&self, account: &str) {
        keyring_delete(account)
    }
}

/// A provider's extras with values, from the keyring; empty when none are stored or readable.
pub fn load_extras(keys: &dyn crate::mesh::keys::KeyStore, provider_id: &str) -> Extras {
    let Some(json) = keys.get(&extras_account(provider_id)) else { return Extras::default() };
    serde_json::from_str(&json).unwrap_or_else(|_| {
        // Never print the stored text: it is the secret.
        tracing::warn!(provider = %provider_id, "the stored extra headers could not be read");
        Extras::default()
    })
}

/// The extras a request needs. A provider with no stored names has none; one WITH names whose
/// values cannot be read from the keyring is an error, never an empty set: the request would go out
/// without its `api-key` and fail as a bare 401.
pub fn required_extras(db: &Db, keys: &dyn crate::mesh::keys::KeyStore, provider_id: &str) -> Result<Extras> {
    if db.provider_extra_names(provider_id)?.is_empty() {
        return Ok(Extras::default());
    }
    keys.get(&extras_account(provider_id))
        .and_then(|stored| serde_json::from_str(&stored).ok())
        .ok_or_else(|| anyhow!("provider headers could not be read from keyring"))
}

/// Save a provider's extra headers and query parameters. Values go to the keyring ONLY: with no
/// keyring this fails (unlike an API key there is no database fallback, since a header value in
/// the database is what must never happen). The names go to the provider row.
pub fn set_extras(
    db: &Db,
    keys: &dyn crate::mesh::keys::KeyStore,
    provider_id: &str,
    headers: &[ExtraEntry],
    query: &[ExtraEntry],
) -> Result<()> {
    let (provider, _) = db.get_provider(provider_id)?.ok_or_else(|| anyhow!("provider not found"))?;
    let old = load_extras(keys, provider_id);
    let next = Extras {
        headers: merge(&provider.kind, headers, &old.headers, "header", true)?,
        query: merge(&provider.kind, query, &old.query, "query parameter", false)?,
    };
    if next == Extras::default() {
        keys.delete(&extras_account(provider_id));
    } else if !keys.set(&extras_account(provider_id), &serde_json::to_string(&next)?) {
        bail!("no keyring is available, and header values are never stored in the database");
    }
    db.set_provider_extra_names(provider_id, &next.names())
}

/// Remove a provider's extras (with the provider).
pub fn delete_extras(keys: &dyn crate::mesh::keys::KeyStore, provider_id: &str) {
    keys.delete(&extras_account(provider_id));
}

/// Delete a provider: its extras leave the keyring first, then the row (and its names) go.
/// A node that learns of the deletion by sync does not run this, so its keyring entry stays (known
/// limit, no cross-node cleanup in v1).
pub fn delete_provider_with(db: &crate::db::Db, keys: &dyn crate::mesh::keys::KeyStore, provider_id: &str) -> Result<()> {
    delete_extras(keys, provider_id);
    db.delete_provider(provider_id)
}

/// Resolved model + provider + key, ready to sample.
pub struct Resolved {
    pub provider: Provider,
    pub model: Model,
    pub key: Option<String>,
    /// The provider's extra headers and query parameters (values from the keyring).
    pub extras: Extras,
    pub auth: Auth,
    /// For `Auth::GrokLogin`: the signed-in user, whom Grok's proxy wants named on every request.
    /// Filled by `ensure_token`.
    pub grok_user: Option<String>,
}

pub fn resolve(db: &Db, model_ref: &str) -> Result<Resolved> {
    let model = db
        .get_model(model_ref)?
        .ok_or_else(|| anyhow!("model {model_ref} not found; pick a model in Settings"))?;
    let (provider, inline) = db
        .get_provider(&model.provider_id)?
        .ok_or_else(|| anyhow!("provider for model {} missing", model.display_name))?;
    let auth = match provider.auth_mode {
        AuthMode::ClaudeLogin => Auth::ClaudeLogin,
        AuthMode::GrokLogin => Auth::GrokLogin,
        AuthMode::ApiKey => Auth::Key,
    };
    let key = match auth {
        Auth::ClaudeLogin | Auth::GrokLogin => None,
        Auth::Key => load_key(&provider.id, inline.as_deref()),
    };
    // Only a provider that has extras touches the keyring for them.
    let extras = required_extras(db, &Keyring, &provider.id)?;
    Ok(Resolved {
        provider,
        model,
        key,
        extras,
        auth,
        grok_user: None,
    })
}

/// Fill `resolved.key` for Claude and Grok login providers (no-op for API-key auth).
pub async fn ensure_token(resolved: &mut Resolved) -> Result<()> {
    match resolved.auth {
        Auth::ClaudeLogin => {
            resolved.key = Some(crate::claude_auth::access_token(resolved.provider.claude_config_dir.as_deref()).await?);
        }
        Auth::GrokLogin => {
            let c = crate::grok_auth::credentials(resolved.provider.grok_home.as_deref()).await?;
            resolved.key = Some(c.token);
            resolved.grok_user = Some(c.user_id);
        }
        Auth::Key => {}
    }
    Ok(())
}

/// Grok Build's version as the proxy sees it (`x-grok-client-version`): what the installed
/// `grok --version` reports, else the version this was written against. The proxy turns away
/// clients below its minimum, so a current `grok` keeps Patchbay current too.
const GROK_VERSION_FALLBACK: &str = "1.0.60";

fn grok_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        let Ok(r) = crate::code::agents::resolve(crate::code::agents::AgentKind::Grok, &Default::default()) else {
            return GROK_VERSION_FALLBACK.to_owned();
        };
        // A hung `grok` must not hold a request up: ask on a thread and give it a few seconds.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut cmd = std::process::Command::new(&r.program);
            cmd.arg("--version");
            #[cfg(test)]
            crate::test_support::isolate_std(&mut cmd);
            let _ = tx.send(cmd.output());
        });
        rx.recv_timeout(std::time::Duration::from_secs(3))
            .ok()
            .and_then(|o| o.ok())
            .filter(|o| o.status.success())
            .and_then(|o| parse_grok_version(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_else(|| GROK_VERSION_FALLBACK.to_owned())
    })
}

/// `grok 1.0.60 (01a8e1c591c0) [alpha]` -> `1.0.60`.
fn parse_grok_version(out: &str) -> Option<String> {
    let v = out.split_whitespace().nth(1)?;
    (v.split('.').count() >= 2 && v.split('.').all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))).then(|| v.to_owned())
}

/// The headers Grok Build sends to its proxy (`cli-chat-proxy.grok.com`), so a request made
/// with a Grok login looks like Grok's own. `user` is the signed-in user's id.
pub(crate) fn grok_headers(user: Option<&str>) -> Vec<(String, String)> {
    let version = grok_version();
    let mut h = vec![
        ("X-XAI-Token-Auth".to_owned(), "xai-grok-cli".to_owned()),
        ("x-authenticateresponse".to_owned(), "authenticate-response".to_owned()),
        ("x-grok-client-version".to_owned(), version.to_owned()),
        ("x-grok-client-identifier".to_owned(), "grok-shell".to_owned()),
        ("x-grok-client-mode".to_owned(), "headless".to_owned()),
        (
            "User-Agent".to_owned(),
            format!("grok-shell/{version} ({}; {})", std::env::consts::OS, std::env::consts::ARCH),
        ),
    ];
    if let Some(u) = user.filter(|u| !u.is_empty()) {
        h.push(("x-userid".to_owned(), u.to_owned()));
        h.push(("x-grok-user-id".to_owned(), u.to_owned()));
    }
    h
}

/// Subscription OAuth tokens only serve requests whose first system block is exactly this line.
/// Anything else (no system prompt, or this text merged into a longer block) gets a bare
/// `429 rate_limit_error: Error`.
pub const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// System blocks the provider needs ahead of the caller's own.
pub fn system_prefix(r: &Resolved) -> Vec<SystemBlock> {
    // Its own block, so the caller's system prompt follows untouched.
    if r.auth == Auth::ClaudeLogin { vec![SystemBlock::new(CLAUDE_CODE_IDENTITY)] } else { Vec::new() }
}

/// Make `req` acceptable to the provider before sending it.
pub fn prepare_request(r: &Resolved, req: &mut patchbay_llm::Request) {
    for (i, b) in system_prefix(r).into_iter().enumerate() {
        req.system.insert(i, b);
    }
}

/// How long the coding agent asks Anthropic to keep its prompt cache. A Claude subscription gets
/// the 1-hour cache, as in Claude Code: cache writes cost plan usage there, not dollars, and a
/// long-lived coding session is idle between prompts far more than 5 minutes. A stored API key
/// pays for the dearer 1-hour write, so it keeps the API's 5-minute default.
pub fn agent_cache_ttl(r: &Resolved) -> patchbay_llm::CacheTtl {
    if r.provider.kind == ProviderKind::Anthropic && r.auth == Auth::ClaudeLogin {
        patchbay_llm::CacheTtl::OneHour
    } else {
        patchbay_llm::CacheTtl::Default
    }
}

pub fn llm_config(r: &Resolved) -> patchbay_llm::Config {
    let mut cfg = patchbay_llm::Config::new(
        wire_format(&r.provider.kind),
        normalize_base_url(&r.provider.base_url),
        r.model.model_id.clone(),
    );
    cfg.key = r.key.clone();
    if r.provider.kind == ProviderKind::Anthropic {
        match r.auth {
            Auth::ClaudeLogin => {
                cfg.auth = AuthScheme::Bearer;
                cfg.extra_headers.push(("anthropic-beta".into(), ANTHROPIC_OAUTH_BETA.into()));
            }
            Auth::Key => cfg.auth = AuthScheme::XApiKey,
            // A Claude login on an Anthropic provider only; a Grok login never reaches here.
            Auth::GrokLogin => {}
        }
    }
    if r.auth == Auth::GrokLogin {
        cfg.auth = AuthScheme::Bearer;
        cfg.extra_headers.extend(grok_headers(r.grok_user.as_deref()));
        // Grok sends the model's id and groups a session's requests by header as well.
        cfg.extra_headers.push(("x-grok-model-override".into(), r.model.model_id.clone()));
        cfg.session_headers = vec!["x-grok-conv-id".into(), "x-grok-session-id".into()];
        cfg.request_id_header = Some("x-grok-req-id".into());
    }
    // The user's own headers come after Patchbay's, so an `api-key` for Azure sits beside the bearer.
    cfg.extra_headers.extend(r.extras.headers.iter().cloned());
    cfg.extra_query = r.extras.query.clone();
    let p = &r.model.params;
    cfg.max_output_tokens = p.max_output_tokens;
    cfg.temperature = p.temperature;
    cfg.top_p = p.top_p;
    cfg.effort = p.reasoning_effort();
    cfg.summary = p.reasoning_summary();
    cfg.send_reasoning = p.send_reasoning.unwrap_or(true);
    cfg
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: String,
    /// Context window when the endpoint reports one (OpenRouter, Fireworks, vLLM, Anthropic…).
    pub context_window: Option<u64>,
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelListItem>,
}

#[derive(Deserialize)]
struct ModelListItem {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

/// Endpoints spell the context window differently; take the first one present.
fn context_window_of(rest: &serde_json::Map<String, serde_json::Value>) -> Option<u64> {
    const KEYS: &[&str] = &[
        "context_length",
        "context_window",
        "max_input_tokens",
        "max_context_length",
        "max_model_len",
        "input_token_limit",
    ];
    let num = |v: &serde_json::Value| v.as_u64().or_else(|| v.as_str()?.parse().ok());
    KEYS.iter()
        .find_map(|k| rest.get(*k).and_then(num))
        .or_else(|| rest.get("top_provider")?.get("context_length").and_then(num))
        .filter(|n| *n > 0)
}

/// Gemini lists `{models: [{name: "models/x", displayName, inputTokenLimit, ...}]}`.
fn gemini_models(body: &serde_json::Value) -> Vec<DiscoveredModel> {
    body.get("models")
        .and_then(|m| m.as_array())
        .map(|ms| {
            ms.iter()
                .filter(|m| {
                    m.get("supportedGenerationMethods")
                        .and_then(|g| g.as_array())
                        .is_none_or(|g| g.iter().any(|x| x == "generateContent"))
                })
                .filter_map(|m| {
                    let name = m.get("name")?.as_str()?;
                    let id = name.strip_prefix("models/").unwrap_or(name).to_owned();
                    Some(DiscoveredModel {
                        display_name: m
                            .get("displayName")
                            .and_then(|d| d.as_str())
                            .map(str::to_owned)
                            .unwrap_or_else(|| id.clone()),
                        context_window: m.get("inputTokenLimit").and_then(|n| n.as_u64()).filter(|n| *n > 0),
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `GET {base}/models` — works for OpenAI-compatible servers, Anthropic and Gemini.
pub async fn discover_models(
    kind: &ProviderKind,
    base_url: &str,
    key: Option<&str>,
    auth: Auth,
) -> Result<Vec<DiscoveredModel>> {
    discover_models_for(kind, base_url, key, auth, None, &Extras::default()).await
}

/// `discover_models`, naming the signed-in Grok user (`Auth::GrokLogin` only).
pub async fn discover_models_for(
    kind: &ProviderKind,
    base_url: &str,
    key: Option<&str>,
    auth: Auth,
    grok_user: Option<&str>,
    extras: &Extras,
) -> Result<Vec<DiscoveredModel>> {
    let mut url = format!("{}/models", normalize_base_url(base_url));
    if *kind == ProviderKind::Gemini {
        url.push_str("?pageSize=1000");
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let mut req = client.get(&url);
    match (kind, auth, key) {
        (ProviderKind::Anthropic, Auth::ClaudeLogin, Some(k)) => {
            req = req
                .bearer_auth(k)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("anthropic-beta", ANTHROPIC_OAUTH_BETA);
        }
        (ProviderKind::Anthropic, Auth::ClaudeLogin, None) => {
            return Err(anyhow!("Claude Code login required to discover models"));
        }
        (ProviderKind::Anthropic, Auth::Key, Some(k)) => {
            req = req
                .header("x-api-key", k)
                .header("anthropic-version", ANTHROPIC_VERSION);
        }
        (ProviderKind::Anthropic, Auth::Key, None) => {
            req = req.header("anthropic-version", ANTHROPIC_VERSION);
        }
        (_, Auth::GrokLogin, Some(k)) => {
            req = req.bearer_auth(k);
            for (name, value) in grok_headers(grok_user) {
                req = req.header(name, value);
            }
        }
        (_, Auth::GrokLogin, None) => return Err(anyhow!("Grok login required to discover models")),
        (ProviderKind::Gemini, _, Some(k)) => req = req.header("x-goog-api-key", k),
        (_, _, Some(k)) => req = req.bearer_auth(k),
        (_, _, None) => {}
    }
    for (name, value) in &extras.headers {
        req = req.header(name, value);
    }
    if !extras.query.is_empty() {
        req = req.query(&extras.query);
    }
    // `without_url`: reqwest prints the full URL, and an extra query value is a secret.
    let resp = req.send().await.map_err(reqwest::Error::without_url).with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GET {url} returned {status}: {}", truncate(&body, 400)));
    }
    if *kind == ProviderKind::Gemini {
        let body: serde_json::Value = resp.json().await.context("parse model list")?;
        let mut out = gemini_models(&body);
        out.sort_by(|a, b| a.id.cmp(&b.id));
        return Ok(out);
    }
    let list: ModelList = resp.json().await.context("parse model list")?;
    let mut out: Vec<DiscoveredModel> = list
        .data
        .into_iter()
        .map(|m| DiscoveredModel {
            display_name: m
                .display_name
                // Grok's list names its models (`name: "Grok 4.7"`).
                .or_else(|| (auth == Auth::GrokLogin).then(|| m.rest.get("name").and_then(|n| n.as_str()).map(str::to_owned)).flatten())
                .unwrap_or_else(|| m.id.clone()),
            context_window: context_window_of(&m.rest),
            id: m.id,
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::{context_window_of, parse_grok_version, scoped_with};
    use super::{Extras, Resolved};

    #[test]
    fn reads_grok_s_version_from_its_banner() {
        assert_eq!(parse_grok_version("grok 1.0.60 (01a8e1c591c0) [alpha]\n").as_deref(), Some("1.0.60"));
        assert_eq!(parse_grok_version("grok 2.1.0").as_deref(), Some("2.1.0"));
        assert_eq!(parse_grok_version("grok (dev)"), None);
        assert_eq!(parse_grok_version("command not found"), None);
        assert_eq!(parse_grok_version(""), None);
    }

    #[test]
    fn a_grok_login_sends_groks_headers_and_session_ids() {
        use super::*;
        let r = Resolved {
            provider: Provider {
                id: "p".into(),
                name: "Grok".into(),
                kind: ProviderKind::OpenaiResponses,
                base_url: "https://cli-chat-proxy.grok.com/v1".into(),
                has_key: false,
                auth_mode: AuthMode::GrokLogin,
                claude_config_dir: None,
                grok_home: None,
            },
            model: Model {
                id: "m".into(),
                provider_id: "p".into(),
                model_id: "grok-4.7".into(),
                display_name: "Grok 4.7".into(),
                vision: false,
                context_window: None,
                params: Default::default(),
            },
            key: Some("tok".into()),
            extras: Extras::default(),
            auth: Auth::GrokLogin,
            grok_user: Some("user-1".into()),
        };
        let cfg = llm_config(&r);
        let get = |k: &str| cfg.extra_headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str());
        assert_eq!(cfg.format, patchbay_llm::WireFormat::Responses);
        assert!(matches!(cfg.auth, AuthScheme::Bearer));
        assert_eq!(cfg.key.as_deref(), Some("tok"));
        assert_eq!(get("X-XAI-Token-Auth"), Some("xai-grok-cli"));
        assert_eq!(get("x-grok-client-identifier"), Some("grok-shell"));
        assert_eq!(get("x-userid"), Some("user-1"));
        assert_eq!(get("x-grok-user-id"), Some("user-1"));
        assert_eq!(get("x-grok-model-override"), Some("grok-4.7"));
        assert!(get("x-grok-client-version").is_some_and(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit())));
        assert!(get("user-agent").is_some_and(|v| v.starts_with("grok-shell/")));
        assert_eq!(cfg.session_headers, ["x-grok-conv-id", "x-grok-session-id"]);
        assert_eq!(cfg.request_id_header.as_deref(), Some("x-grok-req-id"));
        // Nothing of Grok's goes to an API-key provider.
        let mut plain = r;
        plain.auth = Auth::Key;
        let cfg = llm_config(&plain);
        assert!(cfg.extra_headers.is_empty() && cfg.session_headers.is_empty() && cfg.request_id_header.is_none());
    }

    #[test]
    fn reads_context_window_variants() {
        let m = |j: serde_json::Value| j.as_object().unwrap().clone();
        assert_eq!(context_window_of(&m(serde_json::json!({"context_length": 131072}))), Some(131072));
        assert_eq!(context_window_of(&m(serde_json::json!({"max_model_len": "32768"}))), Some(32768));
        assert_eq!(
            context_window_of(&m(serde_json::json!({"top_provider": {"context_length": 200000}}))),
            Some(200000)
        );
        assert_eq!(context_window_of(&m(serde_json::json!({"owned_by": "x"}))), None);
    }

    #[test]
    fn lists_gemini_models() {
        let body = serde_json::json!({"models": [
            {"name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro", "inputTokenLimit": 1048576,
             "supportedGenerationMethods": ["generateContent", "countTokens"]},
            {"name": "models/embedding-001", "supportedGenerationMethods": ["embedContent"]},
        ]});
        let m = super::gemini_models(&body);
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].id.as_str(), m[0].context_window), ("gemini-2.5-pro", Some(1048576)));
    }

    #[test]
    fn claude_login_leads_with_claude_code_identity() {
        use super::*;
        let resolved = |auth| Resolved {
            extras: Extras::default(),
            provider: Provider {
                id: "p".into(),
                name: "Anthropic".into(),
                kind: ProviderKind::Anthropic,
                base_url: "https://api.anthropic.com/v1".into(),
                has_key: false,
                auth_mode: AuthMode::ClaudeLogin,
                claude_config_dir: None, grok_home: None,
            },
            model: Model {
                id: "m".into(),
                provider_id: "p".into(),
                model_id: "claude-opus-4-8".into(),
                display_name: "Claude Opus 4.8".into(),
                vision: true,
                context_window: None,
                params: Default::default(),
            },
            key: None,
            auth,
            grok_user: None,
        };
        let request = || patchbay_llm::Request {
            system: vec![SystemBlock::new("be terse")],
            messages: vec![patchbay_llm::Message::user("hi")],
            ..Default::default()
        };
        let systems = |req: &patchbay_llm::Request| -> Vec<String> {
            req.system.iter().map(|b| b.text.clone()).collect()
        };

        let mut req = request();
        prepare_request(&resolved(Auth::ClaudeLogin), &mut req);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(systems(&req), [CLAUDE_CODE_IDENTITY, "be terse"]);

        let mut req = request();
        prepare_request(&resolved(Auth::Key), &mut req);
        assert_eq!(systems(&req), ["be terse"]);
    }

    #[test]
    fn only_vendor_endpoints_have_native_search() {
        use super::*;
        let on = |kind, url: &str, auth| Resolved {
            extras: Extras::default(),
            provider: Provider { id: "p".into(), name: "n".into(), kind, base_url: url.into(), has_key: false, auth_mode: Default::default(), claude_config_dir: None, grok_home: None },
            model: Model {
                id: "m".into(),
                provider_id: "p".into(),
                model_id: "x".into(),
                display_name: "x".into(),
                vision: false,
                context_window: None,
                params: Default::default(),
            },
            key: None,
            auth,
            grok_user: None,
        };
        assert!(native_web_search(&on(ProviderKind::Anthropic, "https://api.anthropic.com/v1", Auth::Key)));
        assert!(native_web_search(&on(ProviderKind::Anthropic, "https://api.anthropic.com/v1", Auth::ClaudeLogin)));
        assert!(native_web_search(&on(ProviderKind::OpenaiResponses, "https://api.openai.com/v1", Auth::Key)));
        assert!(native_web_search(&on(ProviderKind::Gemini, "https://generativelanguage.googleapis.com/v1beta", Auth::Key)));
        assert!(!native_web_search(&on(ProviderKind::Anthropic, "http://localhost:4000/v1", Auth::Key)));
        assert!(!native_web_search(&on(ProviderKind::OpenaiResponses, "http://localhost:11434/v1", Auth::Key)));
        assert!(!native_web_search(&on(ProviderKind::OpenaiChat, "https://api.openai.com/v1", Auth::Key)));
    }

    // --- extra headers and query parameters (pb-20261111-34) ---

    use crate::mesh::keys::{KeyStore, MemKeyStore};

    const HEADER_SECRET: &str = "HDR-SECRET-9f3a";
    const QUERY_SECRET: &str = "QRY-SECRET-77c1";

    fn provider_in_temp_db() -> (crate::db::Db, std::path::PathBuf) {
        use super::*;
        let dir = crate::test_support::short_dir("extras");
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::Db::open(&dir.join("db.sqlite")).unwrap();
        let p = Provider {
            id: "azure".into(),
            name: "Azure".into(),
            kind: ProviderKind::OpenaiChat,
            base_url: "https://x.openai.azure.com/openai/deployments/d".into(),
            has_key: false,
            auth_mode: AuthMode::ApiKey,
            claude_config_dir: None,
            grok_home: None,
        };
        db.upsert_provider(&p, None).unwrap();
        (db, dir)
    }

    fn entry(name: &str, value: Option<&str>) -> super::ExtraEntry {
        super::ExtraEntry { name: name.into(), value: value.map(str::to_owned) }
    }

    /// Everything in every table of the database, plus the raw files (the write-ahead log can hold
    /// what the tables no longer do), as one string, for "the secret is nowhere in it".
    fn dump(db: &crate::db::Db, dir: &std::path::Path) -> String {
        let mut out = String::new();
        db.with_conn(|c| -> anyhow::Result<()> {
            let tables: Vec<String> = c
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")?
                .query_map([], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            for t in tables {
                let mut st = c.prepare(&format!("SELECT * FROM \"{t}\""))?;
                let n = st.column_count();
                let rows = st.query_map([], |r| Ok((0..n).map(|i| format!("{:?}", r.get_ref(i))).collect::<Vec<_>>().join("|")))?;
                for row in rows {
                    out.push_str(&format!("{t}: {}\n", row?));
                }
            }
            Ok(())
        })
        .unwrap();
        for f in std::fs::read_dir(dir).unwrap().flatten() {
            out.push_str(&String::from_utf8_lossy(&std::fs::read(f.path()).unwrap_or_default()));
        }
        out
    }

    #[test]
    fn extras_values_go_to_the_keyring_and_nowhere_else() {
        use super::*;
        let (db, dir) = provider_in_temp_db();
        let keys = MemKeyStore::default();
        set_extras(&db, &keys, "azure", &[entry("api-key", Some(HEADER_SECRET))], &[entry("api-version", Some("2024-10-21")), entry("code", Some(QUERY_SECRET))]).unwrap();
        let loaded = load_extras(&keys, "azure");
        assert_eq!(loaded.headers, vec![("api-key".to_owned(), HEADER_SECRET.to_owned())]);
        assert_eq!(loaded.query.len(), 2);
        assert_eq!(
            db.provider_extra_names("azure").unwrap(),
            crate::db::ExtraNames { headers: vec!["api-key".into()], query: vec!["api-version".into(), "code".into()] }
        );
        let everything = dump(&db, &dir);
        for secret in [HEADER_SECRET, QUERY_SECRET, "2024-10-21"] {
            assert!(!everything.contains(secret), "{secret} is in the database files");
        }
        assert!(everything.contains("api-key"), "the NAME is what Settings lists");
        let shown = format!("{loaded:?}");
        assert!(!shown.contains(HEADER_SECRET) && !shown.contains(QUERY_SECRET), "{shown}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn extras_are_not_synced_to_the_mesh() {
        use super::*;
        let (db, dir) = provider_in_temp_db();
        let keys = MemKeyStore::default();
        set_extras(&db, &keys, "azure", &[entry("api-key", Some(HEADER_SECRET))], &[]).unwrap();
        let table = crate::mesh::sync::TABLES.iter().find(|t| t.name == "providers").unwrap();
        assert!(!table.cols.iter().any(|c| c.contains("extra")), "{:?}", table.cols);
        let secrets = crate::mesh::secrets::local_secrets(&db, &keys).unwrap();
        assert!(!secrets.contains_key(&extras_account("azure")), "{:?}", secrets.keys().collect::<Vec<_>>());
        assert!(!secrets.values().any(|v| v.contains(HEADER_SECRET)));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A store whose keyring is unavailable (headless Linux without a Secret Service).
    struct NoKeyring;
    impl KeyStore for NoKeyring {
        fn get(&self, _: &str) -> Option<String> {
            None
        }
        fn set(&self, _: &str, _: &str) -> bool {
            false
        }
        fn delete(&self, _: &str) {}
    }

    #[test]
    fn without_a_keyring_extras_fail_loudly_and_store_nothing() {
        use super::*;
        let (db, dir) = provider_in_temp_db();
        let e = set_extras(&db, &NoKeyring, "azure", &[entry("api-key", Some(HEADER_SECRET))], &[]).unwrap_err().to_string();
        assert!(e.contains("keyring"), "{e}");
        assert!(db.provider_extra_names("azure").unwrap().is_empty(), "no names saved for values that were not");
        assert!(!dump(&db, &dir).contains(HEADER_SECRET), "no database fallback for a header value");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_blank_value_keeps_the_stored_one_and_an_empty_list_clears() {
        use super::*;
        let (db, dir) = provider_in_temp_db();
        let keys = MemKeyStore::default();
        set_extras(&db, &keys, "azure", &[entry("api-key", Some(HEADER_SECRET))], &[entry("api-version", Some("v1"))]).unwrap();
        set_extras(&db, &keys, "azure", &[entry("API-KEY", None), entry("x-team", Some("t"))], &[entry("api-version", Some("v2"))]).unwrap();
        let l = load_extras(&keys, "azure");
        assert_eq!(l.headers, vec![("API-KEY".to_owned(), HEADER_SECRET.to_owned()), ("x-team".to_owned(), "t".to_owned())]);
        assert_eq!(l.query, vec![("api-version".to_owned(), "v2".to_owned())]);
        assert!(set_extras(&db, &keys, "azure", &[entry("new", None)], &[]).is_err(), "no value stored under that name");
        set_extras(&db, &keys, "azure", &[], &[]).unwrap();
        assert_eq!(keys.get(&extras_account("azure")), None);
        assert!(db.provider_extra_names("azure").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_names_and_values_are_refused() {
        use super::*;
        let (db, dir) = provider_in_temp_db();
        let keys = MemKeyStore::default();
        let set = |h: Vec<ExtraEntry>, q: Vec<ExtraEntry>| set_extras(&db, &keys, "azure", &h, &q);
        assert!(set(vec![entry("bad name", Some("v"))], vec![]).is_err(), "space in a header name");
        assert!(set(vec![entry("x-a", Some("v\r\nx-evil: 1"))], vec![]).is_err(), "header injection through a value");
        assert!(set(vec![entry("Content-Type", Some("text/plain"))], vec![]).is_err(), "a header Patchbay sets");
        assert!(set(vec![entry("host", Some("evil"))], vec![]).is_err());
        for name in ["Authorization", "AUTHORIZATION", "Host", "content-TYPE"] {
            assert!(set(vec![entry(name, Some("v"))], vec![]).is_err(), "{name} is set by Patchbay, in any case");
        }
        assert!(set(vec![entry("x-a", Some("1")), entry("X-A", Some("2"))], vec![]).is_err(), "duplicate header, any case");
        assert!(set(vec![], vec![entry("", Some("v"))]).is_err(), "empty query name");
        assert!(set(vec![], vec![entry("k", Some("v")), entry("k", Some("w"))]).is_err(), "duplicate query name");
        assert!(set(vec![entry("x-a", Some(&"v".repeat(5000)))], vec![]).is_err(), "too long");
        assert_eq!(keys.get(&extras_account("azure")), None, "a refused save stores nothing");
        assert!(set(vec![entry("api-key", Some("v"))], vec![]).is_ok(), "Azure's api-key is not reserved");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn extras_reach_the_request_config() {
        use super::*;
        let r = Resolved {
            provider: Provider {
                id: "azure".into(),
                name: "Azure".into(),
                kind: ProviderKind::OpenaiChat,
                base_url: "https://x.openai.azure.com/openai/deployments/d".into(),
                has_key: true,
                auth_mode: AuthMode::ApiKey,
                claude_config_dir: None,
                grok_home: None,
            },
            model: Model {
                id: "m".into(),
                provider_id: "azure".into(),
                model_id: "gpt-x".into(),
                display_name: "GPT".into(),
                vision: false,
                context_window: None,
                params: Default::default(),
            },
            key: Some("the-key".into()),
            extras: Extras { headers: vec![("api-key".into(), HEADER_SECRET.into())], query: vec![("api-version".into(), "2024-10-21".into())] },
            auth: Auth::Key,
            grok_user: None,
        };
        let cfg = llm_config(&r);
        assert_eq!(cfg.extra_headers, vec![("api-key".to_owned(), HEADER_SECRET.to_owned())]);
        assert_eq!(cfg.extra_query, vec![("api-version".to_owned(), "2024-10-21".to_owned())]);
        let shown = format!("{cfg:?}");
        assert!(!shown.contains(HEADER_SECRET) && !shown.contains("2024-10-21") && !shown.contains("the-key"), "{shown}");
        let none = Resolved { extras: Extras::default(), ..r };
        assert!(llm_config(&none).extra_headers.is_empty() && llm_config(&none).extra_query.is_empty());
    }

    #[test]
    fn deleting_a_provider_removes_its_extras_from_the_keyring() {
        use super::*;
        let (db, dir) = provider_in_temp_db();
        let keys = MemKeyStore::default();
        set_extras(&db, &keys, "azure", &[entry("api-key", Some(HEADER_SECRET))], &[]).unwrap();
        assert!(keys.get(&extras_account("azure")).is_some());
        delete_provider_with(&db, &keys, "azure").unwrap();
        assert_eq!(keys.get(&extras_account("azure")), None, "the keyring entry is gone with the provider");
        assert!(db.get_provider("azure").unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn model_discovery_carries_the_extras() {
        use super::*;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = sock.read(&mut buf).await.unwrap();
            let body = r#"{"data":[{"id":"gpt-x"}]}"#;
            let _ = sock
                .write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes())
                .await;
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        let extras = Extras {
            headers: vec![("api-key".into(), HEADER_SECRET.into())],
            query: vec![("api-version".into(), "2024-10-21".into())],
        };
        let found = discover_models_for(&ProviderKind::OpenaiChat, &format!("http://127.0.0.1:{port}/openai"), None, Auth::Key, None, &extras)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        let seen = server.await.unwrap();
        assert!(seen.starts_with("GET /openai/models?api-version=2024-10-21 "), "{seen}");
        assert!(seen.to_ascii_lowercase().contains(&format!("api-key: {}", HEADER_SECRET.to_ascii_lowercase())), "{seen}");
    }

    #[tokio::test]
    async fn a_failed_discovery_does_not_print_the_extra_query_values() {
        use super::*;
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let extras = Extras { headers: vec![], query: vec![("code".into(), QUERY_SECRET.into())] };
        let err = discover_models_for(&ProviderKind::OpenaiChat, &format!("http://127.0.0.1:{port}/v1"), None, Auth::Key, None, &extras)
            .await
            .unwrap_err();
        let shown = format!("{err:#} {err:?}");
        assert!(!shown.contains(QUERY_SECRET) && !shown.contains("code="), "{shown}");
        assert!(shown.contains("127.0.0.1"), "still names the host: {shown}");
    }


    #[test]
    fn reserved_headers_follow_the_providers_kind() {
        use super::*;
        let (db, dir) = provider_in_temp_db();
        for (id, kind) in [("anth", ProviderKind::Anthropic), ("gem", ProviderKind::Gemini)] {
            let p = Provider {
                id: id.into(),
                name: id.into(),
                kind,
                base_url: "https://h/v1".into(),
                has_key: false,
                auth_mode: AuthMode::ApiKey,
                claude_config_dir: None,
                grok_home: None,
            };
            db.upsert_provider(&p, None).unwrap();
        }
        let keys = MemKeyStore::default();
        let refused = |id: &str, name: &str| set_extras(&db, &keys, id, &[entry(name, Some("v"))], &[]).is_err();
        // Anthropic sends x-api-key and anthropic-* itself.
        for name in ["x-api-key", "X-API-KEY", "anthropic-beta", "Anthropic-Version", "authorization"] {
            assert!(refused("anth", name), "{name} on an Anthropic provider");
        }
        // Gemini sends x-goog-api-key itself.
        assert!(refused("gem", "X-Goog-Api-Key"));
        assert!(refused("gem", "Authorization"));
        // An OpenAI-compatible provider (Azure) gets only a bearer from its auth mode, so Azure's
        // `api-key` and a gateway's `x-api-key` are usable, and are the only way they get sent.
        assert!(!refused("azure", "api-key"), "Azure's api-key must stay possible");
        assert!(!refused("azure", "x-api-key"));
        assert!(refused("azure", "Authorization"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn extras_that_cannot_be_read_from_the_keyring_refuse_the_request() {
        use super::*;
        let (db, dir) = provider_in_temp_db();
        let keys = MemKeyStore::default();
        // No names stored: nothing to read, nothing required.
        assert_eq!(required_extras(&db, &keys, "azure").unwrap(), Extras::default());
        set_extras(&db, &keys, "azure", &[entry("api-key", Some(HEADER_SECRET))], &[]).unwrap();
        assert_eq!(required_extras(&db, &keys, "azure").unwrap().headers.len(), 1);
        // Names stored but the keyring holds nothing (unreadable, or another machine's keyring).
        let empty = MemKeyStore::default();
        let e = required_extras(&db, &empty, "azure").unwrap_err().to_string();
        assert_eq!(e, "provider headers could not be read from keyring");
        // Names stored and the keyring entry is garbage.
        let garbled = MemKeyStore::default();
        garbled.set(&extras_account("azure"), &format!("{{not json {HEADER_SECRET}"));
        let e = required_extras(&db, &garbled, "azure").unwrap_err().to_string();
        assert_eq!(e, "provider headers could not be read from keyring", "the stored text is never echoed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[derive(Clone, Default)]
    struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for LogBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
        type Writer = LogBuf;
        fn make_writer(&'a self) -> LogBuf {
            self.clone()
        }
    }

    fn azure_resolved(base_url: &str, extras: Extras) -> Resolved {
        use super::*;
        Resolved {
            provider: Provider {
                id: "azure".into(),
                name: "Azure".into(),
                kind: ProviderKind::OpenaiChat,
                base_url: base_url.into(),
                has_key: true,
                auth_mode: AuthMode::ApiKey,
                claude_config_dir: None,
                grok_home: None,
            },
            model: Model {
                id: "m".into(),
                provider_id: "azure".into(),
                model_id: "gpt-x".into(),
                display_name: "GPT".into(),
                vision: false,
                context_window: None,
                params: Default::default(),
            },
            key: Some("the-key".into()),
            extras,
            auth: Auth::Key,
            grok_user: None,
        }
    }

    /// The error text of a failed call goes to `messages.error` (a synced column: chat.rs stores
    /// `e.to_string()`), to the session request log (`ReqRec.error` is `e.message`) and to tracing.
    /// None of them may carry an extra query value, on a refused connection or on an HTTP error.
    #[tokio::test]
    async fn query_values_stay_out_of_error_text_and_tracing_on_a_refused_connection_and_a_401() {
        use super::*;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let logs = LogBuf::default();
        let subscriber = tracing_subscriber::fmt().with_writer(logs.clone()).with_ansi(false).with_max_level(tracing::Level::TRACE).finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let refused_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let _ = sock.read(&mut buf).await;
            let body = r#"{"error":{"message":"Access denied due to invalid subscription key."}}"#;
            let _ = sock
                .write_all(format!("HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes())
                .await;
        });

        for (label, port, status) in [("refused", refused_port, None), ("401", live_port, Some(401u16))] {
            let extras = Extras {
                headers: vec![("api-key".into(), HEADER_SECRET.into())],
                query: vec![("code".into(), QUERY_SECRET.into())],
            };
            let mut cfg = llm_config(&azure_resolved(&format!("http://127.0.0.1:{port}/openai/deployments/d"), extras));
            cfg.max_retries = if status.is_none() { 1 } else { 0 };
            let debug_of_config = format!("{cfg:?}");
            let req = patchbay_llm::Request { messages: vec![patchbay_llm::Message::user("hi")], ..Default::default() };
            let reasons = std::sync::Mutex::new(Vec::<String>::new());
            let err = patchbay_llm::Client::new(cfg)
                .complete(&req, &tokio_util::sync::CancellationToken::new(), |e| {
                    if let patchbay_llm::Event::Retrying { reason, .. } = e {
                        reasons.lock().unwrap().push(reason);
                    }
                })
                .await
                .unwrap_err();
            assert_eq!(err.status, status, "{label}: {err}");
            let stored_in_messages_error = err.to_string();
            tracing::warn!("title generation failed: {err}");
            let mut texts = vec![stored_in_messages_error, err.message.clone(), format!("{err:?}"), debug_of_config];
            texts.extend(reasons.into_inner().unwrap());
            for t in &texts {
                for secret in [QUERY_SECRET, HEADER_SECRET, "code="] {
                    assert!(!t.contains(secret), "{label}: {secret} leaked into: {t}");
                }
            }
            if status.is_none() {
                assert!(texts[0].contains("127.0.0.1"), "still names the host: {}", texts[0]);
            }
        }
        let logged = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
        assert!(logged.contains("title generation failed"), "the log capture works: {logged}");
        for secret in [QUERY_SECRET, HEADER_SECRET, "code="] {
            assert!(!logged.contains(secret), "{secret} reached tracing: {logged}");
        }
    }


    #[test]
    fn a_second_different_scope_is_an_error_not_a_silent_keep_of_the_first() {
        let cell = std::sync::OnceLock::new();
        super::set_scope_in(&cell, Some("aa11".into())).unwrap();
        super::set_scope_in(&cell, Some("aa11".into())).expect("the same scope again is fine");
        let e = super::set_scope_in(&cell, Some("bb22".into())).unwrap_err().to_string();
        assert!(e.contains("aa11") && e.contains("bb22"), "{e}");
        assert!(super::set_scope_in(&cell, None).is_err(), "the default folder after a scoped one is a mismatch too");
        assert_eq!(cell.get(), Some(&Some("aa11".to_owned())), "the first scope stays");
    }

    #[test]
    fn a_scoped_instance_gets_its_own_provider_names_but_the_default_keeps_the_old_ones() {
        // The default folder (no scope) keeps the names existing installs use.
        assert_eq!(scoped_with(None, "prov-1"), "prov-1");
        // Any other folder (a scratch build, `patchbayd --data-dir`) never reads or overwrites them.
        let a = scoped_with(Some("aa11"), "prov-1");
        assert_eq!(a, "prov-1#aa11");
        assert_ne!(a, scoped_with(Some("bb22"), "prov-1"));
        // MCP and web-search secrets are scoped the same way.
        assert_eq!(scoped_with(Some("aa11"), super::WEB_SEARCH_ACCOUNT), "web-search.brave#aa11");
        // `mesh:` accounts arrive already scoped by OsKeyStore; scoping twice would orphan them.
        assert_eq!(scoped_with(Some("aa11"), "mesh:passphrase#aa11"), "mesh:passphrase#aa11");
    }

    /// Against the real macOS keychain (hence ignored): a process that opened a non-default data
    /// folder writes ONLY the scoped account, reads it back, and leaves nothing when it deletes it.
    /// Run by hand or from `scripts/scratch_rig.py`: `cargo test --lib -- --ignored scoped_keychain`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes the real OS keychain under a unique probe name"]
    fn scoped_keychain_entry_is_the_only_one_written() {
        let dir = std::env::temp_dir().join(format!("pb-scope-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let scope = crate::mesh::keys::data_dir_scope(&dir).expect("a temp folder is not the default folder");
        super::set_keyring_scope(Some(scope.clone())).unwrap();
        let id = format!("pb67-probe-{}", crate::db::new_id());
        let exists = |account: &str| {
            std::process::Command::new("security")
                .args(["find-generic-password", "-s", "dev.patchbay.app", "-a", account])
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(super::keyring_set(&id, "probe-value"), "the keychain accepted the write");
        let (scoped_there, unscoped_there) = (exists(&format!("{id}#{scope}")), exists(&id));
        let read_back = super::keyring_get(&id);
        super::keyring_delete(&id);
        let gone = !exists(&format!("{id}#{scope}"));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(scoped_there, "the scoped account was written");
        assert!(!unscoped_there, "the unscoped account (the live app's name) was NOT written");
        assert_eq!(read_back.as_deref(), Some("probe-value"));
        assert!(gone, "deleting removed the scoped entry");
    }

    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A fake keychain keyed by raw account name, counting reads.
    struct Fake {
        map: RefCell<HashMap<String, String>>,
        reads: RefCell<Vec<String>>,
    }
    impl Fake {
        fn with(entries: &[(&str, &str)]) -> Self {
            Self {
                map: RefCell::new(entries.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()),
                reads: RefCell::default(),
            }
        }
        fn get(&self, a: &str) -> Option<String> {
            self.reads.borrow_mut().push(a.to_owned());
            self.map.borrow().get(a).cloned()
        }
        fn try_get(&self, a: &str) -> Result<Option<String>, String> {
            Ok(self.get(a))
        }
        fn set(&self, a: &str, v: &str) -> bool {
            self.map.borrow_mut().insert(a.to_owned(), v.to_owned());
            true
        }
        fn copy(&self, db: &crate::db::Db, scope: Option<&str>) -> usize {
            super::copy_unscoped_with(db, scope, &|a| self.try_get(a), &|a, v| self.set(a, v))
        }
    }

    fn db_with_provider(id: &str) -> (crate::db::Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("pb-copy-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::Db::open(&dir.join("db.sqlite")).unwrap();
        let p = crate::db::Provider {
            id: id.to_owned(),
            name: "p".into(),
            kind: crate::db::ProviderKind::Anthropic,
            base_url: "https://example.invalid".into(),
            has_key: false,
            auth_mode: Default::default(),
            claude_config_dir: None,
            grok_home: None,
        };
        db.upsert_provider(&p, None).unwrap();
        (db, dir)
    }

    #[test]
    fn a_referenced_unscoped_key_is_copied_to_the_scoped_name_and_the_original_stays() {
        let (db, dir) = db_with_provider("prov-1");
        let kc = Fake::with(&[("prov-1", "sk-old"), ("extras:prov-1", "{\"api-key\":\"x\"}")]);
        assert_eq!(kc.copy(&db, Some("s1")), 2);
        assert_eq!(kc.get("prov-1#s1").as_deref(), Some("sk-old"), "found under the scoped name");
        assert_eq!(kc.get("extras:prov-1#s1").as_deref(), Some("{\"api-key\":\"x\"}"));
        assert_eq!(kc.get("prov-1").as_deref(), Some("sk-old"), "the original is never deleted");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unreferenced_unscoped_entry_is_never_read_and_a_scoped_one_is_never_overwritten() {
        let (db, dir) = db_with_provider("prov-1");
        let kc = Fake::with(&[("other-app-id", "secret"), ("prov-1", "sk-old"), ("prov-1#s1", "sk-new")]);
        kc.copy(&db, Some("s1"));
        let reads = kc.reads.borrow().clone();
        assert!(!reads.iter().any(|r| r.starts_with("other-app-id")), "read an account this database does not refer to: {reads:?}");
        assert_eq!(kc.map.borrow()["prov-1#s1"], "sk-new", "an existing scoped entry wins");
        assert!(!reads.contains(&"prov-1".to_owned()), "no unscoped read when the scoped entry exists");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_copy_runs_once_per_instance() {
        let (db, dir) = db_with_provider("prov-1");
        let kc = Fake::with(&[("prov-1", "sk-old")]);
        assert_eq!(kc.copy(&db, Some("s1")), 1);
        kc.map.borrow_mut().remove("prov-1#s1");
        kc.reads.borrow_mut().clear();
        assert_eq!(kc.copy(&db, Some("s1")), 0, "a second start does not copy again");
        assert!(kc.reads.borrow().is_empty(), "and does not touch the keychain");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_default_folder_is_untouched() {
        let (db, dir) = db_with_provider("prov-1");
        let kc = Fake::with(&[("prov-1", "sk-old")]);
        assert_eq!(kc.copy(&db, None), 0);
        assert!(kc.reads.borrow().is_empty());
        assert_eq!(kc.map.borrow().len(), 1, "nothing written");
        assert!(db.get_setting(super::COPIED_SETTING).unwrap().is_none(), "and nothing recorded");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_copy_is_retried_at_the_next_start() {
        let (db, dir) = db_with_provider("prov-1");
        let kc = Fake::with(&[("prov-1", "sk-old")]);
        assert_eq!(super::copy_unscoped_with(&db, Some("s1"), &|a| kc.try_get(a), &|_, _| false), 0);
        assert_eq!(kc.copy(&db, Some("s1")), 1, "not recorded after a failure");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unreadable_keychain_entry_is_never_overwritten_and_is_retried() {
        let (db, dir) = db_with_provider("prov-1");
        let kc = Fake::with(&[("prov-1", "sk-old"), ("prov-1#s1", "sk-new")]);
        // The scoped entry cannot be read right now (a denied prompt): it must not look missing.
        let denied = |a: &str| if a == "prov-1#s1" { Err("denied".to_owned()) } else { kc.try_get(a) };
        assert_eq!(super::copy_unscoped_with(&db, Some("s1"), &denied, &|a, v| kc.set(a, v)), 0);
        assert_eq!(kc.map.borrow()["prov-1#s1"], "sk-new", "not overwritten");
        // The unscoped one cannot be read: not recorded as done either.
        let (db2, dir2) = db_with_provider("prov-2");
        let kc2 = Fake::with(&[("prov-2", "sk-old")]);
        let denied2 = |a: &str| if a == "prov-2" { Err("denied".to_owned()) } else { kc2.try_get(a) };
        assert_eq!(super::copy_unscoped_with(&db2, Some("s1"), &denied2, &|a, v| kc2.set(a, v)), 0);
        assert_eq!(kc2.copy(&db2, Some("s1")), 1, "retried at the next start, and copied once readable");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    #[test]
    fn mcp_secrets_and_the_web_search_key_are_referenced_only_when_the_database_says_so() {
        let (db, dir) = db_with_provider("prov-1");
        let kc = Fake::with(&[("web-search.brave", "bk"), ("mcp:srv-1:env:TOKEN", "t"), ("mcp:srv-1:env:PLAIN", "p")]);
        assert_eq!(kc.copy(&db, Some("s1")), 0, "no web-search setting, no MCP server: neither is looked at");
        assert!(!kc.reads.borrow().iter().any(|r| r.starts_with("web-search.brave") || r.starts_with("mcp:")));
        // (That run was recorded as done, as designed; a fresh database now says it has both.)
        let (db, dir2) = db_with_provider("prov-1");
        db.set_setting(super::WEB_SEARCH_INLINE, "").unwrap();
        let cfg = crate::code::mcp::McpConfig {
            transport: "stdio".into(),
            command: "x".into(),
            env: vec![
                crate::code::mcp::KeyValue { name: "TOKEN".into(), value: String::new(), secret: true, stored: true },
                crate::code::mcp::KeyValue { name: "PLAIN".into(), value: "p".into(), secret: false, stored: false },
            ],
            ..Default::default()
        };
        db.upsert_mcp_server(&crate::db::McpServer {
            id: "srv-1".into(),
            name: "s".into(),
            enabled: true,
            config: serde_json::to_value(&cfg).unwrap(),
        })
        .unwrap();
        assert_eq!(kc.copy(&db, Some("s1")), 2);
        assert_eq!(kc.map.borrow()["web-search.brave#s1"], "bk");
        assert_eq!(kc.map.borrow()["mcp:srv-1:env:TOKEN#s1"], "t");
        assert!(!kc.map.borrow().contains_key("mcp:srv-1:env:PLAIN#s1"), "a non-secret value is not a keychain account");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }
}
