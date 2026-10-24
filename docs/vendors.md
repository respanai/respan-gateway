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
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/openai-dark.svg"><img src="assets/vendors/openai.svg" width="20" height="20" alt=""></picture> | OpenAI | `openai` | `OPENAI_API_KEY` | `openai-chat`, `openai-responses`, `openai-realtime`, `responses-ws`, `openai-images`, `openai-transcriptions`, `openai-speech`, `openai-embeddings` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/anthropic-dark.svg"><img src="assets/vendors/anthropic.svg" width="20" height="20" alt=""></picture> | Anthropic | `anthropic` | `ANTHROPIC_API_KEY` | `anthropic-messages` |
| <img src="assets/vendors/gemini.svg" width="20" height="20" alt=""> | Gemini | `gemini` | `GEMINI_API_KEY` or `GOOGLE_API_KEY` | `gemini-generate`, `openai-chat`, `gemini-live`, `gemini-images`, `gemini-veo`, `openai-embeddings` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/respan-dark.svg"><img src="assets/vendors/respan.svg" width="20" height="20" alt=""></picture> | Respan | `respan` | `RESPAN_API_KEY` | `openai-chat`, `openai-responses`, `anthropic-messages`, `gemini-generate`, `decisions` |
| <img src="assets/vendors/typesafe.png" width="20" height="20" alt=""> | TypeSafe | `typesafe` | `TYPESAFE_API_KEY` | `decisions` |

## 🟡 Yellow

| | Vendor | `vendor` | Key | Formats |
|:-:|---|---|---|---|
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/groq-dark.svg"><img src="assets/vendors/groq.svg" width="20" height="20" alt=""></picture> | Groq | `groq` | `GROQ_API_KEY` | `openai-chat`, `openai-transcriptions`, `openai-speech` |
| <img src="assets/vendors/deepseek.svg" width="20" height="20" alt=""> | DeepSeek | `deepseek` | `DEEPSEEK_API_KEY` | `openai-chat` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/openrouter-dark.svg"><img src="assets/vendors/openrouter.svg" width="20" height="20" alt=""></picture> | OpenRouter | `openrouter` | `OPENROUTER_API_KEY` | `openai-chat` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/xai-dark.svg"><img src="assets/vendors/xai.svg" width="20" height="20" alt=""></picture> | xAI | `xai` | `XAI_API_KEY` | `openai-chat`, `openai-images` |
| <img src="assets/vendors/mistral.svg" width="20" height="20" alt=""> | Mistral | `mistral` | `MISTRAL_API_KEY` | `openai-chat`, `openai-embeddings`, `openai-transcriptions`, `openai-speech`, `mistral-realtime` |
| <img src="assets/vendors/together.svg" width="20" height="20" alt=""> | Together | `together` | `TOGETHER_API_KEY` | `openai-chat`, `openai-images`, `openai-embeddings` |
| <img src="assets/vendors/fireworks.svg" width="20" height="20" alt=""> | Fireworks | `fireworks` | `FIREWORKS_API_KEY` | `openai-chat`, `openai-embeddings` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/vendors/ollama-dark.svg"><img src="assets/vendors/ollama.svg" width="20" height="20" alt=""></picture> | Ollama | `ollama` | none; `OLLAMA_HOST` says where | `openai-chat`, `openai-embeddings` |
| <img src="assets/vendors/azure.svg" width="20" height="20" alt=""> | Azure OpenAI | `azure` | `AZURE_OPENAI_API_KEY` | `openai-chat` |
| <img src="assets/vendors/vllm.svg" width="20" height="20" alt=""> | vLLM | `vllm` | none | `openai-chat` |
| <img src="assets/vendors/cloudflare.svg" width="20" height="20" alt=""> | Cloudflare | `cloudflare` | `CLOUDFLARE_API_TOKEN` | `openai-chat`, `openai-embeddings` |
| <img src="assets/vendors/cerebras.svg" width="20" height="20" alt=""> | Cerebras | `cerebras` | `CEREBRAS_API_KEY` | `openai-chat` |
| <img src="assets/vendors/perplexity.svg" width="20" height="20" alt=""> | Perplexity | `perplexity` | `PERPLEXITY_API_KEY` | `openai-responses` |
| <img src="assets/vendors/gemini.svg" width="20" height="20" alt=""> | Google Vertex AI | `vertex` | `GOOGLE_VERTEX_ACCESS_TOKEN` or `CLOUDSDK_AUTH_ACCESS_TOKEN` | `gemini-generate`, `anthropic-messages`, `openai-chat`, `openai-embeddings` |
| <img src="assets/vendors/gemini.svg" width="20" height="20" alt=""> | Vertex AI express mode | `vertex-express` | `GOOGLE_VERTEX_API_KEY` | `gemini-generate` |
| <img src="assets/vendors/deepinfra.svg" width="20" height="20" alt=""> | DeepInfra | `deepinfra` | `DEEPINFRA_API_KEY` | `openai-chat`, `openai-images`, `openai-embeddings`, `openai-speech`, `openai-transcriptions` |

