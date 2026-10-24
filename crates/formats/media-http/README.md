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
| `openai-embeddings` | `/embeddings` | JSON; `input` a text, texts, token ids or lists of them, or a list of parts (`{"type": ..}` objects), relayed as written | JSON; vectors as `encoding_format` asks, `float` or `base64`, whatever the provider answers |
| `openai-videos` | `POST /videos`, `GET /videos`, `GET` or `DELETE /videos/{id}`, `GET /videos/{id}/content`, `POST /videos/{id}/remix` | JSON, or a form with an `input_reference` file | A job, a list or a deletion in JSON; content as its bytes |

- A provider is asked to stream as the call's `StreamResponse` says, else
  as the client's request did, if to the same endpoint, else not. `stream`
  and `stream_format` are framing, not part of an `Input`.
- An edit goes to `/images/edits` as a form of its images' bytes, or in JSON
  if one is a URL. An image a client names by file id cannot be relayed and
  is refused.
- A file's type is its part's, unless that says none or
  `application/octet-stream`, which OpenAI refuses for images; then its
  name's, then its first bytes'.
- Text a provider rendered (`srt`, say) is kept as it came and given back as
  `text/plain`; a client that asked for text a provider gave as JSON gets
  the transcript's text, or SubRip or WebVTT of its segments.
- Audio is relayed chunk by chunk as it comes, the provider read no faster
  than the client takes it, as the content type its `response_format` names;
  a video's content likewise, as its variant's: `video/mp4`, `image/webp` for
  a thumbnail, `image/jpeg` for a spritesheet.
- A video's `seconds` is a string in OpenAI's API, as it is written; a
  client may send a number. Its `/videos/{id}` never takes
  `/videos/{id}/content` or `/remix`; an id is escaped in a call's path.
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
client of the spec is still relayed to it.

`gemini::GeminiImages`, named `gemini-images`: Gemini's image models,
such as Nano Banana, through `generateContent`, to clients and providers,
at `https://generativelanguage.googleapis.com/v1beta` with the key in
`x-goog-api-key`.

- A call is `POST /models/{model}:generateContent`, one user turn of the
  images to edit (`inlineData`, or `fileData` for one named by URL) and the
  prompt. Its `size` is the `imageConfig` of the nearest aspect ratio
  Gemini draws (1:1, 2:3, 3:2, 3:4, 4:3, 4:5, 5:4, 9:16, 16:9, 21:9;
  within 5%, else 400), with an `imageSize` of `2K` or `4K` above 1K; its
  count `candidateCount`, which Gemini's image models refuse above 1. A
  mask is refused, 400, as Gemini edits by prompt alone; quality, file
  format and user are dropped.
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
- Content is a `GET` of the URL its job names at the provider's base, its
  path after the version, so the key goes to no other host. The Gemini API
  may redirect it to storage: the transport follows only if told to
  (`HttpTransport::follow_redirects`, which says which locations), else the
  answer is a 502. A redirect to another origin leaves the key behind.
- Veo cannot remix, delete or list videos, nor make a thumbnail or
  spritesheet: those are refused, 400 `unsupported`, before any call.
- What Veo answers beyond the kind is dropped rather than kept native.
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

`bedrock::BedrockEmbeddings`, named `bedrock-embeddings`: Amazon Bedrock's
InvokeModel for embeddings, a provider of the embedding kind, at its
region's runtime, `https://bedrock-runtime.{region}.amazonaws.com`, its key
a Bedrock API key as a bearer token.

- A call is `POST /model/{model}/invoke`, the model escaped in the path; an
  inference profile's id (`us.cohere.embed-v4:0`) is its model as it is.
- Cohere Embed (`cohere.embed-*`) embeds every text in one call:
  `{texts, input_type, embedding_types: ["float"]}`, `input_type` the
  client's (an `openai-embeddings` field) or `search_document`; an item of
  parts is one input, v3's one image (`images`), v4's texts and images
  together (`inputs`); v4 takes `dimensions` as `output_dimension`.
- The others embed one input per call, so a request of several is as many
  calls, a few at a time, one attempt of the provider's: its vectors are
  the inputs' in order, its usage theirs together. Titan Text
  (`amazon.titan-embed-text-*`) embeds a text, v2 with `dimensions`;
  Titan Multimodal (`amazon.titan-embed-image-v1`) a text, an image or the
  two, `dimensions` as `embeddingConfig.outputEmbeddingLength`; Nova
  Multimodal Embeddings (`amazon.nova-2-multimodal-embeddings-v1:0`) each
  text and each part, a text, image, audio or video, `dimensions` as
  `embeddingDimension` and the client's `input_type` as
  `embeddingPurpose` (`GENERIC_INDEX` unless it says); Marengo Embed 3.0
  (`twelvelabs.marengo-embed-3-0-v1:0`) a text, an image or the two.
- A part's media is a base64 data URL, `{"type": "image", "url":
  "data:image/png;base64,..."}`: Bedrock takes media as bytes.
- Vectors are asked for as floats; the client's `encoding_format` is the
  client edge's. Usage is the input tokens Bedrock counts: Titan's
  `inputTextTokenCount`, else the `x-amzn-bedrock-input-token-count`
  header.
- Token ids, media by URL, parts a model does not read, a `dimensions` it
  does not take and fields of another format are refused, 400, before any
  call.

`bedrock::BedrockImages`, named `bedrock-images`: Amazon Bedrock's
InvokeModel for Stability AI images, a provider of the image kind, at the
same runtime and key as `bedrock-embeddings`.

- A call is `POST /model/{model}/invoke`, the model (or inference profile,
  `us.stability.stable-image-inpaint-v1:0`) escaped in the path. One image
  per call: a request for more is refused, 400.
- A generation (`stability.sd3-5-large-v1:0`,
  `stability.stable-image-core-v1:1`, `stability.stable-image-ultra-v1:1`)
  is `{prompt, aspect_ratio, output_format}`, the aspect ratio the nearest
  Stability takes to the request's `size`; Stable Diffusion 3.5 (with
  `mode`) and Ultra take an edit as image-to-image (`image`); the style
  guide draws from the prompt, with `size` too, in its image's style.
  Stability's models are offered in a few regions only (`us-west-2` for
  most), which is not checked here.
- An Image Service (inpaint, outpaint, erase, search and replace, search
  and recolor, remove background, style transfer, style guide, control
  sketch and structure, the fast, creative and conservative upscales) takes
  the edit's first image as base64 `image` (style transfer: `init_image`,
  with the second as `style_image`), the inpaint and erase services its
  `mask`, and a prompt but for erase, remove background and the fast
  upscale. Images are bytes, not URLs.
- The request's native fields (`seed`, `negative_prompt`, `search_prompt`,
  `select_prompt`, `grow_mask`, `control_strength`, `fidelity`,
  `creativity`, `left`/`right`/`up`/`down`, ...) are the service's own and
  pass through, a form's numbers as numbers.
- An answer is `{images, finish_reasons, seeds}`: an image per entry of
  `images`, its type read from its bytes. A `finish_reasons` entry that is
  not null is a fault: a content filter's is a 400 coded `content_filter`,
  another a provider fault. Bedrock reports no tokens, so the images are
  the usage.
