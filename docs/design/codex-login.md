# ChatGPT / Codex subscription login

A ChatGPT/Codex subscription login lets an OpenAI provider run on the sign-in that the `codex` CLI already holds, so a ChatGPT plan can power Chat and Patchbay's native agent without an API key. This document records the locked v1 design for `pb-20261111-33`; it describes intended behavior, not a completed implementation. It follows the shape of [`grok-login.md`](grok-login.md) and [`antigravity-login.md`](antigravity-login.md).

Status words used below: **UNVERIFIED** marks a claim that no official OpenAI page cited here supports, and an endpoint or payload shape that no fixture in this repository backs. Nothing in this document is backed by an official OpenAI page, so every terms claim is UNVERIFIED.

## Goal

- An OpenAI provider can use the Codex login instead of an API key.
- Patchbay only reads the login. It never writes, refreshes or deletes it.
- A missing or expired login shows "run `codex login`" and nothing else happens.
- The provider shows a subscription badge. There is no usage meter.

## Decisions

These are David's rulings of 2026-11-20.

- **Provider kind (D2):** reuse the existing OpenAI provider kind and distinguish the login by `auth_mode`, the way `GrokLogin` does. In the code today the Grok precedent is `ProviderKind::OpenaiResponses` with `AuthMode::GrokLogin` (`src-tauri/src/lib.rs:248`). There is no variant literally named `ProviderKind::OpenAi`; the build maps D2 onto `OpenaiResponses` unless a different variant is decided first. A new `AuthMode::CodexLogin` is added next to `GrokLogin`.
- **v1 reuses the CLI login only (D3):** no in-app OAuth flow, no browser callback server, no OAuth client registration owned by Patchbay.
- **Read-only (token ruling):** Patchbay never writes `<codex_home>/auth.json` and never spends the refresh token. There is no lock file, no atomic write-back and no refresh request. This replaces the write-back design the plan first sketched.
- **Badge, not meter (D5):** the provider shows a subscription badge only. No usage endpoint is called.

## The login

`codex_home` is a non-secret, provider-local directory setting. If it is unset, resolve `$CODEX_HOME` when set and non-empty, otherwise `~/.codex` (`%USERPROFILE%\.codex` on Windows). A configured provider's home takes precedence over the environment default. Each home is one login, so several ChatGPT accounts can sit side by side, one `codex_home` each.

The credential file is `<codex_home>/auth.json`. Observed on one machine with `codex-cli 0.162.1` (macOS, file mode `0600`, key names only; no value was read into this document): the top-level keys are `OPENAI_API_KEY`, `auth_mode`, `last_refresh` and `tokens`, and `tokens` holds `access_token`, `account_id`, `id_token` and `refresh_token`. This is one observation of one CLI version, not a specification. **UNVERIFIED:** that other `codex` versions or platforms keep the same layout, and the meaning of each key (including whether `auth_mode` is the value `chatgpt` for every subscription login).

Resolution is a plain read:

1. Read `<codex_home>/auth.json` in `spawn_blocking`.
2. Use `tokens.access_token` as the bearer and `tokens.account_id` for routing, when present.
3. If the file is absent, unreadable, not the expected shape, or holds no `access_token`, the provider is **not logged in**.
4. If the token is expired, the provider is **expired**. Expiry is judged from the access token's own `exp` claim when it parses, and otherwise from the first 401 on use. **UNVERIFIED:** that the access token is a JWT with an `exp` claim on every login.

Both states show "run `codex login`". Patchbay re-reads the file on every request (cheap, and it picks up a `codex login` run in another terminal without a restart); it caches nothing to disk.

The key `OPENAI_API_KEY` in the same file is for API-key logins of the CLI. A provider in `codex_login` mode ignores it; a user with an API key uses the ordinary `api_key` mode.

### Identity

The account key is derived from `tokens.account_id` when present. When it is absent, derive a short stable id from a hash of the access token's non-secret claims or, failing that, of the token itself (`sha256(...)[..12]`). Never slice a raw token string, and never log or display one.

### Open question: keychain storage

