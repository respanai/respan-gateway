# Tool vendors

A tool vendor is an HTTP API the gateway serves as an MCP server of its
own, in process, though the API itself speaks no MCP. Set its key in the
environment and it is a server named after it, or name it under `[tools]`
in `gateway.toml`:

```toml
[tools.search]
vendor = "linkup"
key = "env:LINKUP_API_KEY"
```

Its tools are reached like any server's: at `/mcp/{name}`, at `/mcp` as
`{name}__{tool}` (`search__search`), and in the tool loop (see
[MCP](mcp.md#tool-vendors)).

Each tool vendor is in one of two tiers, as the [model vendors](vendors.md)
are:

- 🟢 **Green**: maintained, and verified end to end against the vendor's
  live API.
- 🟡 **Yellow**: maintained, but not yet verified end to end against the
  vendor's live API. A yellow vendor becomes green once it is: once each
  of its tools passes the live check in
  [`crates/tool-vendors`](../crates/tool-vendors/README.md).

## 🟢 Green

| | Vendor | For | `vendor` | Key | Tools |
|:-:|---|---|---|---|---|
| <img src="assets/tool-vendors/apify.svg" width="20" height="20" alt=""> | [Apify](https://apify.com) | Web scraping | `apify` | `APIFY_TOKEN` or `APIFY_API_TOKEN` | `search_actors`, `run_actor`, `get_run_items`, `browse_web` |
| <img src="assets/tool-vendors/browserbase.svg" width="20" height="20" alt=""> | [Browserbase](https://browserbase.com) | Browser automation | `browserbase` | `BROWSERBASE_API_KEY` | `search`, `fetch`, `run_agent`, `get_agent_run`, `stop_agent_run`, `live_view` |
| <img src="assets/tool-vendors/cloudinary.svg" width="20" height="20" alt=""> | [Cloudinary](https://cloudinary.com) | Image and video | `cloudinary` | `CLOUDINARY_URL` | `upload`, `search`, `get_asset`, `delivery_url`, `delete_asset` |
| <img src="assets/tool-vendors/context7.svg" width="20" height="20" alt=""> | [Context7](https://context7.com) | Coding docs | `context7` | `CONTEXT7_API_KEY` | `resolve_library_id`, `query_docs`, `search_docs` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/tool-vendors/daytona-dark.png"><img src="assets/tool-vendors/daytona.png" width="20" height="20" alt=""></picture> | [Daytona](https://daytona.io) | Code sandbox | `daytona` | `DAYTONA_API_KEY` | `create_sandbox`, `list_sandboxes`, `delete_sandbox`, `run_command`, `run_code`, `write_file` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/tool-vendors/linkup-dark.svg"><img src="assets/tool-vendors/linkup.svg" width="20" height="20" alt=""></picture> | [Linkup](https://linkup.so) | Search | `linkup` | `LINKUP_API_KEY` | `search`, `answer`, `fetch` |
| <img src="assets/tool-vendors/reducto.svg" width="20" height="20" alt=""> | [Reducto](https://reducto.ai) | Document parsing | `reducto` | `REDUCTO_API_KEY` | `parse_document`, `extract_fields`, `split_document` |
| <img src="assets/tool-vendors/tavily.svg" width="20" height="20" alt=""> | [Tavily](https://tavily.com) | Search | `tavily` | `TAVILY_API_KEY` | `search`, `extract`, `crawl`, `map_site` |
| <img src="assets/tool-vendors/tinyfish.png" width="20" height="20" alt=""> | [TinyFish](https://tinyfish.ai) | Search and browser agent | `tinyfish` | `TINYFISH_API_KEY` | `search`, `fetch`, `run_agent` |
| <img src="assets/tool-vendors/zep.png" width="20" height="20" alt=""> | [Zep](https://getzep.com) | Memory | `zep` | `ZEP_API_KEY` | `search`, `get_graph_context`, `get_thread_context`, `add_messages`, `add_data` |

## 🟡 Yellow

| | Vendor | For | `vendor` | Key | Tools |
|:-:|---|---|---|---|---|
| | [deed.page](https://deed.page) | Web publishing | `deedpage` | `DEEDPAGE_TOKEN` | `publish_site`, `list_sites`, `delete_site` |
| <img src="assets/tool-vendors/e2b.png" width="20" height="20" alt=""> | [E2B](https://e2b.dev) | Code execution | `e2b` | `E2B_API_KEY` | `run_code`, `list_sandboxes`, `kill_sandbox` |
| <picture><source media="(prefers-color-scheme: dark)" srcset="assets/tool-vendors/elevenlabs-dark.svg"><img src="assets/tool-vendors/elevenlabs.svg" width="20" height="20" alt=""></picture> | [ElevenLabs](https://elevenlabs.io) | Voice | `elevenlabs` | `ELEVENLABS_API_KEY` | `text_to_speech`, `search_voices`, `speech_to_text`, `sound_effect` |
| | [Exa](https://exa.ai) | Search | `exa` | `EXA_API_KEY` | `search`, `get_contents` |
| | [Firecrawl](https://firecrawl.dev) | Web scraping and search | `firecrawl` | `FIRECRAWL_API_KEY` | `scrape`, `crawl`, `get_crawl`, `map_site`, `search` |

These tables are kept by hand: a vendor added to `ALL` in
[`crates/tool-vendors`](../crates/tool-vendors/README.md), or a tool added
to a vendor, is a row or an entry here too.

## What a call used

Each call's result carries what it used, in the vendor's own unit, in its
`_meta` under `respan.ai/usage`: `{"credits": 2}`, `{"pages": 40}`. It is
read from the vendor's answer where the answer says, else worked out from
the arguments; a free call carries none.

| Vendor | Unit | From |
|---|---|---|
| Linkup | `usd` | Its price list, by depth and fetch mode |
| Exa | `usd` | The answer's `costDollars.total` |
| Firecrawl | `credits` | Scrape's `data.metadata.creditsUsed`, search's `creditsUsed`; crawl status shows the job's total without charging it again |
| Tavily | `credits` | The answer's `usage.credits` |
| Context7 | `requests` | One a call |
| TinyFish | `steps` | An agent run's `num_of_steps`; search and fetch are free |
| Apify | `items` | The items a run made; its compute is not in the answer |
| Browserbase | `searches`, `requests`, `runs` | One a search, fetch or agent run |
| Reducto | `pages`, `credits` | The answer's `usage` |
| ElevenLabs | `characters`, `seconds` | The text spoken; the audio transcribed; a sound's set length |
| Cloudinary | `transformations` | An upload, and each eager version of an image |
| E2B, Daytona | `seconds` | The call's time in its sandbox, timed |
| Zep | `episodes`, `credits` | Each message or datum, a credit a 350 bytes |

## Their tools

**Apify**, at `https://api.apify.com/v2`, its key a bearer token:

- `search_actors` is `GET /store`: Actors (scrapers) to run, free.
- `run_actor` is `POST /actors/{actor}/run-sync-get-dataset-items`: runs
  one and waits up to 300 s for its items.
- `get_run_items` reads a past run's items, by run or an Actor's last run.
- `browse_web` runs the `apify~rag-web-browser` Actor: a search or a URL,
  pages as markdown.

Runs are billed: compute, plus the Actor's own price. Results are capped
at about 40,000 characters, with the `offset` to read on from.

**Browserbase**, at `https://api.browserbase.com/v1`, its key in
`X-BB-API-Key`:

- `search` is `POST /search`; `fetch` is `POST /fetch`, a page without a
  browser, as markdown.
- `run_agent` is `POST /agents/runs`: Browserbase's browser agent on a
  task. It returns at once; `get_agent_run` reads it and its result,
  `stop_agent_run` stops it.
- `live_view` is `GET /sessions/{id}/debug`: URLs a person can watch the
  agent's browser at.

**Cloudinary**, at `https://api.cloudinary.com/v1_1`, its key Cloudinary's
own `cloudinary://<api_key>:<api_secret>@<cloud_name>`, which names the
cloud too:

- `upload` uploads an image, video or file from a URL, creating an asset.
- `search` is the Search API; `get_asset` an asset's details.
- `delivery_url` builds a transformation URL, calling nothing.
- `delete_asset` deletes an asset, for good.

An EU or AP account sets `base_url` to `https://api-eu.cloudinary.com/v1_1`
or `https://api-ap.cloudinary.com/v1_1`.

**Context7**, at `https://context7.com/api`, its key a bearer token:

- `resolve_library_id` is `GET /v2/libs/search`: a library's Context7 ID.
- `query_docs` is `GET /v2/context`: a library's current docs for a
  question, as markdown.
- `search_docs` is `GET /v3/search`: the same across up to four libraries.

**Daytona**, at `https://app.daytona.io/api`, its key a bearer token:

- `create_sandbox`, `list_sandboxes` and `delete_sandbox` manage
  sandboxes. A new one stops after 15 idle minutes and is deleted when it
  stops, unless asked otherwise.
- `run_command`, `run_code` and `write_file` work in one, through the
  sandbox's toolbox. `run_code` returns charts as images.

Sandboxes bill per second while they run.

**deed.page**, at `https://deed.page`, its key a bearer token:

- `publish_site` is `POST /v1/sites`: one HTML page, served as
  `index.html` at `https://{slug}.deed.page/`, permanent unless
  `ttl_seconds` is given (sent as `X-Ttl`). The URL is returned, never a
  token.
- `list_sites` is `GET /v1/sites`: the token's sites.
- `delete_site` is `DELETE /v1/sites/{slug}`.

Calls are free. A token comes from a first publish without one (see
[the docs](https://deed.page/docs)); every site it publishes stays under it.

**E2B**, at `https://api.e2b.app`, its key in `X-API-Key`:

- `run_code` runs Python, JavaScript, TypeScript, R, Java or Bash in a
  code-interpreter sandbox, new or given, and returns its output, with
  charts as images.
- `list_sandboxes` and `kill_sandbox` manage them.

New sandboxes live 180 s from creation by default and bill per second.
Reusing one can extend its expiry but cannot shorten an existing longer
lifetime; execution uses part of that time. Call `kill_sandbox` when done.
The key never goes to the sandbox's own host.

**ElevenLabs**, at `https://api.elevenlabs.io`, its key in `xi-api-key`:

- `text_to_speech` returns speech as audio, MP3 by default.
- `search_voices` is `GET /v2/voices`, free.
- `speech_to_text` transcribes audio at a URL, with speakers if asked.
- `sound_effect` returns a generated sound as audio.

Speech is billed per character, transcripts per second of audio.

**Exa**, at `https://api.exa.ai`, its key a bearer token:

- `search` is `POST /search`, with ranked results and relevant highlights.
- `get_contents` is `POST /contents`, returning page text and a status per URL.

Each call is billed; Exa reports its cost in US dollars. Its deprecated
`/findSimilar` endpoint is not offered.

**Firecrawl**, at `https://api.firecrawl.dev/v2`, its key a bearer token
from `FIRECRAWL_API_KEY`:

- `scrape` is `POST /scrape`: a page as markdown, with its metadata.
- `crawl` is `POST /crawl`: starts a site crawl and returns its ID. It
  limits the crawl to 10 pages unless a different `limit` is given.
- `get_crawl` is `GET /crawl/{crawl_id}`: the job's status and scraped
  pages. Use `skip` and `limit` to read another page of results.
- `map_site` is `POST /map`: a site's URLs, without their content.
- `search` is `POST /search`: ranked web results.

Scraping, crawling and search use credits. Scrape and search report the
credits used in the call's usage metadata when the API returns them.
`get_crawl` shows the job's total `creditsUsed` in its result; polling does
not count that total again. Map results carry no usage metadata.

**Linkup**, at `https://api.linkup.so/v1`, its key a bearer token:

- `search` is `POST /search`, for ranked sources.
- `answer` is the same, for a written answer with its sources.
- `fetch` is `POST /fetch`, a page as markdown.

Each call is billed; `depth: "deep"` costs more.

**Reducto**, at `https://platform.reducto.ai`, its key a bearer token. Each
tool takes a document URL, or the `jobid://` of an earlier parse:

- `parse_document` is `POST /parse`: the document as markdown.
- `extract_fields` is `POST /extract`: fields by a JSON Schema.
- `split_document` is `POST /split`: the pages of each named section.

Each is billed per page.

**Tavily**, at `https://api.tavily.com`, its key a bearer token:

- `search` is `POST /search`, with an answer if asked.
- `extract` is `POST /extract`: up to 20 pages as markdown.
- `crawl` is `POST /crawl` and `map_site` `POST /map`: a site's pages, or
  only their URLs.

Each is billed in credits; `advanced` depths cost more.

**TinyFish**, its key in `X-API-Key`:

- `search` is `GET https://api.search.tinyfish.ai/`.
- `fetch` is `POST https://api.fetch.tinyfish.ai/`, up to 10 pages as
  markdown.
- `run_agent` is `POST https://agent.tinyfish.ai/v1/automation/run`, a
  browser agent carrying out a goal on a site and waiting for it: slow,
  and billed in credits.

`base_url` moves only the agent's host.

**Zep**, at `https://api.getzep.com/api/v4`, its key as
`Authorization: Api-Key <key>`. Users, threads and graphs are the UUIDs
Zep gives them, which the model must be given:

- `search` searches a graph's facts, entities or episodes.
- `get_graph_context` and `get_thread_context` return a context block.
- `add_messages` and `add_data` add to memory, billed per 350 bytes.

A tool's result is text for the model (results as compact JSON or
markdown, URLs kept), with images or audio where the tool makes them. Bad
arguments, a failed request and a failed run are tool errors, told to the
model as such.

How a tool vendor is built, and how to add one, is in
[`crates/tool-vendors`](../crates/tool-vendors/README.md).

---

Logos are each vendor's trademark, shown only to identify it. Tavily's and
ElevenLabs' are from [LobeHub Icons](https://github.com/lobehub/lobe-icons)
(MIT); the others are from the vendors' own sites.
