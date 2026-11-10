# Decision formats

`respan-gateway-decision-http` is the decision kind over HTTP.

| Format | Endpoint | Served by |
| --- | --- | --- |
| `openai-decisions` (`OpenAiDecisions`) | `POST /openai/v1/decisions` in the standalone gateway | OpenAI (`vendor = "openai"`); Azure OpenAI (`vendor = "azure"`); Fireworks (`vendor = "fireworks"`) |
| `decisions` (`Decisions`) | `POST /v1/decisions`, `POST /v1/systemone` | Respan (`vendor = "respan"`); TypeSafe (`vendor = "typesafe"`) and Fireworks (`vendor = "fireworks"`), at `/systemone`; EmpirioLabs AI (`vendor = "empiriolabs"`); or any compatible base URL |

A provider is called at `/decisions`. A vendor that departs from the
contract lays its dialect over the format
(`respan_gateway_http::dialect::Dialected`), a provider format of the
spec's name: TypeSafe's and Fireworks' move the call to `/systemone`.

A request is `{"model", "state", "questions": {id: question}}`, each question
`{"type", "instructions", "criteria"}`: `noul` with criteria
`{"true", "false"}`, `choice` with `{option: description or null}`, `score`
with a list of levels, `number`, and `box` (answered with `{"box": {"top",
"bottom", "left", "right"}}`, each a fraction of the image). A response is `{"model", "answers": {id: answer},
"usage": {"input_tokens", "output_tokens"}}`, answered whole in JSON.

A question of another type is refused, 400, before any provider is called.
An answer of a type the kind has no form for is kept and written back where
it was. A refusal is `{"error": {"message", "type", "code"}}`, whose message
TypeSafe's SDKs read.

`OpenAiDecisions`, named `openai-decisions`: OpenAI's contract, evidence as
`input` and questions and answers as ordered lists. Its clients are served at
`/v1/decisions`, or at another path (`OpenAiDecisions::serving`): the
standalone gateway serves them at `/openai/v1/decisions`, beside `decisions`.

- It reads into the kind and writes from it, so either format's client is
  served by either's provider. A question's id is its `name`, or its
  position for one unnamed or named as one before it, and a provider is sent
  each question named for its id. Evidence another format gave as JSON is
  sent as its JSON text; a predicate with criteria is refused, 400.
- An answer is read against the question at its position: one out of turn,
  of another type than the question's but a `refusal`, or unreadable is the
  provider's fault. A client is answered from its own request, so its names
  and levels' labels are as it asked them.
- Boolean choices stay apart from strings, and `decisions`, whose choices
  are named, refuses them, as it refuses a refusal.
- Azure (`vendor = "azure"`) serves it in its dialect: the key in `api-key`,
  called at `/decisions` under the configured v1 base.
