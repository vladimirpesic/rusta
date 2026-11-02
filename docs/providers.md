# Providers

Rusta speaks to four kinds of endpoint: **Kimi** (Moonshot AI), **GLM** (Z.AI),
**DeepSeek**, and any **OpenAI-compatible** chat-completions API. Each provider
has its own wire handling in `lm-bridge`, so a request carries exactly the
fields its endpoint documents and accepts.

- [Choosing an endpoint](#choosing-an-endpoint)
- [Kimi](#kimi)
- [GLM (Z.AI)](#glm-zai)
- [DeepSeek](#deepseek)
- [OpenAI-compatible endpoints](#openai-compatible-endpoints)
- [Preserved Thinking](#preserved-thinking)

## Choosing an endpoint

Kimi and Z.AI sell the same models two ways: a **Coding Plan** subscription and
**pay-per-token** API credits. The key decides which plan it is billed against.
Kimi refuses a Coding Plan key at the pay-per-token endpoint, and vice versa, with
an `invalid_authentication_error` that reads like a bad key. Z.AI takes a key at
either endpoint and bills it by its type, while an older key made for an account
with a Coding Plan is billed by the endpoint, so at the pay-per-token one it uses
the account balance (`1113` "Insufficient balance" when there is none, which
Rusta explains). So the billing plan — not just the provider — selects the
endpoint, and with it the default model and, for GLM, the web tools: set
`--billing` to the key's type.

| `--provider` | `--billing` | Default model | Context | Base URL |
| --- | --- | --- | --- | --- |
| `kimi` | `subscription` *(default)* | `kimi-for-coding` | 1,048,576 | `https://api.kimi.com/coding/v1` (or [`api.kimi.ai`](#coding-plan-hosts)) |
| `kimi` | `api` | `kimi-k3.1-pro` | 1,048,576 | `https://api.moonshot.ai/v1` |
| `glm` (alias `zai`) | `subscription` *(default)* | `glm-5.3` | 1,048,576 | `https://api.z.ai/api/coding/paas/v4` |
| `glm` (alias `zai`) | `api` | `glm-5.3` | 1,048,576 | `https://api.z.ai/api/paas/v4` |
| `deepseek` | — | `deepseek-flash` | 1,048,576 | `https://api.deepseek.com` |
| `openai` | — | `gpt-4o-mini` | — | `https://api.openai.com/v1` |

- **Kimi** names its models differently at each door, and neither door knows the
  other's ids. `kimi-for-coding` is the Coding Plan's name for Kimi's current
  coding model, upgraded in place: it serves K2.8 Preview with a 1,048,576-token
  window on every plan that includes coding. On the pay-per-token platform Rusta
  defaults to `kimi-k3.1-pro`, Kimi's flagship, which the platform unlocks after
  a top-up of at least $1.
- **Z.AI** serves `glm-5.3` on every endpoint, so there the billing changes only
  the URL.
- **DeepSeek** has no subscription product, so `--billing` does not apply; a
  `billing` value in the config is accepted and ignored for `--provider deepseek`.
- **`--region china`** selects the mainland deployments: `api.moonshot.cn` for
  Kimi pay-per-token, and `open.bigmodel.cn` for both GLM plans
  (`/api/coding/paas/v4` and `/api/paas/v4`). It does not pick the Kimi Coding
  host: the account picks it.

Any other provider is rejected at startup. `--model` overrides the model and
`--base-url` overrides the endpoint, whatever the billing implies. The endpoint
and model table lives in one place, `crates/rusta-cli/src/endpoints.rs`, so
`init`, `doctor` and a session always agree.

## Kimi

### Coding Plan hosts

Kimi's Coding Plan has two OpenAI-compatible hosts: `https://api.kimi.com/coding/v1`
for kimi.com accounts and `https://api.kimi.ai/coding/v1` for kimi.ai (overseas)
accounts. Each host refuses the other's keys with the same
`invalid_authentication_error` a mistyped key gets. Rusta handles this in three
places:

- **`rusta init`** asks both hosts and pins the one that accepts the key as
  `base_url`.
- **At run time**, with no `base_url` pinned, a key the default host refuses is
  retried once on the other host. The session stays on whichever host accepted
  it, and a log line suggests pinning it. A refusal that names a plan limit (no
  K3 access, K3 past 256K, no HighSpeed) comes from the host that holds the key,
  so it is reported as it is.
- **`rusta doctor`** names the host that accepts the key.

### Models

The Coding Plan serves six model ids:

| Model id | Model | Context | Input | Effort (default) | Plans |
| --- | --- | --- | --- | --- | --- |
| `kimi-for-coding` *(default)* | K2.8 Preview | 1,048,576 | image, video | `low`/`high`/`max` (`max`) | Plus and above (legacy: Andante and above) |
| `kimi-for-coding-highspeed` | K2.7 Code Highspeed: ~6× speed, ~3× quota | 262,144 | image, video | none; always thinks | Pro and above (legacy: Allegretto and above) |
| `k3` | K3 | 1,048,576 from Pro, 262,144 on Plus | image, video | `low`/`high`/`max` (`high`) | Plus and above (legacy: Moderato and above) |
| `k3-256k` | K3 (about half `k3`'s quota) | 262,144 | image | `low`/`high`/`max` (`high`) | Plus and above (legacy: Moderato and above) |
| `k3.1-pro` | K3.1 Pro | 1,048,576 from Pro, 262,144 on Plus | image, video | `low`/`high`/`max` (`high`) | Plus and above (legacy: Moderato and above) |
| `k3.1-pro-highspeed` | K3.1 Pro Highspeed: ~2–3× speed, ~2× quota | 1,048,576 | image, video | `low`/`high`/`max` (`high`) | Pro and above (legacy: Allegretto and above) |

The pay-per-token platform (`--billing api`) names its models by version:

| Model id | Context | Thinking | Effort (default) | Dynamic tools |
| --- | --- | --- | --- | --- |
| `kimi-k3.1-pro` | 1,048,576 | always | `low`/`high`/`max` (`max`) | yes |
| `kimi-k3.1-pro-highspeed` | 1,048,576 | always | `low`/`high`/`max` (`max`) | yes |
| `kimi-k3` | 1,048,576 | always | `low`/`high`/`max` (`max`) | yes |
| `kimi-k2.7-code` | 262,144 | always | none | no |
| `kimi-k2.7-code-highspeed` | 262,144 | always | none | no |
| `kimi-k2.6` | 262,144 | on request | none | no |

All of them take images. Video goes through Kimi's Files API, which Rusta uses
on the Kimi Code endpoints only, so on the platform video is not sent.
Moonshot's retired models (`moonshot-v1-*`, `kimi-k2.5` and the dash-versioned
`kimi-k2-*` ids) answer 404 and are not listed.

Rusta does not depend on these tables staying current:

- **Live list.** It asks the endpoint (`GET /models`) for each model's window,
  image and video input, thinking and effort levels, so a new model or a changed
  window needs no Rusta release. Each model is read in the endpoint's unified
  schema (`limit.context`, `reasoning`, `modalities.input`,
  `tools.dynamic_load`), or from the older flat fields when an endpoint serves
  only those. The platform's list carries four flat fields (window, image and
  video input, whether the model reasons); what it leaves out — that the K3
  series always thinks, its effort levels, its dynamic tools — comes from the
  table above.
- **Cache.** The list is cached per host for a day in `~/.cache/rusta/`.
  Startup refreshes it in the background. `rusta init` and `rusta doctor`
  refresh it on the spot and print it, and `/provider refresh` does in a
  session.
- **Fallback.** The tables above are built in, for a machine that has never
  reached the endpoint.
- **`/model`** with no argument lists the provider's models with Kimi's name for
  each (`kimi-for-coding — K2.8 Preview`) and marks the one in use.

Rusta **defaults to `kimi-for-coding`**: every plan that includes coding runs it
with a 1M window, while `k3` and `k3.1-pro` get 1M only from Pro (Plus runs them
at 262,144) and are not on the legacy Andante plan. To run K3.1 Pro, select it:

```bash
rusta --model k3.1-pro -p "…"    # one invocation
export KIMI_MODEL_NAME=k3.1-pro  # or pin it for the shell
```

or pin it in the config with `model = "k3.1-pro"`. Start a new session for it:
Kimi advises against switching to K3.1 inside a session begun on another model,
which lowers output quality and cache hits (see
[confirmed model switches](#confirmed-model-switches)). `k3.1-pro-highspeed` is
the same model 2–3× faster, at about twice the quota. Check the spelling of an
id: Kimi may serve a mistyped one with K2.8 Preview, without thinking, rather
than refuse it.

### Reasoning effort

`--reasoning-effort` (or `reasoning_effort` in the config) is sent only to models
that list effort levels. On the Kimi Code endpoints it goes inside the thinking
object as `thinking.effort`, where Kimi validates it; the platform's K3 series
takes it as a top-level `reasoning_effort` and no `thinking` object, which is a
K2.x parameter there.

| You pass | Kimi receives |
| --- | --- |
| `ultra`, `max`, `xhigh` | `max` |
| `high`, `medium` | `high` |
| `low`, `minimum`, `minimal`, `light` | `low` |
| `off`, `none`, `disabled` | thinking off, or the lowest effort where it cannot be turned off (below) |
| unset or unrecognized | nothing, so the model default applies (`max` on K2.8 Preview and the platform's K3 series, `high` on the Kimi Code `k` ids) |

- **Unrecognized values.** Kimi rejects an unknown effort with HTTP 400, so
  Rusta drops a value it does not recognize instead of sending it.
- **Missing levels.** A model without the mapped level gets the nearest level it
  has.
- **Thinking off** (`--reasoning-effort none`, or `thinking = false` where a
  request asks for it, such as the session title):
  - `kimi-for-coding`, `kimi-for-coding-highspeed`, `k3`, `k3-256k` and
    `kimi-k2.6` get `thinking: {"type": "disabled"}`. Kimi Code serves the
    first four with K2.8 Preview, without thinking.
  - K3.1 Pro (`k3.1-pro`, `k3.1-pro-highspeed`) answers `disabled` with HTTP
    400, the platform's K3 series has no off switch, and `kimi-k2.7-code`'s
    thinking cannot be disabled. These run at their lowest effort instead
    (`low`; thinking without an effort on `kimi-k2.7-code`), as Kimi advises
    for K3.1 Pro, rather than at the configured one.
- **Changing effort mid-session** invalidates Kimi's context cache, so pick one
  effort per session.

### Prompt cache

Kimi caches a repeated request prefix by itself, and every request carries a
cache key. The platform bills each cache write for the K3 series once, with a
lifetime: five minutes by default, or an hour at twice the write price; every hit
renews the entry, and a hit costs a tenth of an uncached token. An interactive
session's turns wait on you, often for longer than five minutes, so Rusta asks
for `1h` there (`prompt_cache_options: {"mode": "implicit", "ttl": "1h"}`); a
headless run's steps follow each other within seconds, so it takes the default.
Set `--prompt-cache-ttl` (`RUSTA_PROMPT_CACHE_TTL`, `prompt_cache_ttl`) to `5m`
or `1h` to choose. An entry keeps the lifetime it was first written with. Kimi
Code and the platform's K2 models bill no cache writes and are sent nothing. The
writes a request made are reported as `input_cache_creation` in `--stream-json`
usage events, beside the reads (`input_cache_read`).

### Sampling

Every model Kimi serves fixes its temperature (1.0; 0.6 for `kimi-k2.6` without
thinking) and top-p (0.95), and answers any other value with HTTP 400. So the
Kimi provider sends neither: a configured `temperature` or `top_p` is dropped,
with a warning once per run.

### Confirmed model switches

On the Kimi Coding Plan, `/model <id>` confirms a switch Kimi asks to confirm.
Kimi serves the rules (`POST /client_configs`, read once per run) and can change
them without a Rusta release. They name a switch to K3.1 inside a session begun
on another model, which lowers output quality and cache hits, and a switch to
K3.1 Pro Highspeed, which costs about twice K3.1 Pro's quota.

- **In a session that has begun,** the switch waits. The reply gives Kimi's
  reason and three ways on: `/new` and then `/model <id>`, for a fresh session on
  the new model; `/compact` and then `/model <id>`, to continue this one at lower
  cost and better quality; or the same `/model <id>` again, to switch as it is.
- **In a new session,** the switch is made at once, with Kimi's note.

A run that cannot read the rules confirms nothing.

### Kimi Code services

On the Coding Plan, Rusta also uses the plan's own services with the same key:
`/search` and `/fetch` back the `web_search`, `$web_search` and `web_reader`
tools (see [Tools](tools.md#web-tools)), `/tools` writes session titles, and
`/files` takes images and video by reference: a file is uploaded once, referenced
as `ms://<id>` afterwards, and uploaded again if Kimi no longer has it.

A transient `402 Unable to verify membership benefits`, which Kimi's error
reference lists as retryable, is retried at least five seconds apart.

## GLM (Z.AI)

### GLM-5.3

`glm-5.3` is Z.AI's flagship and Rusta's default for `--provider glm` on both
billing doors: 1,048,576-token context, 131,072-token maximum output,
text-only. Three of its rules are handled on the wire:

- **Reasoning cannot be switched off.** `thinking: {type: "disabled"}` is
  rejected. Rusta never sends it to `glm-5.3` or newer; `thinking = false` and
  `--reasoning-effort none` are honoured as `reasoning_effort: "low"` — what
  Z.AI's migration guide prescribes — rather than failing the request.
- **A three-rung effort ladder.** GLM-5.3 serves `low`, `high` and `max`
  (default `max`). Rusta accepts the full seven-rung vocabulary and folds it onto
  those three; nothing is dropped, because a dropped `reasoning_effort` falls
  back to `max`, the most expensive setting.
- **Streamed tool arguments.** `tool_stream: true` goes on any streaming request
  that carries tools, for the `glm-4.6`, `glm-4.7` and `glm-5` lines Z.AI
  documents it for.

Every rule is version-gated, so older GLM models keep the request body they
document. `reasoning_effort` goes to GLM-5.2 and newer only, and only while
thinking is on. Sampling is clamped to Z.AI's documented ranges (temperature
`[0.0, 1.0]`, top-p `[0.01, 1.0]`).

### GLM-5.3-Flash

`glm-5.3-flash` is GLM-5.3 with image and video input. It shares GLM-5.3's
1,048,576-token window, 131,072-token output, always-on thinking and
`low`/`high`/`max` effort, so the rules above apply to it too.

- **Images** go out as `data:` URLs in PNG, JPEG or WebP. GIF and BMP are
  converted first: Z.AI reads only a GIF's first frame and refuses BMP.
- **Video** goes inline.
- **`glm-5.3-flashx`** is the faster variant, on the pay-per-token platform only.

GLM's vision line takes images: `glm-5.3-flash*`, `glm-5v-turbo`, `glm-4.6v*`
(128K) and `glm-4.5v` (64K). Every other GLM is text-only, and Z.AI refuses an
image sent to one with error 1210.

### Coding Plan models

The Coding Plan serves two models, `glm-5.3` and `glm-5.3-flash`. Other ids
still get an answer, but often from a different model:

| Requested | On the Coding Plan |
| --- | --- |
| `glm-5.3`, `glm-5.3-flash` | Served |
| `glm-5`, `glm-5.1`, `glm-5.2` | Answered by `glm-5.3` |
| `glm-4.5`, `glm-4.5-air`, `glm-4.6`, `glm-4.7`, `glm-5-turbo` | Answered by `glm-5.3-flash` |
| `glm-4.5-flash`, `glm-4.7-flash`, `glm-4.6v`, `glm-4.6v-flash`, `glm-4.5v` | Served under their own names, but undocumented |
| `glm-5v-turbo`, `glm-5.3-flashx` | Refused with `1311` |
| `glm-4.5-x`, `glm-4.5-airx`, `glm-4.7-flashx`, `glm-4.6v-flashx`, `glm-4-32b-0414-128k` | Refused with `1113`: billed per token, outside the plan |

Z.AI's devpack overview documents the routing of GLM-5.2, 5.1 and 4.7; the rest
of the table comes from probing the endpoint, which reports the model that
answered in each response. How Rusta handles it:

- **Routed ids.** Rusta follows the model that actually answers: the id gets that
  model's window, thinking and effort ladder. On the Coding Plan, `glm-5.1` is a
  1M-context GLM-5.3.
- **Images.** Z.AI checks images against the id in the request, so a routed
  `glm-4.7` still takes none.
- **Tooling.** `/model` lists the two plan models. `rusta doctor` warns about a
  routed or undocumented id and fails on a refused one. A `1311` error names the
  models to use instead.

The pay-per-token endpoint serves every id as itself.

Z.AI accepts at most 128 tools per request. The tool table is trimmed to fit,
and `select_tools` loads nothing past the cap and says why.

### Z.AI MCP servers

With `--provider glm` and a Coding Plan key (`--billing subscription`, the
default), Rusta connects Z.AI's hosted MCP servers on the platform that issued
the key: `api.z.ai`, or `open.bigmodel.cn` with `--region china`. Z.AI serves
them to Coding Plan keys only, so a pay-per-token key gets none of them; a
vision server named with `Z_AI_VISION_MCP_URL` is still used. The search, reader
and Zread servers only read, which Z.AI does not declare, so Rusta sets
`read_only` on them and the `explore` and `plan` sub-agents get their tools too.

| Server | Tools | Loaded |
| --- | --- | --- |
| `zai_web_search` | `web_search_prime` | Always |
| `zai_web_reader` | `webReader` | Always |
| `zai_zread` | `search_doc`, `get_repo_structure`, `read_file`, for public GitHub repositories | On demand, with `select_tools` |
| `zai_vision` | `analyze_image` and specialised image tools (`extract_text_from_screenshot`, `diagnose_error_screenshot`, `understand_technical_diagram`, `analyze_data_visualization`, `ui_to_artifact`, `ui_diff_check`); `analyze_video` (local or remote video up to 8 MB: MP4, MOV, M4V) | Only when needed (below) |

`zai_vision` is `@z_ai/mcp-server`, run with `npx` (Node.js 22 or newer):

- **Environment.** It gets the key as `Z_AI_API_KEY` and its platform as
  `Z_AI_MODE`: `ZAI`, or `ZHIPU` for mainland keys.
- **What it does.** It describes images for a model without vision, and
  analyses video for a model that cannot take it. An image `read_media` reads
  reaches its `analyze_image` as a temporary local PNG or JPEG file (a remote
  server gets it inline), with a request to describe it in detail, or the
  model's `question` about it.
- **When it runs.** It is not started when every GLM model the session runs
  reads images and video natively: the main model and the `[subagent]` pool's,
  which means GLM-5.3-Flash. If such a session later switches to a text model,
  its images go undescribed.
- **Remote server.** `Z_AI_VISION_MCP_URL` points it at a remote server instead.

A `[[mcp_servers]]` entry with any of these names replaces the injected server,
and `disabled = true` turns it off. `rusta doctor` checks that the vision server
has its key and platform.

On the pay-per-token platform, `web_search` and `web_reader` use Z.AI's own
`/web_search` (engine `search-prime`, or `search_pro` on `open.bigmodel.cn`) and
`/reader`, which are paid per use.

## DeepSeek

`--provider deepseek` speaks DeepSeek's OpenAI-compatible API
(`https://api.deepseek.com`, Bearer key from `DEEPSEEK_API_KEY`). What each model
can do comes from DeepSeek's live catalogue (`GET /models`): its context window,
its output ceiling, whether it takes images, and the effort levels its thinking
accepts. The catalogue is cached for a day and refreshed in the background (or
at once with `/provider refresh`), so a new model or a changed limit needs no
Rusta release. Until one is cached, the built-in table applies; both models are
1M-context, 384K-max-output hybrid thinking models:

| Model id | Notes |
| --- | --- |
| `deepseek-flash` *(default)* | V4.1-Flash, the model DeepSeek recommends: ahead of V4-Pro on its agentic-coding benchmarks, and cheaper and faster. Takes JPEG/PNG/GIF/WebP images, in user messages and in tool results. DeepSeek's legacy `deepseek-v4-flash*` names resolve to it |
| `deepseek-v4-pro` | V4-Pro, the previous generation. Text-only: it accepts an attached image but answers that it cannot see it |

Neither takes video. A model id that neither the catalogue nor the table lists is
taken as text-only. `/model` lists the known models; `DEEPSEEK_MODEL_NAME` pins
one. The wire specifics, all handled in the provider:

- **Thinking is on by default** and is toggled with a top-level
  `thinking: {type: "enabled" | "disabled"}`. `--reasoning-effort` is sent as a
  top-level `reasoning_effort`, mapped as DeepSeek documents:
  `ultra`/`max` → `max`, `high`/`medium`/`xhigh` → `high`,
  `low`/`minimum`/`minimal`/`light` → `low`, `none`/`off` → thinking off with
  effort `none`. A level the model does not list folds onto the nearest one it
  does. An unrecognized value is omitted, so the model default (`high`) applies.
- **Effort defaults to DeepSeek's own `high`.** DeepSeek recommends `high` for
  everyday agent work. For hard tasks and benchmarks pass
  `--reasoning-effort max` (or `reasoning_effort = "max"`): it spends more output
  tokens, and the output room below is sized for it.
- **An explicit `max_tokens` cap is always sent.** DeepSeek's own default is low
  (8K non-thinking, 64K thinking, 128K at `max` effort), and its documented
  ceiling is 384K (393,216). Unless you set one, Rusta sends
  `min(context window − input, 256K)`; a `--max-tokens` of your own may go up to
  the model's output ceiling.
- **Compaction keeps 256K free for output.** It runs when the conversation passes
  786,432 tokens of the 1M window rather than at 90 %, so a long reasoning reply
  at `max` effort still fits.
- **Temperature is never injected.** DeepSeek ignores it while thinking is on,
  every current model's default, so Rusta adds none.
- **Images follow the model.** On `deepseek-flash`, pasted images and
  `read_media` results go out inline as `data:` URLs (JPEG/PNG/GIF/WebP), at
  most 4096 px per side: DeepSeek rejects larger images once a request carries
  15 of them, so a larger one is scaled down even at full resolution.
  `deepseek-v4-pro` takes none, so on it an image `read_media` reads is
  described by `deepseek-flash` (thinking off) on the same key, and the
  description is what the model gets (or the answer to the `question` it asked);
  an image already in the history is replaced with a text note.
- **Images go inline, not through DeepSeek's Files API.** Uploading would keep
  your images on DeepSeek's servers for days and add an id cache with expiry and
  a storage quota to manage, to save only bandwidth: the 20 MB per-request media
  budget keeps every request well under DeepSeek's 48 MiB limit, and inline
  images reuse the prompt cache just as file ids would.
- **Prior turns' `reasoning_content` is echoed back** in the history: DeepSeek
  rejects a tool-carrying request that drops it with a 400.
- **A 402 means the account's balance is spent.** It surfaces as a fatal
  "insufficient DeepSeek balance" error pointing at the top-up page, not as a
  retry. 429s and 5xx retry per the usual rules.
- **`rusta doctor`** proves the key with a `GET /models` (no tokens spent),
  lists what each model can do, caches the catalogue, and shows the account's
  balance per currency (`GET /user/balance`), as an error when DeepSeek says the
  account cannot make requests.

`web_search` and `web_reader` use DeepSeek's Search and Fetch APIs; see
[Tools](tools.md#web-tools).

## OpenAI-compatible endpoints

`--provider openai` speaks any OpenAI-compatible `/chat/completions` endpoint:
`https://api.openai.com/v1` with `OPENAI_API_KEY` by default, or a gateway or a
local server with `--base-url` (`OPENAI_BASE_URL`). The reasoning dialect is
detected per endpoint: `reasoning_content`, `reasoning` (GPT-OSS, vLLM) and
`reasoning_details` (OpenRouter-style gateways) are all read, and thinking is
echoed back under whichever key the endpoint used. `reasoning_key` in the config
(`RUSTA_REASONING_KEY`) fixes the key instead. A structured `reasoning_details`
array is sent back unchanged on the next request, and a gateway's mid-stream
`error` object is classified like an error response. No `max_tokens` is sent
unless one is configured: strict endpoints reject a cap above the model's real
output limit. `inline_video = true` (`RUSTA_INLINE_VIDEO=1`) sends video inline
to a vision model there.

## Preserved Thinking

Preserved Thinking carries a turn's reasoning content into the next request
instead of discarding it. Rusta replays each stored reasoning block back to the
provider unchanged, which is what the vendors require. The providers spell it
differently, and Rusta translates:

| | Kimi | GLM (Z.AI) |
| --- | --- | --- |
| Wire field | `thinking.keep` | `thinking.clear_thinking` |
| Enabled | `"all"` | `false` |
| Disabled | field omitted | `true` |
| Sent to | `kimi-for-coding*`, the Kimi Code `k` ids, and `kimi-k2.6` and newer | every thinking model |
| Default when unset | on, whenever `thinking` is on | the endpoint decides |

- **Kimi.** The `kimi-for-coding` aliases accept `keep: "all"`, so they
  preserve reasoning by default, as the `k` ids and `kimi-k2.6` and newer do.
  The platform's K3 series always preserves its reasoning and takes no
  `thinking` object, so nothing is sent to it. Any other unversioned Kimi alias
  is left alone by default, since Moonshot rejects a property its schema does
  not declare; ask for it explicitly to send it anyway.
- **GLM.** Z.AI's Coding Plan endpoint — Rusta's default GLM base URL —
  preserves reasoning by default; the standard API endpoint does not. Leaving
  `thinking_keep` unset lets the endpoint's default stand.
- **DeepSeek** needs prior `reasoning_content` echoed back on every request that
  carries tools, so it is always replayed there.

Precedence is `--thinking-keep` > `KIMI_MODEL_THINKING_KEEP` > `thinking_keep` in
the config file > the model's own default.
