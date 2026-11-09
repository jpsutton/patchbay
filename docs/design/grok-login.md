# Grok subscription logins

A Grok seat (SuperGrok, X Subscription or Grok Team) works in Patchbay the way a Claude subscription seat does: a provider can run on the sign-in `grok login` keeps, the seat's weekly usage shows in the plan gauge, and "Stop at plan limit" protects it. This note records how it works and what was measured. Measurements are from `grok` 1.0.60 on a Grok Team login (2026-11-09); the endpoints are not a public API, so treat them as observed behavior.

## Goal

- A provider on the OpenAI Responses format can use the login instead of an API key, for Chat and Patchbay's agent.
- Grok Build, running on Patchbay's models, can use a Grok-login provider too.
- The seat's weekly pool shows in the plan gauge (history, per account), and the usage guard, auto-resume and mesh sharing apply to it.
- Several Grok accounts can sit side by side, one Grok home each.

## The login

`grok login` writes `<grok home>/auth.json`: a map keyed `https://auth.x.ai::<client id>`, whose OIDC entry holds `key` (the bearer, about six hours), `refresh_token`, `expires_at`, `oidc_issuer`, `oidc_client_id`, `user_id`, `team_id`, `team_name`, `principal_type` and `principal_id`. The Grok home is `$GROK_HOME` when set and not empty, else `~/.grok`; a provider's "Grok home" overrides both. Patchbay ignores `GROK_AUTH_PATH`.

`grok_auth.rs` reads it, caches the token per Grok home, and refreshes it within five minutes of expiry:

1. Take an exclusive lock on `auth.json.lock` next to the file (the lock `grok` takes; the file is never deleted).
2. Read `auth.json` again under the lock. A `grok` that refreshed meanwhile wins, and nothing is spent.
3. `POST` `grant_type=refresh_token` to the token endpoint from `<issuer>/.well-known/openid-configuration`, with the entry's `client_id` and, for team logins, `principal_type` and `principal_id`. Once, with no retry.
4. Write the file back atomically (temp file in the same directory, mode 0600, synced, renamed; through a symlink) with only `key`, `refresh_token`, `expires_at` and `create_time` changed. Every other key and login stays.

Refresh tokens rotate and the provider detects reuse: spending a rotated token again revokes the successor. That is why the lock, the re-read and the single attempt exist; a wrong refresh signs the user out of `grok`. `invalid_grant` or `invalid_client` becomes "Grok login expired or was revoked — run `grok login`".

The account key is `grok:<team id, or personal>:<user id>`: each seat has its own pool, and both ids come from the auth server, so every mesh member computes the same key.

## Requests

Inference is `POST https://cli-chat-proxy.grok.com/v1/responses`, the OpenAI Responses API (`store: false`, `reasoning.encrypted_content`, `prompt_cache_key`), which Patchbay's `responses` adapter already speaks. `GET /v1/models` on the same base lists the models (`grok-4.7`, `grok-4.7-build-fast`, `grok-4.6`, `grok-4.5` when measured), each with `api_backend`, `context_window` and its reasoning efforts. A call's answer carries `usage.cost_in_usd_ticks` (ten-billionths of a dollar), which Patchbay shows as the cost.

Requests present as Grok Build, as Claude logins present as Claude Code. They send what `grok` sends: `Authorization: Bearer`, `X-XAI-Token-Auth: xai-grok-cli`, `x-authenticateresponse`, `x-grok-client-version` (from the installed `grok --version`, else 1.0.60; the proxy gates on a minimum version), `x-grok-client-identifier: grok-shell`, `x-grok-client-mode`, a `grok-shell/<version> (<os>; <arch>)` user agent, the user id, `x-grok-model-override`, and per request the session id (`x-grok-conv-id`, `x-grok-session-id`) and a fresh `x-grok-req-id`. Which of these the proxy enforces was not tested.

A `403` whose body says the caller has "run out of available resources or do not have an active Grok subscription" means the plan's usage is used up or the account has no Grok subscription. Some third-party clients reported it on some tiers; it was not seen on a Team seat.

xAI supports subscription OAuth in some third-party tools (OpenCode is announced) but publishes no terms for other clients, and its usage docs describe the pool as "eligible first-party usage". Patchbay's use is the same posture as its Claude login.

## Plan usage

`GET /v1/billing?format=credits` (what Grok Build's `x.ai/billing` and `/usage` call) answers with `config.creditUsagePercent`, `config.currentPeriod` (`USAGE_PERIOD_TYPE_WEEKLY`, with `start` and `end`), `config.prepaidBalance`, `config.onDemandUsed` and a top-level `subscription_tier`. `code/grok_plan.rs` maps a weekly period to the weekly window of a `PlanLimits` (so the Claude plan guard, history and mesh events apply unchanged) and past 100 percent with credit to draw on to extra usage. A monthly period reports nothing rather than a gauge with the wrong span.

A report is requested once at start, after each model call, status update and finished turn of a session on a Grok login (at most every 15 seconds per login), and every minute while one runs. The response's `x-ratelimit-*` headers are per-window throughput limits, not the pool.

## Grok Build

Grok attaches no session credential to a custom model's provider: a proxy-backed `[model_providers]` entry with no key answers 401 "no auth context". With `env_key` set to the access token it works, with no `auth.json` at all. So `grok_config.rs` gives a Grok-login provider the login's token as `PATCHBAY_KEY_<n>`, like a stored key, and `code_start_local` refreshes it just before the launch. The token lasts what is left of its lifetime; a session that outlives it gets 401s until the agent restarts.

## Not covered

- Grok Bot (Cursor's cloud agent) and its separate pool.
- Grok's server-side tools (`web_search`, `x_search`, image and video); Patchbay keeps its own web search.
- Importing Grok Build sessions.
- Refreshing the token of a running Grok Build session.
