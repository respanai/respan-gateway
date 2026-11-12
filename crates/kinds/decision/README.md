# Decision kind

`respan-gateway-decision` is the decision kind: typed questions about a
state, each answered with probabilities rather than prose, as TypeSafe's
System One and decision models like it answer them. Its formats over HTTP
are [`respan-gateway-decision-http`](../../formats/decision-http/README.md)'s.

| Type | What it holds |
| --- | --- |
| `Input` | `model`, `state` (a text, or JSON), `questions` in order |
| `Question` | `id`, `instructions`, and an `Ask`: `Noul` (yes or no), `Choice` (options by name, each optionally described), `Score` (levels, lowest first), `Native` (a type the kind has no form for, as its format has it) |
| `Output` events | `Start`, an `Answer` per question, `Usage`, `Native` |
| `Answer` | `id`, `confidence`, and a `Verdict`: `Noul` (the probability of yes), `Choice` (the option chosen and each option's probability), `Score` (the expected level and each level's probability) |

What the kind has no form for, such as a score's `legend` or an answer of a
type it does not know, stays as the format that decoded it has it, and only
that format writes it. The crate is the kind's types alone, depending only
on `respan-gateway-core`, the conversation kind's `Usage`, and `serde_json`.

A choice's options are strings, and a noul has no criteria: a format whose
choices are typed, or whose nouls say what each side means, keeps that as
its own.
