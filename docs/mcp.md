# Tools

Each `[tools.<name>]` is a tool server: an [MCP](https://modelcontextprotocol.io)
server, or a tool vendor's HTTP API. The gateway fronts them behind the same
client keys as everything else, serving each over MCP at `/mcp/{name}`:

```toml
[tools.github]                          # HTTP upstream
url = "https://api.githubcopilot.com/mcp/"
auth = { bearer = "env:GITHUB_TOKEN" }

[tools.files]                           # stdio upstream, a child per client session
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/data"]

[tools.linear]                          # A server a person must approve once
url = "https://mcp.linear.app/mcp"
auth = "login"                          # Run: respan-gateway mcp login linear
```

The fields match an MCP client's `mcp.json`, so entries copy across. `auth`
can be:

- `{ bearer = "env:KEY" }`, a static key;
- `{ oauth = { token_url, client_id, client_secret, ... } }`, the gateway's
  own OAuth client;
- `"login"`, a login saved by `respan-gateway mcp login <name>`. That
  command opens your browser on Enter. On a headless machine, it takes the
  callback address pasted back instead;
- `"forward"`, which sends each client's own credentials on.

`respan-gateway mcp` lists the servers and whether each is authenticated.

## Tool vendors

A tool vendor is an HTTP API that is not an MCP server, such as
[Linkup](https://linkup.so) or [TinyFish](https://tinyfish.ai). With
`vendor`, the gateway serves its tools as an MCP server of its own, in
process:

```toml
[tools.search]
vendor = "linkup"                       # Key from LINKUP_API_KEY

[tools.tinyfish]
vendor = "tinyfish"
key = "env:MY_TINYFISH_KEY"             # Default: TINYFISH_API_KEY
# base_url = "https://..."              # Default: the vendor's own
```

A `vendor` entry takes `key` (literal or `env:NAME`), `base_url` and `proxy`
alone.
Its tools are reached like any server's: at `/mcp/{name}`, at `/mcp` as
`{name}__{tool}` (`search__search`, `tinyfish__run_agent`), and in the tool
loop. [Tool vendors](tool-vendors.md) lists them,
their tiers and their tools.

Without a `gateway.toml`, each tool vendor whose key is in the environment
(such as `TAVILY_API_KEY`) is a server named after it, served at `/mcp` with
the others. The tool loop stays off until you configure it.

## One endpoint for all servers

`/mcp` merges every tool server into one, with tools named
`{server}__{tool}`. It is always served when there are tool servers, and
fronts all of them but those with `auth = "forward"`, whose sessions would
need a client's own credentials. A client key limited to certain `tools`
reaches tool servers only through it
(see [client keys](configuration.md#client-keys)), and a tool turned off
(below) is not offered.

## Tool loop

With `[plugins.tool_loop]`, the gateway gives the model the MCP servers'
tools and runs the calls itself, for any chat format and any provider, and
in realtime sessions too. The client sees one ordinary reply.

```toml
[plugins.tool_loop]
servers = ["github"]                    # Default: every server /mcp fronts
allow = ["github__search_*"]            # Offer only what you would let any client do
max_steps = 8
```

The model sees tools as `<server>__<tool>`, with other characters replaced
by `_` and names shortened to 64 characters. If distinct tools would have
the same name, each gets a stable suffix; other names stay as they were.
`allow`, `deny`, and client tool scopes match these final names. Names are
assigned before filters or switches hide tools, so those do not change
which original tool a name calls.

A tool's description and output are untrusted input to the model. Offer
only tools from servers you trust, and use `deny` to keep destructive ones
out. A client can opt one request out with `x-respan-tool-loop: off`.

## Turning tools off

The web UI's Tools page turns a server or a single tool off and on while
the gateway runs, as does the admin API: `PUT /ui/api/mcp/{server}` or
`PUT /ui/api/mcp/{server}/tools/{tool}` with `{"enabled": false}` (or
`true`). A tool turned off, or every tool of a server turned off, is
neither offered nor run by `/mcp` and the tool loop, from their next list
or call. A server turned off also refuses clients at `/mcp/{server}`
(`403 tool_server_off`); a single tool turned off is still reached there,
since that endpoint relays the server as it is. The switches are held in
memory only: a config reload keeps them, a restart turns everything back on.
