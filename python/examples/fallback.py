"""A route with fallback, called through the Anthropic SDK, in-process.

    ANTHROPIC_API_KEY=sk-ant-... OPENAI_API_KEY=sk-... python examples/fallback.py
"""

import respan_gateway as rg

CONFIG: rg.GatewayConfig = {
    "providers": {"anthropic": {"vendor": "anthropic"}, "openai": {"vendor": "openai"}},
    "routes": {
        # Each candidate is tried twice, 250 ms apart, before the next.
        "smart": {"to": ["anthropic/claude-haiku-5-5", "openai/gpt-6-luna"], "tries": 2, "backoff": "250ms"},
    },
}


def main(gw: rg.Gateway, model: str = "smart") -> str:
    client = gw.anthropic()
    reply = client.messages.create(model=model, max_tokens=64, messages=[{"role": "user", "content": "Say hi."}])
    text = "".join(block.text for block in reply.content if block.type == "text")
    print(text)
    last = gw.requests(limit=1)[0]
    print(f"answered by {last['provider']} in {last['duration_ms']} ms")
    return text


if __name__ == "__main__":
    with rg.Gateway(config=CONFIG) as gw:
        main(gw)
