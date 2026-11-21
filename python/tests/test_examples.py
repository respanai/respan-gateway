"""The examples in `examples/` run against the fake provider."""

from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

import respan_gateway as rg
from conftest import CHAT, config_for

EXAMPLES = Path(__file__).parent.parent / "examples"


def load(name: str):
    spec = importlib.util.spec_from_file_location(f"example_{name}", EXAMPLES / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@pytest.mark.parametrize("name", ["chat", "media", "serve"])
def test_example_runs(fake, name, capsys):
    with rg.Gateway(config=config_for(fake)) as gw:
        load(name).main(gw)
    assert capsys.readouterr().out


def test_fallback_example_falls_back(fake, capsys):
    config = config_for(fake)
    config["providers"]["down"] = {"formats": ["openai-chat"], "base_url": "http://127.0.0.1:9/v1"}
    config["routes"] = {"smart": {"to": ["down/fake-1", "openai/fake-1"], "backoff": "10ms"}}
    with rg.Gateway(config=config) as gw:
        assert load("fallback").main(gw)
        assert gw.requests(limit=1)[0]["provider"] == "openai"
    assert "answered by openai" in capsys.readouterr().out


def test_gateway_prints_nothing_per_request_by_default(fake, capfd):
    with rg.Gateway(config=config_for(fake)) as gw:
        gw.request("POST", "/v1/chat/completions", json=CHAT)
        assert gw.requests()  # the in-memory log still records
    assert capfd.readouterr().err == ""


def test_config_can_ask_for_the_log(fake, capfd):
    with rg.Gateway(config={**config_for(fake), "log": "json"}) as gw:
        gw.request("POST", "/v1/chat/completions", json=CHAT)
    assert "fake-1" in capfd.readouterr().err
