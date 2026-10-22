# Realtime over WebSocket

`respan-gateway-realtime-ws` is the [realtime kind](../../kinds/realtime/README.md)
over a WebSocket session ([`respan-gateway-ws`](../../wires/ws/README.md)),
a format per spec:

- `OpenAiRealtime`, `openai-realtime`: at `/realtime` after
  `wss://api.openai.com/v1`, in the subprotocol `realtime` its browser
  clients offer.
- `GeminiLive`, `gemini-live`: at
  `/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent`
  after `wss://generativelanguage.googleapis.com`. Its server sends JSON in
  binary frames, which its transport takes as text.
- `MistralRealtime`, `mistral-realtime`: Mistral's realtime transcription, at
  `/audio/transcriptions/realtime` after `wss://api.mistral.ai/v1`.
- `ResponsesWs`, `responses-ws`: OpenAI's Responses over WebSocket, at
  `/responses` after `wss://api.openai.com/v1`. It is not realtime: it
  streams Responses over a socket, and uses the kind's types only as native
  events.

Each is served at a path ending in its own, but not in another's that ends
in it (Mistral's in OpenAI's `/realtime`), and sends to it;
`format.transport(base_url)` is a `WsTransport` to its provider, set as its
frames need. The key is the transport's (`with_headers`: OpenAI's and
Mistral's `Authorization: Bearer`, Gemini's `x-goog-api-key`) or the
client's. A format that speaks a subprotocol offers it to the provider
beside the client's own offers, and gives it back to a client that offered
it. They share their session's handshake, and differ in their spec and
their codec.

