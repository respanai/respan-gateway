# Media formats over HTTP

`respan-gateway-media-http` is the [media kinds](../../kinds/media/README.md)
over HTTP.

`openai::{OpenAiImages, OpenAiTranscriptions, OpenAiSpeech, OpenAiVideos,
OpenAiEmbeddings}`, named `openai-images`, `openai-transcriptions`,
`openai-speech`, `openai-videos` and `openai-embeddings`: OpenAI's endpoints, as OpenAI and compatible vendors serve
them. A client's request is a `POST` to a path ending in the endpoint
(`/v1/images/generations` and `/openai/v1/images/generations` alike), or a
video job's by its method and path; a provider's base URL includes the
version, as OpenAI's SDKs take it: `https://api.openai.com/v1`,
`https://api.groq.com/openai/v1`,
`https://generativelanguage.googleapis.com/v1beta/openai`.

| Format | Endpoints | Request | Response |
| --- | --- | --- | --- |
| `openai-images` | `/images/generations`, `/images/edits` | JSON; an edit a form (`image` or `image[]`, `mask`), or JSON naming its images by `image_url` | JSON; with `stream`, `image_generation.*` or `image_edit.*` events |
| `openai-transcriptions` | `/audio/transcriptions`, `/audio/translations` | A form (`file`) | JSON (`json`, `verbose_json`, `diarized_json`) or text (`text`, `srt`, `vtt`); with `stream`, `transcript.text.*` events |
| `openai-speech` | `/audio/speech` | JSON | The audio's bytes; with `stream_format: "sse"`, `speech.audio.*` events |
| `openai-embeddings` | `/embeddings` | JSON; `input` a text, texts, token ids or lists of them, or a list of `{"content": [..]}`, each a list of parts (`text`, `image_url`, `input_audio`, `input_video`, `input_file`), read as typed parts and written back as they were read; any other list of objects is relayed as written | JSON; vectors as `encoding_format` asks, `float` or `base64`, whatever the provider answers; input tokens by modality as `usage.prompt_tokens_details.{text,image,audio,video,file}_tokens` |
| `openai-videos` | `POST /videos`, `GET /videos`, `GET` or `DELETE /videos/{id}`, `GET /videos/{id}/content`, `POST /videos/{id}/remix` | JSON, or a form with an `input_reference` file | A job, a list or a deletion in JSON; content as its bytes |

- A provider is asked to stream as the call's `StreamResponse` says, else
  as the client's request did, if to the same endpoint, else not. `stream`
  and `stream_format` are framing, not part of an `Input`.
- An edit goes to `/images/edits` as a form of its images' bytes, or in JSON
  if one, or the mask, is a URL. An image a client names by file id cannot be
  relayed and is refused. The mask is `openai-images`' native field
  (`mask: {"image_url"}`, a data URL of a form's file): another format drops
  it.
- An answer's image is of its `output_format`, else of the type its bytes
  say, else PNG.
- An image request's `quality`, `output_format` and `user`, and an answer's
  `created`, `revised_prompt` and streamed partial images, are
  `openai-images`' native fields: another format drops them.
- A file's type is its part's, unless that says none or
  `application/octet-stream`, which OpenAI refuses for images; then its
  name's, then its first bytes'.
- Text a provider rendered (`srt`, say) is kept as it came and given back as
  `text/plain`; a client that asked for text a provider gave as JSON gets
  the transcript's text, or SubRip or WebVTT of its segments.
- Audio is relayed chunk by chunk as it comes, the provider read no faster
  than the client takes it. Speech asked with no `response_format` is asked
  for as `mp3`; its raw audio is labelled as its first bytes show (WAV, MP3,
  FLAC, AAC), else as its `response_format` names, and PCM always as asked;
  a video's content likewise, as its variant's: `video/mp4`, `image/webp` for
  a thumbnail, `image/jpeg` for a spritesheet.
- A video's `seconds` is a string in OpenAI's API, as it is written; a
  client may send a number. Its `/videos/{id}` never takes
  `/videos/{id}/content` or `/remix`; an id is escaped in a call's path.
  A remix, a deletion, a page of jobs and a still (`?variant=thumbnail` or
  `spritesheet`) are the format's own operations, relayed as they came, the
  answer too, to an `openai-videos` provider only.
- Usage `{"type": "tokens"}` is `Usage`, `{"type": "duration"}` `Seconds`; a
  usage object's other fields, such as `input_token_details`, stay native.
- A stream that ends before its end event fails `truncated`.
- A client body without its required field (`prompt`, an edit's `image`,
  `file`, `input`) is refused, 400, before any call. A provider's error is
  read leniently, as compatible servers write it, and a client's is written
  in OpenAI's schema, `{"error": {"message", "type", "param", "code"}}`,
  under its status.

Vendors' fields the kind has no form for (Groq's `x_groq`, Together's
`steps`) stay native, so a relay in the same format keeps them.

