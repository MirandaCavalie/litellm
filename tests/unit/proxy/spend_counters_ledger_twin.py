"""Reruns existing post-call counter scenarios with ``windows_from_ledger=True``, the path the Rust plan serves."""

from __future__ import annotations

from collections.abc import Callable, Mapping
from types import ModuleType
from typing import Final

import pytest

import litellm.proxy.proxy_server as ps


def _scenario(name: str, keep: Callable[[str], bool]) -> bool:
    return not name.startswith("__") and (not name.startswith(("test_", "Test")) or keep(name))


def ledger_scenarios(module: ModuleType, keep: Callable[[str], bool] = lambda _: True) -> Mapping[str, object]:
    """The module's tests and the fixtures and helpers they reach by name."""
    return {name: value for name, value in vars(module).items() if _scenario(name, keep)}


def force_ledger_windows(monkeypatch: pytest.MonkeyPatch) -> None:
    original: Final = ps._increment_spend_counters_batched  # pyright: ignore[reportPrivateUsage]  # the twin wraps the fan-out entry

    async def ledger(**kwargs: object) -> None:
        await original(**{**kwargs, "windows_from_ledger": True})  # pyright: ignore[reportArgumentType]  # forwards the caller's typed kwargs

    monkeypatch.setattr(ps, "_increment_spend_counters_batched", ledger)
