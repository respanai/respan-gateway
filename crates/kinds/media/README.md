# Media kinds

`respan-gateway-media` is the media kinds. Each operation is a kind of its
own, an `Input` in and an `Output` out, in a module of its name. Their
formats over HTTP are
[`respan-gateway-media-http`](../../formats/media-http/README.md)'s.

| Module | Kind | `Input` | `Output` events |
| --- | --- | --- | --- |
| `image` | Image generation and edits | `prompt`, source `images` and `mask` (an edit has images), `count`, `size`, `quality`, `output_format` | `Start`, `Partial` previews, `Image`, `Usage` |
| `transcription` | Speech to text, or to English (`Task`) | `audio`, `task`, `language`, `prompt`, `temperature` | `Delta`, `Transcript`, `Rendered`, `Usage`, `Seconds` |
| `speech` | Text to speech | `text`, `voice`, `instructions`, `speed`, `audio_format` | `Audio` chunks, `Usage` |
| `embedding` | Embeddings of texts, images and videos | `items` (`Text`, `Tokens` or `Parts`), `dimensions`, `user` | `Start`, `Embedding` vectors, `Usage` |
| `video` | Video generation, a job | an operation: `Create` (`model`, `prompt`, `seconds`, `size`, a `reference` image), `Remix`, `Retrieve`, `Content` (a `Variant`), `Delete`, `List` | `Job`, `Jobs`, `Content` chunks, `Deleted` |

Every `Input` has an optional `model`, read by `model()` and rewritten by
`set_model()`, as a router does; a video's is a `Create`'s alone. Media are the
conversation kind's `Media` (a MIME type, bytes or a URL, a name) and token
usage its `Usage`; a kind has a type of its own only where neither says it:
a `Transcript` (`text`, `language`, `duration`, `segments`, `words`), and
`Seconds` of audio a provider bills, as Whisper does. Every `Output` also has
`Native`, an event the kind has no form for.

What a kind has no form for stays as the format that decoded it has it, in
optional `NativeFields` on a value or an `Output::Native`; only that format
writes it, and another refuses it. Values are plain data: they keep nothing
of the bytes they were decoded from, and a response is always written again
from its events. A stream and a whole body decode into the same events, so
a client gets the framing it asked for whatever the provider's was.

The crate is the kinds' types alone, depending only on
`respan-gateway-core`, the conversation kind's types, `bytes` and
`serde_json`.

## Video jobs

A video is a job: created, polled until done, then downloaded, each a
request of its own, hours apart. A job's `id` is the provider's, opaque to
the gateway: a client gives it back as a path segment, and a follow-up
(`Retrieve`, `Content`, `Remix`, `Delete`, read by `Input::job_id()`) must
reach the provider that made the job. Nothing in the request says which but
the id's spelling: OpenAI Videos' ids start `video_` (OpenAI's own API was
removed on 2026-09-24; a compatible provider's), Veo's `veo_`, Veo on
Vertex AI's `vertex_veo_`.
`video::VideoJobs`, registered after the router, sends each follow-up (and
a `List` after one) to its job's provider by that spelling. Providers that
spell ids alike, such as two deployments of one vendor, have their ids
wrapped on the way out as `<provider>~<id>`, and unwrapped before the call.

Some providers serve a finished job's content only at the URLs the job
names (`Job::videos`), as Veo does: `video::LocateContent`, given those
providers' names and registered after the router, retrieves the job before
a `Content` request goes on, and sets its `url`. Content not ready is
refused, 400 `video_not_ready`. A provider that holds the content in the
finished job (Veo on Vertex AI, as base64) needs no locating: it serves a
`Content` request by retrieving the job again.
