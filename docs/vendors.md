# Vendors

A vendor is a provider the gateway already knows: where its API is, how it
takes a key, and which formats it speaks. Set its key in the environment and
it is a provider, or name it in `gateway.toml`:

```toml
[providers.openai]
vendor = "openai"
key = "env:OPENAI_API_KEY"
```

Anything else that speaks a supported format is a provider too, given
`formats` and a `base_url` (see [Configuration](configuration.md#providers)).

Each vendor is in one of two tiers:

- 🟢 **Green**: maintained, and verified end to end against the vendor's
  live API.
- 🟡 **Yellow**: maintained, but not yet verified end to end against the
  vendor's live API. A yellow vendor becomes green once it is.

## 🟢 Green

| | Vendor | `vendor` | Key | Formats |
|:-:|---|---|---|---|
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/respan-dark.svg"><img src="assets/vendors/respan.svg" width="20" height="20" alt=""></picture> | Respan | `respan` | `RESPAN_API_KEY` | `openai-chat`, `openai-responses`, `anthropic-messages`, `gemini-generate`, `decisions` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/openai-dark.svg"><img src="assets/vendors/openai.svg" width="20" height="20" alt=""></picture> | OpenAI | `openai` | `OPENAI_API_KEY` | `openai-chat`, `openai-responses`, `openai-realtime`, `openai-live`, `openai-images`, `openai-transcriptions`, `openai-speech`, `openai-embeddings`, `openai-decisions` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/anthropic-dark.svg"><img src="assets/vendors/anthropic.svg" width="20" height="20" alt=""></picture> | Anthropic | `anthropic` | `ANTHROPIC_API_KEY` | `anthropic-messages` |
| <img src="assets/vendors/gemini.svg" width="20" height="20" alt=""> | Gemini | `gemini` | `GEMINI_API_KEY` or `GOOGLE_API_KEY` | `gemini-generate`, `openai-chat`, `gemini-live`, `gemini-images`, `gemini-omni`, `gemini-decisions`, `gemini-embeddings`, `gemini-speech`, `gemini-transcriptions` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/ollama-dark.svg"><img src="assets/vendors/ollama.svg" width="20" height="20" alt=""></picture> | Ollama | `ollama` | none; `OLLAMA_HOST` says where | `openai-chat`, `openai-embeddings` |
| <img src="assets/vendors/typesafe.png" width="20" height="20" alt=""> | TypeSafe | `typesafe` | `TYPESAFE_API_KEY` | `decisions` |

## 🟡 Yellow

| | Vendor | `vendor` | Key | Formats |
|:-:|---|---|---|---|
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/openrouter-dark.svg"><img src="assets/vendors/openrouter.svg" width="20" height="20" alt=""></picture> | OpenRouter | `openrouter` | `OPENROUTER_API_KEY` | `openai-chat`, `openai-responses`, `anthropic-messages`, `openai-images`, `openai-embeddings`, `openai-transcriptions`, `openai-speech`, `openrouter-videos`, `decisions` |
| <img src="assets/vendors/azure.svg" width="20" height="20" alt=""> | Azure | `azure` | `AZURE_OPENAI_API_KEY` | `openai-chat`, `openai-responses`, `openai-decisions`, `openai-realtime` |
| <img src="assets/vendors/bedrock.svg" width="20" height="20" alt=""> | Amazon Bedrock | `bedrock` | `AWS_BEARER_TOKEN_BEDROCK`; `AWS_REGION` | `bedrock-converse`, `anthropic-messages` (Claude), `bedrock-embeddings`, `bedrock-images` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/xai-dark.svg"><img src="assets/vendors/xai.svg" width="20" height="20" alt=""></picture> | xAI | `xai` | `XAI_API_KEY` | `openai-chat`, `openai-images`, `openai-realtime` |
| <img src="assets/vendors/mistral.svg" width="20" height="20" alt=""> | Mistral | `mistral` | `MISTRAL_API_KEY` | `openai-chat`, `openai-embeddings`, `openai-transcriptions`, `openai-speech`, `mistral-realtime` |
| <img src="assets/vendors/qwen.svg" width="20" height="20" alt=""> | Qwen | `qwen` | `DASHSCOPE_API_KEY` | `openai-chat`, `dashscope-speech`, `dashscope-transcriptions`, `dashscope-images`, `dashscope-videos`, `openai-realtime-beta` |
| <img src="assets/vendors/deepseek.svg" width="20" height="20" alt=""> | DeepSeek | `deepseek` | `DEEPSEEK_API_KEY` | `openai-chat`, `anthropic-messages` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/deepinfra-dark.svg"><img src="assets/vendors/deepinfra.svg" width="20" height="20" alt=""></picture> | DeepInfra | `deepinfra` | `DEEPINFRA_API_KEY` | `openai-chat`, `openai-images`, `openai-embeddings`, `openai-speech`, `openai-transcriptions`, `openai-videos`, `decisions` |
| <img src="assets/vendors/together.svg" width="20" height="20" alt=""> | Together | `together` | `TOGETHER_API_KEY` | `openai-chat`, `openai-images`, `openai-embeddings`, `together-videos` |
| <img src="assets/vendors/fireworks.svg" width="20" height="20" alt=""> | Fireworks | `fireworks` | `FIREWORKS_API_KEY` | `openai-chat`, `openai-embeddings`, `decisions` (at `/systemone`), `openai-decisions` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/baseten-dark.svg"><img src="assets/vendors/baseten.svg" width="20" height="20" alt=""></picture> | Baseten | `baseten` | `BASETEN_API_KEY` | `openai-chat`, `openai-responses`, `anthropic-messages`, `decisions` (at `/systemone`) |
| <img src="assets/vendors/perplexity.svg" width="20" height="20" alt=""> | Perplexity | `perplexity` | `PERPLEXITY_API_KEY` | `openai-responses`, `decisions` |
| <img src="assets/vendors/minimax.svg" width="20" height="20" alt=""> | MiniMax | `minimax` | `MINIMAX_API_KEY` | `openai-chat`, `openai-images`, `openai-transcriptions` (its speech recognition, at `/speech_to_text`), `minimax-videos`, `minimax-speech` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/zai-dark.svg"><img src="assets/vendors/zai.svg" width="20" height="20" alt=""></picture> | Z.ai | `zai` | `ZAI_API_KEY` | `openai-chat`, `openai-images`, `openai-transcriptions` (its usage in Chat Completions' counts), `zai-videos` |
| <img src="assets/vendors/cloudflare.svg" width="20" height="20" alt=""> | Cloudflare | `cloudflare` | `CLOUDFLARE_API_TOKEN` | `openai-chat`, `openai-embeddings` |
| <img src="assets/vendors/vllm.svg" width="20" height="20" alt=""> | vLLM | `vllm` | none | `openai-chat` |
| <img src="assets/vendors/empiriolabs.png" width="20" height="20" alt=""> | EmpirioLabs AI | `empiriolabs` | `EMPIRIOLABS_API_KEY` | `openai-chat`, `openai-responses`, `anthropic-messages`, `gemini-generate`, `openai-images`, `openai-embeddings`, `openai-speech`, `openai-realtime`, `decisions`, `empiriolabs-videos` |
| <img src="assets/vendors/stepfun.svg" width="20" height="20" alt=""> | StepFun | `stepfun` | `STEPFUN_API_KEY` | `openai-chat`, `openai-speech`, `openai-transcriptions`, `openai-realtime-beta` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/primeintellect-dark.png"><img src="assets/vendors/primeintellect.png" width="20" height="20" alt=""></picture> | Prime Intellect | `primeintellect` | `PRIME_API_KEY` | `openai-chat`, `decisions` (its evaluations, at `/evaluations`) |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/morph-dark.svg"><img src="assets/vendors/morph.svg" width="20" height="20" alt=""></picture> | Morph | `morph` | `MORPH_API_KEY` | `openai-chat`, `decisions` (at `/systemone`) |
| <img src="assets/vendors/deeprelay.svg" width="20" height="20" alt=""> | DeepRelay | `deeprelay` | `DEEPRELAY_API_KEY` | `openai-chat`, `openai-embeddings`, `openai-images`, `openai-videos` (a creation's `seconds` a number) |
| <img src="assets/vendors/opencode.svg" width="20" height="20" alt=""> | [OpenCode Zen](#opencode-zen) | `opencode` | `OPENCODE_API_KEY` | `openai-chat`, `openai-responses`, `anthropic-messages` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/phonic-dark.svg"><img src="assets/vendors/phonic.svg" width="20" height="20" alt=""></picture> | Phonic | `phonic` | `PHONIC_API_KEY` | `phonic-sts` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/groq-dark.svg"><img src="assets/vendors/groq.svg" width="20" height="20" alt=""></picture> | Groq | `groq` | `GROQ_API_KEY` | `openai-chat`, `openai-transcriptions`, `openai-speech` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/cerebras-dark.svg"><img src="assets/vendors/cerebras.svg" width="20" height="20" alt=""></picture> | Cerebras | `cerebras` | `CEREBRAS_API_KEY` | `openai-chat` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/concentrate-dark.svg"><img src="assets/vendors/concentrate.svg" width="20" height="20" alt=""></picture> | Concentrate | `concentrate` | `CONCENTRATE_API_KEY` | `openai-chat`, `openai-responses`, `decisions` (at `/systemone`), `openai-decisions` |

These tables are kept by hand: a vendor added to `ALL` in
[`crates/vendors`](../crates/vendors/README.md), or a format added to a
vendor, is a row or an entry here too.

MiniMax H3 image calls use `https://api.minimax.io/v1` and the shared
OpenAI Images format. Partial failures are retained as `failed_images`,
with their original indices and errors, alongside successful images and usage.
Upstream calls use JSON responses; clients requesting SSE receive the shared
client framing after generation completes, rather than MiniMax's image-by-image
stream. Masks and transparent backgrounds are refused before sending.
This integration has not been verified against the live API.

MiniMax speech, `minimax-speech`, is called at the same base URL, `/t2a_v2`; the
key is a bearer token.

MiniMax's speech recognition is OpenAI's transcriptions at `/speech_to_text`
of the same base URL. Its seconds are an answer's `duration`, and its stream
is answered in OpenAI's events once it ended. A `text`, `srt` or `vtt` answer
is asked as `verbose_json` and rendered from its segments; a translation is
refused before sending, and a `language` is not passed on.

xAI's images take no `size`: it is sent as the nearest `aspect_ratio` xAI
draws (within 5%, else 400) and a `resolution`, `2k` above 1024 pixels on the
long side, else `1k`. xAI takes no form, so an edit's images are sent in JSON
as data URLs of their bytes.

## Base URLs

A provider's `base_url` is the one its own SDK takes. OpenAI-compatible ones
include the version: `https://api.openai.com/v1`,
`https://api.groq.com/openai/v1`, `https://openrouter.ai/api/v1`,
`https://api.cerebras.ai/v1`,
`https://api.empiriolabs.ai/v1`, `https://api.deepinfra.com/v1/openai`, and
Gemini's OpenAI-compatible endpoint
`https://generativelanguage.googleapis.com/v1beta/openai` (all
`openai-chat`). Anthropic's does not: `https://api.anthropic.com`, nor
EmpirioLabs AI's and OpenRouter's for `anthropic-messages`: `https://api.empiriolabs.ai`, `https://openrouter.ai/api`. The media
formats (`openai-images`, `openai-transcriptions`, `openai-speech`,
`openai-videos`, `openai-embeddings`), `openrouter-videos` and
`openai-responses` take the same base URL as `openai-chat`; Together's
`together-videos`, EmpirioLabs AI's `empiriolabs-videos` and MiniMax's
`minimax-videos` take their host alone, `https://api.together.xyz`,
`https://api.empiriolabs.ai` and `https://api.minimax.io`. Gemini's own formats, `gemini-generate`
(`generateContent` and `streamGenerateContent`, the model in the path),
`gemini-images` (its image models' `generateContent`), `gemini-speech` (its
speech models' `interactions`), `gemini-transcriptions` (its transcription
models' `generateContent`), `gemini-omni` and
`gemini-decisions` (at `/decisions`, the model in the body), take `https://generativelanguage.googleapis.com/v1beta` with
the key in `x-goog-api-key`; EmpirioLabs AI serves `gemini-generate` at
`https://api.empiriolabs.ai/v1beta`. DeepInfra serves `decisions` at
`https://api.deepinfra.com/v1`, not under its `/openai` base, so a DeepInfra
provider with a `base_url` names the formats it covers, or gives each format
its own URL in `formats`. Session formats take a `wss://` URL as their
SDK does: `wss://api.openai.com/v1` for `openai-realtime` (EmpirioLabs AI's
at `wss://api.empiriolabs.ai/v1`, which takes a transcription session's model
at `?model=` too; xAI's, which its Grok Voice models speak, at
`wss://api.x.ai/v1`; Azure's at its resource's
`wss://<resource>.openai.azure.com/openai/v1`, the deployment at `?model=`
and the key in `api-key`) and
`openai-live` (OpenAI's Live API, which its Live models
such as `gpt-live-1` speak, and which OpenAI sends a session with one in),
`wss://generativelanguage.googleapis.com` for `gemini-live`,
`wss://api.mistral.ai/v1` for `mistral-realtime`: Mistral's realtime
transcription, the key a bearer token and the model at `?model=`, which
transcribes for an OpenAI Realtime or Gemini Live transcription as its own
clients transcribe with theirs; `wss://api.phonic.ai/v1` for `phonic-sts`:
Phonic's speech-to-speech, which its models such as `phonic-1` speak, the
key a bearer token and the model in its `config`.
A provider that still speaks OpenAI Realtime's beta protocol takes
`openai-realtime-beta` and its own `wss://` base URL, such as
`wss://api.stepfun.ai/v1`: the key a bearer token and the model at
`?model=`, its clients speaking `openai-realtime`.
Qwen's Omni Realtime models speak it too, at
`wss://dashscope-intl.aliyuncs.com/api-ws/v1`. It hears PCM at 16 kHz, which
the user's audio is converted to, and speaks it at 24 kHz; a session names
neither format to it.
Perplexity's Agent API speaks `openai-responses` at
`https://api.perplexity.ai/v1`; its models are named `<vendor>/<model>`,
such as `openai/gpt-5.6-sol`. Its `decisions` are at the same base, and
their models are `pplx-decider-*`.
OpenRouter's `decisions` are in alpha, at `https://openrouter.ai/api/alpha`.
Baseten's `anthropic-messages` take its host alone, `https://inference.baseten.co`,
the key a bearer token.

Bedrock takes a Bedrock API key. The gateway does not sign calls with an
AWS key pair (SigV4): as with an HTTP proxy, a hop outside it does. Run
AWS's [`aws-sigv4-proxy`](https://github.com/awslabs/aws-sigv4-proxy) with
`--name bedrock --region <region>` and set the provider's `base_url` to it,
with no key. Its models are then not listed, as the proxy reaches only the
runtime.
Its InvokeModel formats, `bedrock-embeddings` (Cohere Embed and Titan) and
`bedrock-images` (Stability AI's models and services), are called at the same
base URL, `https://bedrock-runtime.<region>.amazonaws.com`.

DashScope's own formats, `dashscope-speech`, `dashscope-transcriptions`,
`dashscope-images` and `dashscope-videos` (Alibaba Cloud Model Studio's
speech synthesis, recognition, Qwen-Image and Wan image generation and Wan
video generation, which its
OpenAI-compatible endpoint does not serve), take its region's API with the
version, `https://dashscope-intl.aliyuncs.com/api/v1`, the key a bearer token.

Z.ai's formats, its OpenAI-compatible ones and `zai-videos` (CogVideoX
video generation), take its international site's API with the version,
`https://api.z.ai/api/paas/v4`, the key a bearer token. Its transcriptions
count their usage in Chat Completions' `prompt_tokens` and
`completion_tokens`, read as OpenAI's input and output tokens.

DeepRelay's formats take its API with the version, `https://api.deeprelay.ai/v1`,
the key a bearer token. Its videos take a creation's `seconds` as a number,
not OpenAI's string, and end `cancelled` or `expired`, read as `failed`.

### OpenCode Zen

`openai-chat` and `openai-responses` use `https://opencode.ai/zen/v1`
with a bearer key. `anthropic-messages` uses
`https://opencode.ai/inference/anthropic` with `x-api-key`. Choose a
format supported by the model you route to, as listed in the
[Zen model catalog](https://opencode.ai/docs/zen/#endpoints).

The [OpenCode dialect](../crates/vendors/yellow/src/opencode.rs) carries
Zen's session headers even when the gateway translates the request. If
the client provides no session identity, the gateway derives one from
its remaining credentials or generates a fresh one if none remain.
OpenCode's free-model access rules (`403`) and Go subscription limits
still apply.

### Other base URLs

Meta's `meta-transcriptions` (speech recognition at `/asr/transcribe`) takes
its API with the version, `https://api.meta.ai/v1`, the key a bearer token.

Respan's formats sit under one API, `https://api.respan.ai/api`:
`openai-chat` and `openai-responses` at it, `anthropic-messages` at
`/anthropic`, `gemini-generate` at `/google/gemini/v1beta` (its key in
`x-goog-api-key`, as Google's), `decisions` at `/v1`. Its models are its public catalog,
`https://www.respan.ai/api/gateway-models/list`, read a page at a time and
sent no key; a provider at another base URL lists at
`{base}/gateway-models/list`.

`decisions` is called at `{base}/decisions`, as Respan serves it. TypeSafe
serves it at `/systemone` instead, which `vendor = "typesafe"` knows; a
provider of no vendor at TypeSafe's URL calls `/decisions` and is not found.

Without a config file, `RESPAN_<VENDOR>_BASE_URL` moves a vendor's base URL
(`OLLAMA_HOST` for `ollama`); with one, only the file's `base_url` counts
(`respan-gateway init` writes the moved URL into it). `azure` and `vllm`
need a `base_url`; `cloudflare` needs `CLOUDFLARE_ACCOUNT_ID`, and `bedrock`
`AWS_REGION`, or one.

How each vendor's quirks are handled, and how to add one, is in
[`crates/vendors`](../crates/vendors/README.md).

---

Logos are each vendor's trademark, shown only to identify it. Most are from
[LobeHub Icons](https://github.com/lobehub/lobe-icons) (MIT).
