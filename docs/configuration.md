# Configuration

Cica reads `config.toml` from your platform config directory. Run `cica paths` to see the exact location (e.g. `~/Library/Application Support/cica/config.toml` on macOS, `~/.config/cica/config.toml` on Linux). `cica init` writes this file for you; you can also edit it by hand.

A handful of settings can be supplied (or overridden) by environment variables — this is how a cloud worker runs with **no `config.toml` at all**, taking everything from its task environment.

See [config.example.toml](../config.example.toml) for a copy-pasteable template.

## Minimal single-box config

```toml
backend = "claude"

[channels.telegram]
bot_token = "123456:ABC..."

[claude]
api_key = "sk-ant-..."
```

That's enough to run `cica`. Everything else is optional.

## Top-level keys

| Key | Type | Default | Meaning |
|---|---|---|---|
| `backend` | `"claude"` \| `"cursor"` | `"claude"` | Which AI backend to use. |
| `audit` | bool | `true` | Log conversations and system events to `audit.db`. |
| `onboarding_prompt` | string | — | Global onboarding prompt; a channel can override it. |

## Channels

Configure one or more. A channel section's presence is what enables it.

### `[channels.telegram]`
| Key | Type | Default | Meaning |
|---|---|---|---|
| `bot_token` | string | — | Telegram bot token. |
| `auto_approve` | bool | `false` | Auto-approve new users (skip `cica approve`). |
| `shared_identity` | bool | `false` | Use shared `PERSONA.md` instead of per-user identity/onboarding. |
| `onboarding_prompt` | string | — | Per-channel onboarding prompt override. |

### `[channels.signal]`
| Key | Type | Default | Meaning |
|---|---|---|---|
| `phone_number` | string | — | The Signal number cica registers as. |
| `auto_approve` | bool | `false` | Auto-approve new users. |
| `shared_identity` | bool | `false` | Use shared `PERSONA.md`. |
| `onboarding_prompt` | string | — | Per-channel override. |

### `[channels.slack]`
| Key | Type | Default | Meaning |
|---|---|---|---|
| `bot_token` | string | — | Slack bot token (`xoxb-...`). |
| `app_token` | string | — | Slack app-level token (`xapp-...`). |
| `auto_approve` | bool | `false` | Auto-approve new users. |
| `shared_identity` | bool | `false` | Use shared `PERSONA.md`. |
| `onboarding_prompt` | string | — | Per-channel override. |
| `unfurl_links` | bool | `false` | Let Slack preview links in bot messages. |

Files the agent names with an `[attachment:...]` marker are uploaded with the bot token, which needs the `files:write` scope. Without it the reply still arrives as text, with a note that the file could not be attached.

### `[channels.linear]`

The one **inbound** channel. Telegram long-polls, Slack uses Socket Mode, Signal
talks to a local daemon — Linear POSTs an `AgentSessionEvent` webhook when the
app is `@mentioned` on an issue, so cica opens a listening port.

cica never terminates TLS: put an ALB or a reverse proxy in front and point it
at `listen_addr`. cica verifies the `Linear-Signature` HMAC itself regardless,
and refuses to start if `webhook_secret` is empty.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `client_id` | string | — | The OAuth application's client id. |
| `client_secret` | string | — | Its client secret. |
| `refresh_token` | string | — | Accepted and **ignored** — see below. |
| `access_token` | string | — | A pre-minted token. Local testing only — see the note below. |
| `webhook_secret` | string | — | Webhook signing secret. Required — cica will not listen unverified. |
| `listen_addr` | string | `"0.0.0.0:8080"` | Where the listener binds. Webhook at `POST /webhooks/linear`, health check at `GET /health`. |
| `auto_approve` | bool | `false` | Auto-approve new users. |
| `shared_identity` | bool | `false` | Use shared `PERSONA.md`. |
| `onboarding_prompt` | string | — | Per-channel override. |

Every key also has an env form, for deployments that would rather not write
secrets into `config.toml`: `CICA_LINEAR_CLIENT_ID`,
`CICA_LINEAR_CLIENT_SECRET`, `CICA_LINEAR_REFRESH_TOKEN`,
`CICA_LINEAR_ACCESS_TOKEN`, `CICA_LINEAR_WEBHOOK_SECRET`,
`CICA_LINEAR_LISTEN_ADDR`.

> **Which credential, and why it matters.** Use `client_id` + `client_secret`.
> cica mints a 30-day app-actor token from them and renews an hour before expiry.
>
> A Linear app has a **single grant**. Minting a client-credentials token
> revokes any outstanding authorization-code grant — access token *and* refresh
> token. So a channel holding a refresh token loses it the moment anything else
> authenticates as the same app (a CLI, a skill, a CI job), and the failure is
> quiet: the turn completes, the work lands, and only the closing activity
> fails, leaving the Linear session "running" forever. `refresh_token` is
> therefore accepted and ignored, with a warning.
>
> `access_token` is for a throwaway local test only — it cannot renew, so the
> channel goes quiet after 24 hours with a 401 in the log.
>
> **The `actor=app` installation is still required**, and is separate from the
> credential: it is what makes Linear deliver agent webhooks at all. It is
> one-time setup and survives minting. Note that a live client-credentials token
> *blocks* an installation from completing (`Scope updates are not supported for
> client credentials tokens`), so revoke outstanding tokens — rotating the client
> secret does it — before installing or re-installing.