A vendor's departures from one of these formats are its dialect of it,
laid over the format (`respan_gateway_http::dialect::Dialected`), so a
client of the spec is still relayed to it. A dialect or a host reaches a
format's native only through that format's own functions: `fields`, and,
for each but Gemini's images, `with_fields`.

`gemini::GeminiImages`, named `gemini-images`: Gemini's image models,
such as Nano Banana, through `generateContent`, to clients and providers,
at `https://generativelanguage.googleapis.com/v1beta` with the key in
`x-goog-api-key`.

- A call is `POST /models/{model}:generateContent`, one user turn of the
  images to edit (`inlineData`, or `fileData` for one named by URL) and the
  prompt. Its `size` is the `imageConfig` of the nearest aspect ratio
  Gemini draws (1:1, 2:3, 3:2, 3:4, 4:3, 4:5, 5:4, 9:16, 16:9, 21:9;
  within 5%, else 400), with an `imageSize` of `2K` or `4K` above 1K; its
  count `candidateCount`, which Gemini's image models refuse above 1. As
  Gemini edits by prompt alone, a mask is dropped.
- An answer's images are its parts' `inlineData` (or `fileData`), not its
  thoughts'; its text is dropped; its usage is `usageMetadata`'s. An
  answer with no image is refused, 400 `no_image`, saying why: the
  `finishReason` or block reason, and what the model said.
- A client of it, served by any provider of the kind: a `POST` to
  `.../models/{model}:generateContent` or `:streamGenerateContent`, one
  user turn of text and images; a history is refused, 400.
  `generationConfig`'s `candidateCount` is the count, its `imageConfig`
  of one of those aspect ratios (and `1K`, `2K` or `4K`) a size, about
  Gemini's own (`1248x832` for 3:2); `responseModalities` is dropped, as
  the answer is images; the rest of it, and the body's other fields, are
  this format's native fields. The answer is one candidate of the images
  and the usage; a stream is that response once, as SSE with `alt=sse`,
  else in a JSON array.
- The gateway serves it beside `gemini-generate`, at the same paths: a
  call is an image's if routing sends its model, of the image kind, to a
  provider that cannot take the call as `generateContent`, else a chat's.
  So a Gemini client draws with OpenAI's models, and Gemini's own image
  models are asked as they were, as chats, which keep a conversation.

`gemini::GeminiVeo`, named `gemini-veo`: Google's Veo as the Gemini API serves it, to clients and providers,
at `https://generativelanguage.googleapis.com/v1beta` with the key in
`x-goog-api-key`.

- A `Create` is `POST /models/{model}:predictLongRunning`, which needs a
  model: its `size` an `aspectRatio` (16:9 or 9:16, the only ones Veo makes,
  else 400) and a `resolution` (`1080p` from 1080 pixels on the short side,
  else `720p`), its `seconds` `durationSeconds` (Veo takes 4, 6 or 8, and
  refuses others itself), its reference image the instance's base64 `image`,
  and its `gemini-veo` native fields (`negativePrompt`, `personGeneration`)
  parameters.
