# Decision kind

`respan-gateway-decision` is the decision kind: typed questions about a
state, each answered with probabilities rather than prose, as TypeSafe's
System One and decision models like it answer them. Its formats over HTTP
are [`respan-gateway-decision-http`](../../formats/decision-http/README.md)'s.

| Type | What it holds |
| --- | --- |
| `Input` | `model`, `state` (a text, or JSON), `questions` in order |
| `Question` | `id`, `instructions`, and an `Ask`: `Noul` (yes or no, each side optionally described), `Choice` (typed options, each optionally described), `Score` (levels, lowest first), `Number`, `Box` (where something is in the evidence's image) |
| `Output` events | `Start`, an `Answer` per question, `Usage`, `Native` |
| `Answer` | `id`, `confidence`, and a `Verdict`: `Noul` (the probability of yes), `Choice` (the option chosen and each option's probability), `Score` (the expected level and each level's probability), `Number`, `Box` (the box's edges as fractions of the image), `Refusal` |

What the kind has no form for, such as a score's `legend` or an answer of a
type it does not know, stays as the format that decoded it has it, and only
that format writes it. The crate is the kind's types alone, depending only
on `respan-gateway-core`, the conversation kind's `Usage`, and `serde_json`.

`ChoiceValue` distinguishes string and boolean values in both questions and
answers. This changes the former string-only choice API: callers constructing
choices must wrap strings in `ChoiceValue::String`. A format with string-keyed
choices cannot encode a boolean and refuses it rather than coercing it.
