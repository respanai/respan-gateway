# AWS event stream

`respan-gateway-eventstream` is the binary framing AWS streams responses in
(`application/vnd.amazon.eventstream`), as bytes. It is a wire: it knows no
HTTP and no kind. A `Parser` splits a body into `Message`s, checking both
CRCs of each, and `Message::encode` writes one.

A message is a prelude (its total length, its headers' length and their CRC-32),
typed headers, a payload, and the CRC-32 of all before it. A message that
claims a length past the parser's limit, a header it cannot read or a CRC that
does not match is an upstream fault; a body that ends inside a message
is a truncated one. Which headers a service sends (`:message-type`,
`:event-type`, `:exception-type`) and what its payloads hold is its format's.
