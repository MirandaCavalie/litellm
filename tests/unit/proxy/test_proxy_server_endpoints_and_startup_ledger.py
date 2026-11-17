from typing import Final

import pytest

from tests.unit.proxy import test_proxy_server_endpoints_and_startup as scenarios
from tests.unit.proxy.spend_counters_ledger_twin import force_ledger_windows, ledger_scenarios

_WINDOW_ROW_QUEUE_SCENARIOS: Final = frozenset({"test_a_slow_window_row_read_does_not_hold_back_the_key_spend_counter"})


def _counter_scenario(name: str) -> bool:
    return ("spend_counter" in name or "increment_spend" in name) and name not in _WINDOW_ROW_QUEUE_SCENARIOS


globals().update(ledger_scenarios(scenarios, _counter_scenario))


@pytest.fixture(autouse=True)
def _ledger_windows(monkeypatch: pytest.MonkeyPatch) -> None:
    force_ledger_windows(monkeypatch)
