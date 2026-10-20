# Decision formats

`respan-gateway-decision-http` is the decision kind over HTTP.

| Format | Endpoint | Served by |
| --- | --- | --- |
| `decisions` (`Decisions`) | `POST /v1/decisions`, `POST /v1/systemone` | Respan (`vendor = "respan"`); TypeSafe (`vendor = "typesafe"`), at `/systemone`; or any compatible base URL |

A provider is called at `/decisions`. A vendor that departs from the
contract says so in its `dialect::Dialect`, and lists
`Dialected::new(Decisions, dialect)`, a provider format of the spec's name:
TypeSafe's says it serves it at `/systemone`.

A request is `{"model", "state", "questions": {id: question}}`, each question
`{"type", "instructions", "criteria"}`: `noul` with criteria
`{"true", "false"}`, `choice` with `{option: description or null}`, `score`
with a list of levels. A response is `{"model", "answers": {id: answer},
"usage": {"input_tokens", "output_tokens"}}`, answered whole in JSON.

A question of another type is refused, 400, before any provider is called.
An answer of a type the kind has no form for is kept and written back where
it was. A refusal is `{"error": {"message", "type", "code"}}`, whose message
TypeSafe's SDKs read.