`OpenAiRealtime` decodes each frame into the kind's events: a session's
configuration, audio (its bytes, in the session's format), messages, tools'
calls and answers, a response's start, content, usage and end, errors.
What the kind has no place for stays OpenAI's: an event's other fields in
its `native`, as the frame has them, and events of no form in the kind,
such as its items and content parts, as `Output::Native`. A field is taken
as the kind's only where it writes back as it came, so a relay to OpenAI
keeps what every event says, not its exact bytes (audio is base64 anew,
padded).

`GeminiLive` does the same for the Live API's messages: a client's `setup`
(first and once, its model without `models/`; a message of more than one
field closes the session 1007), `clientContent` turns, `realtimeInput`
audio and text, `activityEnd`, `toolResponse`; the server's
`setupComplete`, model turns of audio and text, transcripts, `toolCall`,
`interrupted`, `voiceActivity` (its `type` and `audioOffset`),
`turnComplete` and `usageMetadata` (each turn's own counts). A
message that says several things is several events: what it says beyond
them stays on the first its client writes, so it is written back as it
came, unless its events are written as several messages, such as a turn
of two parts. Messages and tools' answers are held until what answers
them, so `clientContent` and `toolResponse` are written whole.
`activityStart`, `audioStreamEnd`, `goAway`, `toolCallCancellation` and
`sessionResumptionUpdate` stay native.

`OpenAiRealtime` and `GeminiLive` write each other's events too: see
[Translation](#translation). `ResponsesWs` decodes every frame to the
kind's `Input::Native` or `Output::Native`, tagged with its name, and
writes no other format's.

Plugins filter events with `cx.filter(|e: &mut Output| ..)` (client edge)
or `px.filter(..)` (provider edge), and drop one with a `Filter` that does
not push it. A handshake one refuses is answered with the fault's error
code, never its message.

## Translation

The target format's writer converts audio its spec cannot carry: G.711 is
decoded for Gemini, which takes PCM at any rate it is told; Gemini's
clients' audio is resampled to OpenAI's 24 kHz; Gemini's 24 kHz audio is
encoded as the G.711 an OpenAI client asked for. A response from Gemini
reaches an OpenAI client with the items, content parts and ids OpenAI
would send, its usage in `response.done`.

What one spec has and the other has not is emulated, dropped or refused:

- To Gemini: the session is created before its setup; an update that
  changes nothing is acknowledged, one that changes anything closes the
  session 1008 (Gemini fixes its setup); `response.cancel` ends the
  response at once and drops the rest of its turn; with manual turns the
  user's audio opens an activity and its commit ends it. The usage of a
  response that ended in calls or an interruption, which Gemini reports
  after, at its turn's end, is in the next `response.done`.
  `conversation.item.truncate` is dropped, as Gemini keeps only what it
  sent; so are ids, tracing, noise reduction, transcription settings and
  VAD tuning. A message is taken only as the user's text answered at once;
  history, item edits, response overrides, a commit under server VAD, a
  tool choice other than `auto` and `create_response: false` are refused.
  A transcription session (`type: "transcription"`, the kind's
  `Purpose::Transcription`), which the Live API answers all the same, has
  its answers dropped: the user's transcript streams, and is whole at an
  answer's end, the user's next turn, or a transcriber's end of its
  generation.
- To OpenAI: Gemini's client routes by `?model=` on its URL (OpenAI takes
  its model at connect), and its `activityStart` while the model speaks is
  `response.cancel` (one that reaches OpenAI as the response ends is refused
  there for want of a response, and that refusal is dropped, not a close);
  its tools' schemas name their types as JSON Schema
  does (`object` for `OBJECT`); a response that calls tools, or is
  interrupted, ends its turn with `turnComplete`, as the Live API's do;
  context compression, VAD sensitivities and
  `audioStreamEnd` are dropped; sampling, resumption, proactivity, Google
  Search, non-blocking functions and video are refused. An error closes a
  Gemini client, which knows a refusal only as a close: 1007 if its cause
  is `Invalid`, and none if `NothingToCancel`.

`MistralRealtime` transcribes and says nothing else, so it is the provider of
an OpenAI Realtime transcription session (`type: "transcription"`), and a
conversation (`type: "realtime"`) is refused. It has no voice detection: its
transcript streams as the audio comes, and is whole at a commit. It decodes
`session.created` and `session.updated` (a transcription, its model and the
audio format, PCM 16-bit or G.711),
`transcription.text.delta` as an input transcript's delta,
`transcription.done` as its whole text with the session's usage (prompt tokens
as audio input tokens, completion tokens as output), and `error`. Its
`transcription.language` and `transcription.segment` stay native, so an
OpenAI client is not told of them. It writes a session's audio format
(`session.update`), the user's audio in appends of at most 256 KiB
(`input_audio.append`, converted to the session's format, 16 kHz until a
session says another), a commit as `input_audio.flush`, and, when the
client's events end, `input_audio.end`, which makes it send the whole
transcript. Turn settings (server VAD or manual), ids and transcription
settings are dropped, as the text is the same; a message, a tool's answer, a
response to make or cancel, instructions, a voice, tools, an output modality
and a change of model are refused. Its model is taken at connect, as
OpenAI's is. Its own `target_streaming_delay_ms` is a native field of a
session (`session.target_streaming_delay_ms`) that a plugin sets, and
another format's provider may drop.

Its client's session is a transcription from its connect, of the model at
its `?model=`, its turns the client's: an append is the user's audio (16 kHz
PCM until its session says another format), and a flush or the end of its
audio a commit, which makes the transcript whole. It is told the session
(`session.created` and `session.updated`: the model and audio format), the
transcript's deltas and its end, with the usage reported since the last in
`usage`, and errors with their code; what tells of an answer, which a
transcription has none of, is not written. So a Mistral client transcribes
with OpenAI's or Gemini's models, and a relay to Mistral says what it said.

A refused input is `bad_request`, closing the client 1008; a refused
output `upstream`. Another format's events and fields a provider sends are
dropped at the client, but an event its format says tells of what the model
said (OpenAI's MCP events) is refused.

Which of a format's natives another may drop is for that format alone to
say, of its own fields and events (`Harmless`, in each spec's module): no
spec names another's. A writer drops another format's native where its
format says so and refuses it where not; a format that says nothing has its
native inputs refused and its native outputs dropped. An error says its
cause as any format reads it (`Cause`: `Invalid`, `NothingToCancel`,
`Provider`), set by the format that read it from its own codes.

A session's first input arrives only once it is routed, so routing reads
its model from the handshake: `requested_model(&Original)` is `?model=`,
OpenAI's and, by the gateway's convention, a Gemini client's. The model
routing chose goes in the call as `SessionModel`, which OpenAI's and
Mistral's providers dial (else the client's `?model=`; with neither the
session is refused), and into the opening `Session` by
`realtime::set_model`, which Gemini's setup names. Each provider writes its
own query, never the client's.

A transcription is one from its connect, as OpenAI's is: its client asks
for it at `?intent=transcription`, beside the `?model=` that routes it, and
OpenAI's provider dials `?intent=transcription` with no model, naming the
model routing chose as the one that transcribes, in every session. Its
first session is then a transcription, unless it says otherwise; a session
that says nothing of it is a conversation.

Used by: the [binary](../../../apps/gateway/src/main.rs), and the
[realtime examples](../../../examples/realtime).