`codex` may store its login in the OS keychain (or another store) instead of `auth.json`, depending on a credential-store setting. If it does, `<codex_home>/auth.json` is absent or stale and v1 shows **not logged in**, which is the correct and safe v1 behavior (Patchbay does not read the keychain for this login).

**UNVERIFIED:** whether `codex` has such a setting, its name, its values, and its default. The remediation plan names a `cli_auth_credentials_store` option; a search of the installed `codex` 0.162.1 native binary for that string found nothing, and `~/.codex/config.toml` on the observing machine does not set it, so neither the name nor the behavior is confirmed here. Resolve this against OpenAI's published `codex` documentation or source before the build, and decide then whether a keychain-backed login needs its own row.

## Requests

All of this section is **UNVERIFIED**: no fixture in this repository captures these exchanges, and no official OpenAI page is cited. It records what the plan expects, to be confirmed against recorded fixtures before the build.

- The wire format is the OpenAI Responses API, which Patchbay's `responses` adapter already speaks (the same adapter the Grok login uses).
- Inference is expected at `https://chatgpt.com/backend-api/codex/responses` and model discovery at `https://chatgpt.com/backend-api/codex/models`.
- Headers are expected to be `Authorization: Bearer <access_token>`, `ChatGPT-Account-ID: <account_id>` when an account id is present, and a `User-Agent` naming the client. The exact `User-Agent` the service expects, if it checks one, is not known.

The provider preset is `ChatGPT / Codex (subscription)`: OpenAI kind, `auth_mode: codex_login`, and a base URL from the allowlist below. The preset's base URL is **UNVERIFIED** until the endpoints above are fixture-confirmed.

### Host allowlist

A request is refused before the credential is read unless the base URL is `https://` and its host is exactly `chatgpt.com`. Plain HTTP, IP addresses, other ports, userinfo, and look-alike or suffix hosts (for example `chatgpt.com.example.test`) are refused. Enterprise or tenant hosts are out of scope for v1. The check runs on the parsed URL, not on a string prefix.

### Secrets

The access token, `id_token`, `refresh_token` and `account_id` never appear in a log line, an error message, a panic message, or a mesh message. The login is never synced over the mesh (`mesh/secrets.rs`); each machine reads its own `codex_home`. Tests use temp directories and fake files and never touch the real `~/.codex`, the real keychain, or the network.

## Plan usage

The provider shows a **subscription badge** (a plain "ChatGPT subscription" indicator when the login resolves). It does not show a numeric gauge, history, or "Stop at plan limit" behavior. A usage endpoint exists in the plan's notes (`/backend-api/wham/usage`); it is **UNVERIFIED**, not stable or public as far as this design knows, and is deliberately not used in v1. A usage meter is a separate row if wanted later.

## Terms and undocumented endpoint posture

`chatgpt.com/backend-api/codex` is treated here as an internal service route used by OpenAI's own first-party clients, not as part of the public developer API at `api.openai.com`. No official OpenAI page is cited in this document, so each of the following is **UNVERIFIED**:

- that `chatgpt.com/backend-api/codex` is intentionally undocumented rather than documented somewhere this design did not find;
- that OpenAI's terms permit a third-party application to call it with a token read from `~/.codex/auth.json`, and whether the subscription terms cover this use;
- whether OpenAI applies client identification, TLS fingerprinting, bot detection or rate limits that could reject Patchbay's requests;
- whether a terms violation would produce a warning, an API denial, an account restriction, or nothing. No enforcement outcome for this specific integration is established.

The user takes on this risk by choosing the `codex_login` mode, and v1 does not claim the use is permitted because the endpoint is callable. The residual risks are that OpenAI changes or removes the route without notice, rejects Patchbay's client identity, or revokes the login, in which case requests fail with an authorization or service error and the user runs `codex login` again in the official CLI. Because Patchbay never writes or refreshes the credential, it cannot invalidate the CLI's own login through a refresh race. That risk (the rotating refresh token being spent by a second client) was the main reason for the read-only ruling.

## Not covered

- GitHub Copilot is out of scope.
- An in-app sign-in flow, refresh, or any write to `auth.json`.
- A usage meter, history or plan-limit stop for this login.
- Enterprise or custom hosts.
- Keychain-backed logins (see the open question above).
