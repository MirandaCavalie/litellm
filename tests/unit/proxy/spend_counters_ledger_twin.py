"""Reruns existing post-call counter scenarios with the ledger on, so ``increment_spend_counters`` runs the Rust
counter step. Only scenarios whose store is the Rust ``NativeSpendCounters`` can run that way."""

from __future__ import annotations

from collections.abc import Callable, Mapping
from types import ModuleType
from typing import Final

import pytest

import litellm.proxy.proxy_server as ps

_INCREMENT: Final = ps.increment_spend_counters


async def _ledger_increment(*args: object, **kwargs: object) -> None:
    await _INCREMENT(*args, **{**kwargs, "windows_from_ledger": True})  # pyright: ignore[reportArgumentType]  # forwards the caller's typed arguments


def _scenario(name: str, keep: Callable[[str], bool]) -> bool:
    return not name.startswith("__") and (not name.startswith(("test_", "Test")) or keep(name))


def _namespace(module: ModuleType) -> Mapping[str, object]:
    return vars(module)


def ledger_scenarios(module: ModuleType, keep: Callable[[str], bool] = lambda _: True) -> Mapping[str, object]:
    """The module's tests and the fixtures and helpers they reach by name, with ``increment_spend_counters`` imported
    by name replaced by its ledger form."""
    return {
        name: _ledger_increment if value is _INCREMENT else value
        for name, value in _namespace(module).items()
        if _scenario(name, keep)
    }


def force_ledger_windows(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(ps, "increment_spend_counters", _ledger_increment)