- A job is Veo's long-running operation, its id `veo_` and the operation's
  name, `models/{model}/operations/{op}`, in unpadded base64url
  (`GeminiVeo::job_id`, `GeminiVeo::operation`), as a path segment holds
  it; another id is not found, 404. An operation not done is `in_progress`,
  done with videos `completed`, done with an error, or with none as when
  Veo's filters took them all, `failed`.
- A Gemini Omni model (`gemini-omni-*`), which replaced Veo's previews
  (shut down 2026-10-22), is created by `POST /interactions`: its prompt,
  after its reference image if any, the `input`, its `size` the
  `response_format`'s `aspect_ratio` and `resolution` (with no size, its
  native `aspectRatio` and `resolution`), its `seconds` the `duration`
  (`"8s"`); it has no place for Veo's other parameters (`negativePrompt`,
  `personGeneration`), so those are dropped. Its video is a file of the Files API, asked at a URI, and the
  file is its job: its name, `files/{id}`, spelled as an operation's,
  retrieved by `GET /files/{id}`, `in_progress` until the file is `ACTIVE`,
  `completed` then with the file's download URI as its video, `failed` if
  `FAILED`. An answer with no video is the provider's fault, 502. The
  interaction's `usage`, its tokens by modality, is the created job's
  native `usage`; a file reports none.
- Content is a `GET` of the URL its job names at the provider's base, its
  path after the version, so the key goes to no other host. The Gemini API
  may redirect it to storage: the transport follows only if told to
  (`HttpTransport::follow_redirects`, which says which locations), else the
  answer is a 502. A redirect to another origin leaves the key behind.
- Another format's operation, such as OpenAI's remix, delete, list or
  thumbnail, is refused, 400 `unsupported`, before any call.
- What else Veo answers beyond the kind is dropped rather than kept native.
- A client of it, served by any provider of the kind: it creates with
  `POST .../models/{model}:predictLongRunning` (one instance; its prompt,
  its base64 `image`; `aspectRatio` and `resolution` read as a size,
  `durationSeconds` as seconds, its other parameters kept as `gemini-veo`
  native fields). A job is answered as an operation named
  `models/{model}/operations/{job id}`, polled at `GET .../{name}`, so a
  follow-up reaches the job's provider by its id; done, it names its video
  at the gateway, `GET .../files/{job id}:download?alt=media`, never the
  provider's own URL, which would need its key. A failure is the
  operation's `error`, or, filtered, a response of
  `raiMediaFilteredReasons`; a refusal is Google's `{"error": {code,
  message, status}}`. Veo reports no progress: an operation is done or not.
  Polling a Gemini Omni operation by its qualified name returns that same
  name, including when the provider reports the video through the Files API.

`openrouter::OpenRouterVideos`, named `openrouter-videos`: OpenRouter's video
generation, as a provider only, its key a bearer token. Its job ids start
`gen-vid-`, which `video::VideoJobs` routes a follow-up by.

- `POST /videos` makes a job: the kind's `seconds` go as `duration`, its
  `size` as it is, its reference image, a URL or a data URL of its bytes, as
  the first of the `frame_images`, and its `openrouter-videos` native fields
  (`resolution`, `aspect_ratio`, `generate_audio`, `seed`) as they are.
- `GET /videos/{id}` polls it: `pending` is `queued`, `in_progress`,
  `completed` and `failed` are the kind's, its error a string; `cancelled`
  and `expired` are `failed`, with that as the error unless one is said; its
  `unsigned_urls` are the job's videos; its `usage`, the cost of a job
  done, is the job's native `usage`; the signed `urls` of its video are
  dropped.
  It reports no progress, model, size or length. `GET /videos/{id}/content`
  is the video's bytes, relayed as they come. It answers a 404 for the
  content of a job not finished, as for a job it never made, so
  `video::LocateContent` retrieves the job first and refuses one not
  finished, 400 `video_not_ready`.
- Another format's operation, such as OpenAI's remix, delete, list or
  thumbnail, is refused, 400 `unsupported`, before any call.

`together::TogetherVideos`, named `together-videos`: Together's video
generation, as a provider only, its key a bearer token, its base URL the
API's host with no version (`https://api.together.xyz`). Its job ids are
Together's after `together_`, which `video::VideoJobs` routes a follow-up by.

