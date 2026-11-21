# Antigravity subscription login

An Antigravity subscription login lets a Gemini provider use the Google sign-in already held by `agy` or Antigravity IDE. Patchbay uses the Cloud Code service to discover models and stream Chat and native-agent requests, and reports the account's plan quota in the session gauge. This document records the locked v1 design for `pb-20261117-10`; it describes intended behavior, not a completed implementation.

## Goal

- A Gemini provider can use Antigravity login instead of a Gemini API key for Chat and Patchbay's native agent.
- Multiple Google accounts can be configured side by side, one `gemini_home` per account.
- Antigravity model quota appears in the plan gauge and uses the existing plan polling path.
- ACP `AgentKind::Antigravity` behavior remains as it is.

## The login

`gemini_home` is a non-secret, provider-local directory setting. If it is unset, resolve `$GEMINI_HOME` when non-empty, otherwise `~/.gemini`. A configured provider's home takes precedence over the environment default. Each home identifies a separate login and token cache.

Credential resolution preserves the `pb-20261107-01` (#354) invariants, parameterized by the resolved home:

1. `<gemini_home>/antigravity-cli/antigravity-oauth-token` (the `agy` CLI token file).
2. `<gemini_home>/oauth_creds.json` (the Antigravity IDE OAuth credentials).
3. As a last resort, macOS Keychain service `gemini`, account `antigravity`, and only for the default `gemini_home` (not an explicitly selected alternate account directory).

Every file and Keychain read runs in `spawn_blocking`. Keychain access is single-flight and uses exponential `keychain_backoff_secs` after failure, so a denied or unanswered authorization dialog cannot block the async runtime or trigger a prompt on every poll. Tests must use fake readers and must never access the real Keychain. The OAuth `client_id` (`*.apps.googleusercontent.com`) and `client_secret` (`GOCSPX-*`) are discovered dynamically by scanning the locally installed `agy` or Antigravity `language_server` binary; neither value is hardcoded into Patchbay.

Token refresh posts `grant_type=refresh_token`, the refresh token, and the discovered client credentials to Google's documented token endpoint, `https://oauth2.googleapis.com/token`. Refreshed access tokens are cached in memory per login; the existing quota implementation does not rewrite the credential store. Google describes installed desktop applications as public clients and documents the browser-based installed-app authorization-code flow and refresh-token exchange (see [OAuth 2.0 for Desktop Apps](https://developers.google.com/identity/protocols/oauth2/native-app)).

The current `src-tauri/src/code/antigravity_plan.rs` implements refresh-token exchange and reads already-created credentials; it does not implement an interactive authorization-code sign-in in this module. The v1 design extracts the shared resolution and refresh support into `antigravity_auth`, parameterized by `gemini_home`, so the provider and quota poller share it. The source is currently environment/default-home based; per-provider home behavior is part of the locked v1 design.

## Requests

OAuth inference, model discovery, project lookup, and plan quota polling use `https://daily-cloudcode-pa.googleapis.com` and Cloud Code `v1internal` methods (including `retrieveUserQuotaSummary`), not the public Gemini Developer API at `generativelanguage.googleapis.com/v1beta`.

Requests use these headers:

- `Authorization: Bearer <refreshed access token>`.
- `User-Agent: antigravity/cli/<ver>`, where `<ver>` is read from `<gemini_home>/bin/agy.version.json` or `<gemini_home>/antigravity-cli/agy.version.json`, falling back to `1.4.12`.
- `x-goog-api-client: google-cloud-sdk vscode_cloudshelleditor/0.1`.
- `client-metadata: {"ideType":"ANTIGRAVITY","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}`.

`POST {base}/v1internal:fetchAvailableModels` with `{}` (or `{"project": ...}`) discovers models. The `models` map is keyed by model ID and may include Gemini and third-party models. Disabled entries are skipped, and `maxTokens` maps to Patchbay's `context_window`.

Streaming uses `POST {base}/v1internal:streamGenerateContent?alt=sse`. The standard Gemini request body is wrapped in the Cloud Code envelope:

```json
{
  "project": "",
  "model": "<model_id_without_models_prefix>",
  "userPromptId": "<fresh_request_id>",
  "request": {
    "contents": [],
    "systemInstruction": {},
    "generationConfig": {},
    "tools": [],
    "toolConfig": {}
  }
}
```

If a Workspace account rejects the empty project, resolve `cloudaicompanionProject` with `POST {base}/v1internal:loadCodeAssist` and the same client metadata; cache the result per login. In `v1internal` mode, function declarations use `parameters` after removing JSON Schema keywords Cloud Code rejects (including `$schema` and `additionalProperties`). Tool-call `thoughtSignature` values round-trip. The SSE event parser unwraps `response` when present before parsing candidates, usage metadata, prompt feedback, and model version, so wrapped Cloud Code and unwrapped Gemini streams use the same downstream parser.

These wire requirements are the locked specification in section C of the `pb-20261117-10` remediation plan. Google's [Gemini Developer API reference](https://ai.google.dev/api/generate-content) documents the separate `generativelanguage.googleapis.com/v1beta` surface. Google's [Cloud Code API and references index](https://docs.cloud.google.com/gemini/docs/api-and-reference) documents `cloudaicompanion` REST APIs; the exact `daily-cloudcode-pa.googleapis.com/v1internal` methods described above were not found there.

## Plan usage

`useSessionPlanKind` returns `antigravity` for a session whose provider has `authMode === "antigravity_login"` on a Patchbay or Grok session. Native sessions using that provider receive the same plan-limit and background polling treatment as `AgentKind::Antigravity`, with reports returned as `{ kind: "antigravity", limits }`. This reuses the existing Antigravity plan quota path and its account-aware reporting.

## Terms and undocumented endpoint posture

This design uses two distinct Google-facing surfaces. Google's public OAuth documentation describes the installed-app authorization and refresh mechanisms and the token endpoint. Google's public Gemini API documentation describes a different public API surface. This design does **not** call the public Gemini Developer API for subscription inference.

The Cloud Code hostname `daily-cloudcode-pa.googleapis.com` and the `v1internal` methods `fetchAvailableModels`, `streamGenerateContent`, `loadCodeAssist`, and quota retrieval are internal/undocumented service interfaces for purposes of this design. They are not represented here as documented public Google or Cloud Code API methods. A search of Google's public developer documentation did not locate public method documentation for these exact `v1internal` calls. That search result is not proof that no private authorization or applicable service agreement exists.

Google's [API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy) says: “Do not use undocumented APIs without express permission” and limits access to the means stipulated in the relevant API's official documentation. It also requires accurate representation of the environment in which an authentication page appears and prohibits false or misleading claims about who authorized or manages an application. The [Google APIs Terms of Service](https://developers.google.com/terms/) prohibit interfering with APIs or their servers and restrict reverse engineering of APIs or related software. Google's [OAuth 2.0 for Desktop Apps guidance](https://developers.google.com/identity/protocols/oauth2/native-app) describes installed applications as public clients and recommends PKCE; the existing refresh-token reuse does not itself establish that Patchbay is an authorized OAuth client for Cloud Code. The exact applicability of these terms to a user's Antigravity subscription, this third-party client, and each Cloud Code endpoint has not been determined from public materials. **UNVERIFIED:** that Google has granted Patchbay or its maintainers express permission to call these `v1internal` methods, that the subscription terms authorize this third-party use, and that the client metadata/client identity used here satisfies Google's client representation requirements. No conclusion that this use is permitted follows from the endpoints being callable or used by Google's own clients.

David and Jared's project accepts the residual risk that Google may change or disable these private methods, reject Patchbay's client identity, enforce or interpret terms in a way that disallows this use, or revoke the login/client access. In those cases model discovery or requests may fail with authorization or service errors, and a revoked or expired refresh token requires the user to sign in again through a supported Google/Antigravity flow. The user's subscription itself and first-party Antigravity access are outside Patchbay's control. **UNVERIFIED:** whether a policy or terms violation would result in a warning, API denial, account restriction, or another outcome; public material reviewed here does not establish an enforcement result for this specific integration.

## Not covered

- ChatGPT/Codex subscription login: see [`codex-login.md`](codex-login.md) (`pb-20261111-33`). GitHub Copilot was dropped from scope (2026-11-20).
- Turn cancellation, send gating, and auto-resume (`pb-20261117-11`).
- Changes to the existing ACP `AgentKind::Antigravity` path.
- Interactive sign-in UI or authorization-code flow implementation in this design phase.

## Design notes and source discrepancies

- The existing quota code refreshes a stored token and reads the default/environment Gemini home. The locked provider design adds per-provider `gemini_home` resolution; this document follows the plan for v1.
- The current quota source applies Keychain fallback after the two disk sources without a default-home-only guard. The locked v1 specification requires Keychain access only for the default home; the design follows that requirement.
- The plan calls the refresh support an RFC 8252 installed-app OAuth flow. The inspected source implements refresh-token exchange for an existing login, not interactive installed-app authorization. This document describes that distinction rather than claiming interactive sign-in already exists.
