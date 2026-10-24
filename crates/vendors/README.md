# Vendors

`respan-gateway-vendors` is what the gateway knows of each vendor: where
it is, how it takes a key, and the formats it speaks, as it speaks them.
`ALL` lists them; the binary's `vendor = "<id>"` is one. Each vendor is one
file in its tier's crate:

- [`green/`](green) (`respan-gateway-vendors-green`): maintained, and
  verified end to end against the vendor's live API.
- [`yellow/`](yellow) (`respan-gateway-vendors-yellow`): maintained, but
  not verified end to end against the vendor's live API. A yellow vendor
  becomes green once it is.

[`kit/`](kit) (`respan-gateway-vendor-kit`) is what they are built of and
share. This crate puts them together (`ALL`, `GREEN`, `YELLOW`, `find`),
each by its name (`respan_gateway_vendors::openai`).

A vendor is a template (`Vendor`, from the kit). A
`VendorProvider` is one with a key, each format at its base URL, ready to
register, so a provider builder writes none of it again:

```rust,ignore
use respan_gateway_vendors::openai;

// Each format at its base URL, its key from `OPENAI_API_KEY`; `.base(..)`,
// `.base_of(format, ..)` and `.key(..)` set others.
let openai = openai::VENDOR.provider("openai");
// A core provider of each format it speaks, each at its base URL, taking
// the key as the format does.
let gateway = openai.register(Gateway::new())?;
// Its models, by its model list.
let models = openai.models().await;
```

A provider of no vendor is `VendorProvider::new(name).formats(..).base(..)`,
each format as its spec has it (`FORMATS`, `format(None, name)`).

A format is a spec and names no vendor: it keeps what a vendor sends beyond
the spec native, where it appeared. A quirk is a vendor's deployment
departing from a spec it speaks, and its file handles it:

- as data: each format's base URL, as its docs give it, and how it lists
  its models if not as OpenAI's `GET /models` does (`Vendor::models`, a fn
  of the provider, which calls with `client()`);
- as its dialect of the spec's format, such as `groq::Groq`, a `Dialect`
  (`respan_gateway_http::dialect`) laid over Chat Completions: its inputs
  before the format writes them and its outputs after the format reads
  them, as the kind's values; the call and its answer on the wire, their
  heads and whole bodies; and the header it takes its key in if not the
  spec's. A dialect says how the vendor departs from the spec, never who
  the client is. The vendor lists it as
  `(&Dialected::new(OpenAiChat, Groq), Some(BASE_URL))`, a format of the
  spec's name, so a client of the spec is still relayed to it, and no
  plugin of the gateway's sees anything but the kind's values;
- as a format of its own, for a vendor that speaks no spec.

What a spec says, and what its own vendor does, such as Anthropic's key in
`x-api-key`, OpenAI Realtime's transcription model or Gemini's thought
signatures in `generateContent`, is the spec's, in its format
(`AnthropicMessages`, `OpenAiRealtime`, `GeminiGenerate`), not a quirk. Each quirk
is described where its file handles it.

A vendor is one file in its tier's crate, a line in that crate's `ALL`, and
one in this crate's `ALL` and module list. A provider of no vendor speaks
each spec as it is: it takes its key as the spec's format does, at the
base URL its config gives.

Every vendor, its tier, key and formats, and each one's base URL, are in
[`docs/vendors.md`](../../docs/vendors.md); a vendor added here, or a format
added to a vendor, is a row or an entry there too.