- `POST /v2/videos` makes a job: the kind's `seconds` go as they are, as
  text, its `size` as `width` and `height`, its reference image, a URL or a
  data URL of its bytes, as the first of the media's `frame_images`, and its
  `together-videos` native fields (`resolution`, `ratio`, `generate_audio`,
  `negative_prompt`, `seed`) as they are.
- `GET /v2/videos/{id}` polls it: `in_progress`, `completed` and `failed`
  are the kind's, its error `{code, message}`; its model, size and length
  are the job's; the `video_url` of its `outputs` is its video, and their
  `cost`, what it charged, the job's native `cost`.
- It serves the video only at that URL, so content is refused, 400
  `unsupported`, before any call, as is another format's operation, such as
  OpenAI's remix, delete, list or thumbnail.

`empiriolabs::EmpirioLabsVideos`, named `empiriolabs-videos`: EmpirioLabs
AI's video generation, as a provider only, its key a bearer token, its base
URL the API's host with no version (`https://api.empiriolabs.ai`). Its job
ids are the provider's after `empiriolabs_`, which `video::VideoJobs` routes
a follow-up by.

- `POST /v1/videos/generations` makes a job: the kind's `seconds` go as its
  `duration`, its reference image, a URL or a data URL of its bytes, as its
  `image`, its `size` as a `resolution` (`<short side>p`, for 360, 480,
  540, 720, 768 or 1080) and an `aspect_ratio` (16:9, 9:16, 1:1, 4:3, 3:4,
  3:2, 2:3 or 21:9, the nearest within 5%), and its `empiriolabs-videos`
  native fields (`generate_audio`, `mode`, and a `resolution` or
  `aspect_ratio` the `size` does not say) as they are. Any other `size` is
  refused, 400, before any call; a model that lacks the
  resolution answers with its own error.
- `GET /v1/jobs/{id}` polls it: `processing`, `completed` and `failed` are
  the kind's, `cancelled` its own, its error `{code, message}`; its model is
  the job's; the `url`s of its `result`'s `data` are its videos, their
  `duration_seconds` its length, and the `usage`'s `cost_usd`, what it
  charged, the job's native `cost_usd`.
- It serves a video only at those URLs, so content is refused, 400
  `unsupported`, before any call, as is another format's operation, such as
  OpenAI's remix, delete, list or thumbnail.

`minimax::MiniMaxVideos`, named `minimax-videos`: MiniMax's video
generation (its `/v2` API, the H3 models), as a provider only, its key a
bearer token, its base URL the API's host with no version
(`https://api.minimax.io`). Its job ids are MiniMax's after `minimax_`,
which `video::VideoJobs` routes a follow-up by.