#### `[channels.linear.identity]`

Memories and `USER.md` are keyed `<channel>_<user_id>`, so the same human is a
stranger the first time they appear on a new channel. This table maps a Linear
account's email onto an identity elsewhere, so their memories, preferences and
pairing carry over:

```toml
[channels.linear.identity]
"rodrigo@example.com" = "slack:U0123ABC"
```

Matching is case-insensitive. An unmapped person becomes `linear:<their user
id>` and goes through the normal pairing flow.

#### Setting up the Linear side

1. Create an OAuth application at `https://linear.app/settings/api/applications/new`.
2. Copy its **Client ID** and **Client Secret** into `client_id` / `client_secret`.
   The app-actor token cica mints from them creates and acts as an app user in the
   workspace, so everything the agent writes is authored by it rather than by a
   colleague.
3. Scopes are requested per authorization, not configured on the app. cica asks
   for `read,write,comments:create,issues:create,app:mentionable`. It does **not**
   ask for `app:assignable`: assignment in Linear reads as "you own this now",
   which is a different promise from "answer this". Add it only if you want the
   agent delegated whole issues.
4. Add a webhook subscribed to **Agent session events**, pointed at
   `https://<your-host>/webhooks/linear`, and copy its signing secret into
   `webhook_secret`.

`cica init` walks through this and validates the credentials by resolving
`viewer`, which prints the name that will appear on every activity.

A turn is keyed to the **issue**, not the agent session, so a mention next week
resumes the same conversation rather than starting cold.

## Backends

### `[claude]`
| Key | Type | Default | Meaning |
|---|---|---|---|
| `api_key` | string | — | Anthropic API key or OAuth token (unused when `use_bedrock` or `use_vertex` is set). |
| `model` | string | — | Alias (`"sonnet"`, `"opus"`) or full model ID. |
| `use_bedrock` | bool | `false` | Use Amazon Bedrock instead of the Anthropic API. Credentials come from the AWS provider chain, so `api_key` is unused; takes precedence over `use_vertex`. |
| `bedrock_region` | string | — | AWS region for Bedrock (e.g. `"eu-central-1"`). When unset, Claude Code resolves the region from the ambient AWS environment. |
| `use_vertex` | bool | `false` | Use Google Vertex AI instead of the Anthropic API. |
| `vertex_project_id` | string | — | GCP project ID (required when `use_vertex`). |
| `vertex_region` | string | `"europe-west1"` | GCP region for Vertex. |
| `vertex_credentials_path` | string | — | Path to a GCP service-account JSON key. When set, long-lived auth is used so no interactive `gcloud login` is needed — recommended for servers. |

### `[cursor]`
| Key | Type | Default | Meaning |
|---|---|---|---|
| `api_key` | string | — | Cursor API key. |
| `model` | string | `claude-sonnet-4-20250514` | Model to use. |

## Deployment (cloud mode)

All of `[deployment]` is optional; absent means single-box. See [architecture.md](architecture.md) for what each setting does.

### `[deployment]`
| Key | Type | Default | Meaning |
|---|---|---|---|
| `store` | `"filesystem"` \| `"s3"` | none | Durable state store. None disables hydration (pure local). |
| `state_path` | string | `internal/state-store` | Root path for the filesystem store. |
| `provider` | `"local"` \| `"subprocess"` \| `"docker"` \| `"fargate"` | `local` | Where a turn executes. |
| `docker_image` | string | `cica-worker:latest` | Worker image for `provider = "docker"`. |
| `worker_idle_secs` | u64 | `600` | Drain a worker after this long without a turn. |
| `worker_start_timeout_secs` | u64 | `180` | Allow a launched worker this long to become live. |
| `turn_timeout_secs` | u64 | `900` | Per-turn worker deadline. |
| `worker_cap` | usize | `32` | Maximum warm workers owned by one router. |
| `worker_max_age_secs` | u64 | `86400` | Stop accepting turns after this age, then drain. |

### `[deployment.s3]` (when `store = "s3"`)
| Key | Type | Default | Meaning |
|---|---|---|---|
| `bucket` | string | — | Bucket name (required). |
| `region` | string | AWS chain | Region; falls back to the default chain. |
| `prefix` | string | — | Optional key namespace within the bucket. |
| `endpoint` | string | — | Endpoint override (LocalStack / MinIO / testing). |

> Credentials are **never** in config — they come from the standard AWS provider chain (env vars or instance/task IAM role).

