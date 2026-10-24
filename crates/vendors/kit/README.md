# Vendor kit

`respan-gateway-vendor-kit` is a vendor as a template, and a provider of it,
and what vendors share. The vendors themselves are
[`respan-gateway-vendors`](../README.md)', green and yellow.

- `Vendor`: where its key comes from (`key_env`), the formats it speaks,
  its dialects of a spec among them, each at the base URL its docs give
  (where a `{NAME}` is that environment variable, `located`), and how it
  lists its models if not as OpenAI does (`models`). What it does not say
  is `Vendor::DEFAULT`'s.
- A format is any core `Format` that says how its spec has it reached
  (`Upstream`, in `respan-gateway-http`): where it takes the key
  (`KeyIn`), over HTTP or as a WebSocket session and what redirects it
  follows (`Reach`), and whether its paths are whole (`at_origin`). The kit
  lists formats by it and depends on none.
- `VendorProvider`: a provider of a vendor (`Vendor::provider`), or of
  formats of no vendor (`VendorProvider::new`), with a key. Each format is
  at a base URL of its own (the vendor's, or `base_of`), used as it is; or
  at one its formats share (`base`), its API's, where a format of whole
  paths takes the origin. `register` adds a core provider of each format,
  taking the key as the format does, over HTTP or, for a session,
  WebSocket. `models` lists its models; `client` is a client of it, at its
  first API's base URL with its key, for any call of its own.
- `FORMATS` and `format`: every spec's format, as a provider of no vendor
  speaks it, and a format by name as a vendor speaks it or as its spec has
  it.
- `chat`: what vendors of Chat Completions share in their dialects:
  reasoning taken back as `reasoning_content` (`write_reasoning`, a filter
  of a call's input).
- `Dialect` and `Dialected`: a vendor's dialect of a spec, laid over the
  spec's format (`respan_gateway_http::dialect`).

A format is a spec and names no vendor; a vendor's departures from a spec
are its own, in the formats it speaks: its dialect of the spec, laid over
the spec's format, or a format of its own.