- `POST /v2/video_generation` makes a job: the kind's `seconds` go as its
  `duration`, its prompt as the `text` of its `content`, its reference image
  as the `first_frame` image there, its `size` as a `ratio` and a
  `resolution` (`480P` or `768P` by its short side, the ratio the one of
  MiniMax's nearest, within 5%: 864x480 is 16:9, 480P), and its
  `minimax-videos` native fields (`resolution`, `ratio`, `generate_audio`,
  `extra`) as they are, where the `size` has not said them. Any other
  `size` is refused, 400, before any call.
- `GET /v2/query/video_generation/{id}` polls it: `queued`, `running`,
  `succeeded` and `failed` are the kind's, its error `{code, message}`;
  any other state, such as `cancelled`, is the format's own. The
  `content.url` of its task, a time-limited URL, is its video, and its
  `usage`, what it bills, the job's native `usage`.
- It serves the video only at that URL, so content is refused, 400
  `unsupported`, before any call, as is another format's operation, such as
  OpenAI's remix, delete, list or thumbnail.

`gemini::GeminiEmbeddings`, named `gemini-embeddings`: Gemini's embedding
models on `POST /models/{model}:batchEmbedContents`, its key in
`x-goog-api-key`.

- Every text is one request of the batch, one part each; `dimensions` is
  each request's `outputDimensionality`.
- An item of parts is one request of those parts, one joint vector. Media
  goes inline from its bytes (`inlineData`), or from its URL for Gemini to
  fetch (`fileData`) with its type, which may be a family's wildcard such as
  `image/*`; a URL of no type at all is refused, 400, before the call. An
  item's native fields, such as `taskType`, go in its request.
- Usage is `usageMetadata`'s `promptTokenCount`, which Gemini reports for
  some models only (not `gemini-embedding-001`); its OpenAI-compatible
  endpoint reports none. `promptTokenDetails` are the input tokens of each
  modality (text, image, audio, video, document).
- Token ids are refused, 400, before the call; other fields of another
  format, such as an end user's id, are dropped.
- A client speaks it too, of any provider of the kind: a `POST` to
  `.../models/{model}:embedContent`, one request, or
  `:batchEmbedContents`, its `requests`. A request's `content` is an item:
  a text if it is one `text` part and says no more, else parts (`text`,
  `inlineData`, `fileData`). An `outputDimensionality` every request says
  is the `dimensions`; a request's other fields, such as `taskType` and
  `title`, and sizes that differ between requests, stay its item's, for a
  Gemini provider only: another provider is not sent them. The answer is
  `embedding`, or `embeddings`, and the usage as `usageMetadata`.

`bedrock::BedrockEmbeddings`, named `bedrock-embeddings`: Amazon Bedrock's
embedding models on InvokeModel, `POST /model/{model}/invoke`, as a provider
only, its key a Bedrock API key as a bearer token.

- Cohere Embed (`cohere.embed-*`) embeds every text in one call as a
  `search_document`: a client's `input_type` is another format's native
  field, so it is dropped; v4 takes `dimensions` as `output_dimension`.
- Titan Text (`amazon.titan-embed-text-*`) and Titan Multimodal's texts
  (`amazon.titan-embed-image-v1`) embed one text per call, so a request of
  several is refused, 400; v2 and Multimodal take `dimensions`.
- Usage is the input tokens Bedrock counts: Titan's `inputTextTokenCount`,
  else the `x-amzn-bedrock-input-token-count` header.
- Token ids, parts and a `dimensions` the model takes none of are refused,
  400, before the call; other fields of another format, such as an end
  user's id, are dropped.

`bedrock::BedrockImages`, named `bedrock-images`: Amazon Bedrock's Stability AI
models on InvokeModel, `POST /model/{model}/invoke`, as a provider only, its
key a Bedrock API key as a bearer token.

- Stable Image Core and Ultra and Stable Diffusion 3.5 Large
  (`stability.*`) draw from a prompt, with the nearest `aspect_ratio` of the
  request's `size` (within 5%, else 400; `auto` sends none). Stable
  Diffusion 3.5 Large (with `mode`) and Ultra take an edit as an
  image-to-image, Core takes none.
- The Image Services (`stability.stable-*`: inpaint, outpaint, erase object,
  search and replace, search and recolor, remove background, style transfer,
  style guide, control sketch and structure, and the fast, creative and
  conservative upscales) edit the first image, sent as `image` (style
  transfer's `init_image`, with the second as `style_image`), and take no
  generation. A mask, a `mask` native field as OpenAI's edits have it, goes as
  `mask` to inpaint and erase, and is refused by the rest: it is Stability's,
  white where it paints. An edit's `size` is dropped, but for style guide's
  `aspect_ratio`; the prompt is dropped where a service takes none (erase,
  remove background, fast upscale).
- Stability's own fields are native fields of the request, sent as they are,
  a form's numbers as numbers: `output_format`, `seed`, `negative_prompt`,
  `style_preset`, `strength`, `search_prompt`, `select_prompt`, `grow_mask`,
  `left`, `right`, `up`, `down`, `creativity`, `control_strength`,
  `fidelity`, `composition_fidelity`, `style_strength` and `change_strength`.
  Other fields of another format, such as a `quality` or `user`, are dropped.
- An answer's `images` are the images, each of the type its bytes say; a
  `finish_reasons` entry that is not null is a fault, not an image: a filter's
  is a 400 coded `content_filter`, `Inference error` a provider fault.
  Bedrock reports no tokens for these models, so there is no usage; a call
  makes one image.
- More than one image (`n`), images or a mask by URL, an image to a model
  that takes none (or none to a service that edits), a `size` of no aspect
  ratio Stability draws, and a `response_format` but `b64_json` are refused,
  400, before the call.

`dashscope::{DashScopeSpeech, DashScopeTranscriptions}`, named
`dashscope-speech` and `dashscope-transcriptions`: Alibaba Cloud Model
Studio's audio models on DashScope's own API, as a provider only. Its
OpenAI-compatible endpoint serves chat alone. A provider's base URL is its
region's API, with the version, `https://dashscope-intl.aliyuncs.com/api/v1`,
its key a bearer token.

- Speech (`qwen-audio-3.*-tts-*`) is a `POST` to
  `/services/audio/tts/SpeechSynthesizer`, asked to stream
  (`X-DashScope-SSE: enable`), as its whole answer names the audio at a URL.
  The text, voice, `instructions` (as `instruction`), speed (as `rate`) and
  audio format, `mp3` unless asked, are its `input`. Each event's base64
  `output.audio.data` is the next chunk of audio; the last event's
  (`finish_reason: "stop"`) usage is its tokens, the rest of it, such as
  the `characters` some models bill, a native `{"usage": {..}}`. A request
  without a voice, or of a format but `mp3`, `wav`, `pcm` and `opus`, is
  refused, 400, before the call; a parameter DashScope refuses, in the
  stream or as a refusal written as an event, is a 400.
- Recognition of a whole file (`qwen-audio-3.*-asr-flash`, `qwen3-asr-flash`,
  `fun-asr-flash-*`) is a `POST` to
  `/services/aigc/multimodal-generation/generation`, a user message of the
  audio as a data URI of its bytes, or its URL, with its `format` (from its
  type) and the language as both `language_hints` and `asr_options.language`
  in `parameters`, as Qwen3-ASR and the other models each read one. The answer's
  `output.text`, or its first choice's text, is the transcript; its usage is
  the seconds billed and its tokens, if any. A translation, a prompt and a
  temperature are refused, 400, before the call.

`minimax::MiniMaxSpeech`, named `minimax-speech`: MiniMax's speech synthesis
on its international API, as a provider only. A provider's base URL is
`https://api.minimax.io/v1`, its key a bearer token.

- A `POST` to `/t2a_v2`, the audio asked in hex, never as a URL. The text,
  model, voice (`voice_setting.voice_id`), speed (`voice_setting.speed`) and
  audio format (`audio_setting.format`, `mp3` unless asked) are its body;
  native `voice_setting` and `audio_setting` fields (`vol`, `pitch`,
  `sample_rate`) join them, and other native fields (`language_boost`) sit at
  the top. `pcm` and `opus` are asked at 24 kHz unless a `sample_rate` is native (MiniMax's 32 kHz is refused for Opus). A
  request without a voice, with `instructions`, or of a format but `mp3`,
  `pcm`, `flac`, `wav` and `opus`, is refused, 400, before the call.
- `mp3`, `pcm` and `opus` are streamed: each event's hex `data.audio` is the
  next chunk of audio, and the last (`data.status: 2`), which MiniMax makes
  repeat the whole audio, is asked not to (`exclude_aggregated_audio`). `wav`
  (whose streamed header has no length) and `flac` are asked whole. The
  `extra_info` (the characters billed, the audio's length) is a native
  `{"extra_info": {..}}`.
- MiniMax answers an error as a success whose `base_resp.status_code` is not
  0: it is a 400 for a parameter or voice refused (2013, 1042, 2054), 401 for
  a key (1004, 2049), 402 for a balance (1008), 429 for a limit (1002, 1039),
  else a 502.