These tables are kept by hand: a vendor added to `ALL` in
[`crates/vendors`](../crates/vendors/README.md), or a format added to a
vendor, is a row or an entry here too.

## Base URLs

A provider's `base_url` is the one its own SDK takes. OpenAI-compatible ones
include the version: `https://api.openai.com/v1`,
`https://api.groq.com/openai/v1`, `https://openrouter.ai/api/v1`,
`https://api.cerebras.ai/v1`, `https://api.deepinfra.com/v1/openai`, and
Gemini's OpenAI-compatible endpoint
`https://generativelanguage.googleapis.com/v1beta/openai` (all
`openai-chat`). Anthropic's does not: `https://api.anthropic.com`. The media
formats (`openai-images`, `openai-transcriptions`, `openai-speech`,
`openai-videos`, `openai-embeddings`) and `openai-responses` take the same
base URL as `openai-chat`. Gemini's own formats, `gemini-generate`
(`generateContent` and `streamGenerateContent`, the model in the path),
`gemini-images` (its image models' `generateContent`) and `gemini-veo`,
take `https://generativelanguage.googleapis.com/v1beta` with
the key in `x-goog-api-key`. Session formats take a `wss://` URL as their
SDK does: `wss://api.openai.com/v1` for `openai-realtime` and
`responses-ws`, `wss://generativelanguage.googleapis.com` for `gemini-live`,
`wss://api.mistral.ai/v1` for `mistral-realtime`: Mistral's realtime
transcription, the key a bearer token and the model at `?model=`, which
transcribes for an OpenAI Realtime or Gemini Live transcription as its own
clients transcribe with theirs.
Perplexity's Agent API speaks `openai-responses` alone, at
`https://api.perplexity.ai/v1`; its models are named `<vendor>/<model>`,
such as `openai/gpt-5.6-sol`.

Vertex AI's formats are under a project's location,
`https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}`
(`https://aiplatform.googleapis.com/..` for the `global` location), each
with a Google access token as the bearer key: `gemini-generate` at
`/publishers/google`; `anthropic-messages` at `/publishers/anthropic`,
Claude's, each call posted to the model's `rawPredict` (or
`streamRawPredict`) with `anthropic_version` in the body and no model;
`openai-chat` at the location itself, posted to
`/endpoints/openapi/chat/completions`, but a Mistral model's to its own
`rawPredict` at `/publishers/mistralai`; and `openai-embeddings` at
`/endpoints/openapi`. Its models are each publisher's list
(`/v1beta1/publishers/{publisher}/models`) of those it serves with no
deployment of one's own: Google's and Anthropic's named as they are, and
sent in their own formats, the others' as `{publisher}/{model}`, such as
`meta/llama-4-maverick-17b-128e-instruct-maas`, in Chat Completions; a
Google model is `google/{model}` there, which the gateway writes for it. A
model whose publisher is also a provider's name, such as `openai`, is
asked for as `vertex/openai/gpt-oss-120b-maas`. The lists say no kind,
limits or prices. The key expires within the hour: the gateway sends the
one it is given, so whoever runs it gives it a fresh one, such as `gcloud
auth print-access-token` prints, in a config file it reloads. Express mode,
`vertex-express`, takes an API key that does not expire, in
`x-goog-api-key`, for `gemini-generate` alone at
`https://aiplatform.googleapis.com/v1/publishers/google`, under no
project; it lists no models.

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
need a `base_url`; `cloudflare` needs `CLOUDFLARE_ACCOUNT_ID`, or one;
`vertex` needs `GOOGLE_CLOUD_PROJECT` and `GOOGLE_CLOUD_LOCATION`, or each
format's URL in `formats`.

How each vendor's quirks are handled, and how to add one, is in
[`crates/vendors`](../crates/vendors/README.md).

---

Logos are each vendor's trademark, shown only to identify it. Most are from
[LobeHub Icons](https://github.com/lobehub/lobe-icons) (MIT).