### `[deployment.fargate]` (when `provider = "fargate"`)
Requires a build with `--features fargate`.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `cluster` | string | — | ECS cluster name or ARN (required). |
| `task_definition` | string | — | Task-def family or `family:revision` (required). |
| `subnets` | string[] | `[]` | awsvpc subnets to launch into (required in practice). |
| `security_groups` | string[] | `[]` | Security groups for the task. |
| `assign_public_ip` | bool | `false` | Assign a public IP (default: private subnets + NAT). |
| `region` | string | AWS chain | Region. |
| `container_name` | string | `cica-worker` | Which container in the task definition runs the worker loop. |
| `poll_interval_secs` | u64 | `5` | DescribeTasks poll interval. |

## Worker command

Launchers run `cica worker --session <affinity_id> --worker-id <id> --idle-secs <n> --turn-timeout-secs <n>`. The hidden `--turn <id>` compatibility mode remains available for old routers during rollout. Launchers may also pass these path overrides:

| Flag | Meaning |
|---|---|
| `--home <dir>` | Worker-local mutable data root. |
| `--deps <dir>` | Shared dependency directory. |
| `--skills <dir>` | Shared read-only skills directory. |
| `--config <file>` | Configuration file to load. Relative paths in the file resolve from its parent directory. |

## Skills git-sync

### `[skills]`
Absent means no skills sync (skills are read from the local folder only).

| Key | Type | Default | Meaning |
|---|---|---|---|
| `repo` | string | — | Git repository URL of the skills repo (required). |
| `ref` | string | `"main"` | Branch, tag, or sha to check out. |
| `refresh_secs` | u64 | `600` | Seconds between re-pulls. |

> The git credential is read from the `CICA_SKILLS_GIT_TOKEN` env var — never from config.

## Environment variables

These overlay config at load time (env wins over file). The cloud worker uses these so it needs no `config.toml`.

> With `provider = "fargate"` a worker's config is only what these variables carry. The launcher overrides the
> container command and cannot deliver the router's `config.toml`, so a setting with no variable in this table falls
> back to its default on every worker turn.

| Variable | Overrides | Notes |
|---|---|---|
| `CICA_CLAUDE_API_KEY` | `claude.api_key` | |
| `CICA_CURSOR_API_KEY` | `cursor.api_key` | |
| `CICA_CLAUDE_MODEL` | `claude.model` | Alias (`"sonnet"`, `"opus"`) or full model ID; workers use it only as a fallback. |
| `CICA_CURSOR_MODEL` | `cursor.model` | Workers use it only as a fallback. |
| `CICA_CLAUDE_USE_BEDROCK` | `claude.use_bedrock` | `1`/`true`/`yes`/`on` (or the falsy equivalents). An unrecognised value is ignored with a warning. |
| `CICA_CLAUDE_BEDROCK_REGION` | `claude.bedrock_region` | AWS region for Bedrock; omit to use the ambient AWS environment. |
| `CICA_BACKEND` | `backend` | `claude` or `cursor`; workers do not use it for turns. |
| `CICA_STORE` | `deployment.store` | `s3` or `filesystem`. |
| `CICA_STATE_PATH` | `deployment.state_path` | Filesystem state-store root; Docker workers receive `/data/cica/internal/state-store`. |
| `CICA_WORKER_IDLE_SECS` | `deployment.worker_idle_secs` | |
| `CICA_WORKER_START_TIMEOUT_SECS` | `deployment.worker_start_timeout_secs` | |
| `CICA_TURN_TIMEOUT_SECS` | `deployment.turn_timeout_secs` | |
| `CICA_WORKER_CAP` | `deployment.worker_cap` | |
| `CICA_WORKER_MAX_AGE_SECS` | `deployment.worker_max_age_secs` | |
| `CICA_S3_BUCKET` | `deployment.s3.bucket` | |
| `CICA_S3_REGION` | `deployment.s3.region` | |
| `CICA_SKILLS_GIT_TOKEN` | — | Git credential for the skills sync loop. Env-only. |
| `CICA_LOG_JSON` | — | If set, emit logs as JSON (set to any value). |
| `RUST_LOG` | — | Standard tracing filter (e.g. `info`, `cica=debug`). |
| AWS chain (`AWS_*` / instance role) | — | S3 + Fargate credentials. Never in config. |

## What's secret

Cica does not integrate a secret manager itself — in single-box mode, tokens and keys sit in `config.toml` in plaintext, so protect that file. The secret-bearing fields:

- `claude.api_key`, `cursor.api_key`
- `channels.*.bot_token` / `app_token`
- `claude.vertex_credentials_path` (points at a credentials file)
- `CICA_SKILLS_GIT_TOKEN` (env)
- AWS credentials (via the provider chain)

In cloud mode, inject these as environment variables from your platform's secret store rather than baking a `config.toml` — see your deployment's runbook.
