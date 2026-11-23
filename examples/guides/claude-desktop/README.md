# Use DeepSeek and any other model in Claude Desktop

*Run Claude Desktop, chat and the Code tab, on DeepSeek, GPT, Gemini or a
model on your own machine.*

In the Claude Code CLI this is easy: set `ANTHROPIC_BASE_URL` and pass any
model name. Claude Desktop is different, and this guide is about Desktop.

## Why Desktop needs a router

Claude Desktop can send its requests to your own server instead of
Anthropic's. Anthropic calls this
[third-party inference](https://claude.com/docs/third-party/claude-desktop).
But it comes with two limits:

1. **Desktop only accepts Claude model names.** Its model list expects
   Anthropic model IDs such as `claude-sonnet-4-5`. Enter
   `deepseek-chat` and it tells you it "doesn't look like an Anthropic
   model".
2. **Desktop only speaks the Anthropic Messages API.** Many models, such as
   GPT or a model on Ollama, speak another API.

So you can't point Desktop straight at DeepSeek and pick DeepSeek. Desktop
will ask for `claude-sonnet-4-5`, and DeepSeek has no model by that name.

Respan Gateway sits in the middle and fixes both. A **route** takes the
Claude name Desktop asks for and sends the request to the model you chose,
translating it to that model's API on the way:

```
Claude Desktop ──"claude-sonnet-4-5"──▶ Respan Gateway ──"deepseek-chat"──▶ DeepSeek
                                        route: claude-sonnet-4-5 → deepseek/deepseek-chat
```

In the model picker you still see a name you choose, such as
"DeepSeek Chat".

## 1. Run the gateway with a route

Install it:

```bash
pip install respan-gateway
```

Write a `gateway.toml` that gives each model a Claude name:

```toml
listen = "127.0.0.1:8080"

[providers.deepseek]
vendor = "deepseek"
key = "env:DEEPSEEK_API_KEY"

# Desktop asks for a Claude name; the route sends it to DeepSeek.
[routes."claude-sonnet-4-5"]
to = "deepseek/deepseek-chat"

[routes."claude-opus-4-5"]
to = "deepseek/deepseek-reasoner"
```

Start it:

```bash
export DEEPSEEK_API_KEY=sk-...
respan-gateway
```

The right-hand side of a route can be any model the gateway reaches:
`openai/gpt-6-luna`, `gemini/...`, or `ollama/...` for a local model.
`respan-gateway models` lists what each provider serves.

## 2. Turn on Developer Mode in Claude Desktop

The setting for third-party inference is in the Developer menu, which is
hidden until you turn on Developer Mode.

1. In the menu bar, open **Help → Troubleshooting → Enable Developer
   Mode…**
2. Confirm in the dialog. A **Developer** menu now appears in the menu bar.

<p align="center"><img src="img/enable-developer-mode.png" width="720" alt="Claude Desktop's Help menu, with Troubleshooting open and Enable Developer Mode highlighted"></p>

## 3. Add the gateway as an inference provider

1. Open **Developer → Configure Third-Party Inference…**

   <p align="center"><img src="img/configure-third-party-inference.png" width="520" alt="Claude Desktop's Developer menu, with Configure Third-Party Inference highlighted"></p>

2. Under **Connection**, choose **Gateway**.
3. Fill in **Gateway credentials**:

   | Field | Value |
   |---|---|
   | Credential kind | Static API key |
   | Gateway base URL | `http://127.0.0.1:8080` |
   | Gateway API key | any text, such as `unused`. On `127.0.0.1` the gateway needs no key. If you set `client_keys`, use one of them |
   | Gateway auth scheme | `bearer` |

   Click **Test connection** to check that Desktop reaches the gateway.

   <p align="center"><img src="img/gateway-connection.png" width="560" alt="The Connection settings set to Gateway, with credential kind, base URL, API key and auth scheme filled in"></p>

4. Turn **Model discovery** off, so you choose the models yourself.
5. Under **Model list**, click **Add model** once for each route:

   | Model ID | Display name |
   |---|---|
   | `claude-sonnet-4-5` | DeepSeek Chat |
   | `claude-opus-4-5` | DeepSeek Reasoner |

   The **Model ID** must match a route name in `gateway.toml` exactly. The
   **Display name** is what you see in the model picker.

6. Save, and restart Claude when it asks.

Desktop now opens in third-party mode. Pick **DeepSeek Chat** in the model
picker, in chat or in the Code tab, and your messages go to DeepSeek.

## 4. Check that it works

Open [localhost:8080/ui/](http://localhost:8080/ui/) and send a message
from Desktop. The request shows up with the model it really went to,
`deepseek-chat`, and its tokens, cost and latency.

If a message fails, ask the gateway where a name goes:

```bash
curl "http://127.0.0.1:8080/ui/api/routes/explain?model=claude-sonnet-4-5"
```

## Adding more models

Every new model is two lines in `gateway.toml` and one row in Desktop:

```toml
[routes."claude-haiku-4-5"]
to = "openai/gpt-6-luna"
```

Then click **Add model** in Desktop, set **Model ID** to
`claude-haiku-4-5` and **Display name** to `gpt-6-luna`. You can leave the
other fields empty.

<p align="center"><img src="img/model-list.png" width="560" alt="A model entry in Claude Desktop: Model ID claude-haiku-4-5, Display name gpt-6-luna"></p>

Each Claude name can stand for one model, so use a different Claude name
for each model you want in the picker.

A route can also list several models: `to = ["deepseek/deepseek-chat",
"openai/gpt-6-luna"]` falls back to GPT when DeepSeek fails.
