import pytest

from tests.unit.proxy.proxy_server import test_spend_counters as scenarios
from tests.unit.proxy.spend_counters_ledger_twin import force_ledger_windows, ledger_scenarios

globals().update(ledger_scenarios(scenarios))


@pytest.fixture(autouse=True)
def _ledger_windows(monkeypatch: pytest.MonkeyPatch) -> None:
    force_ledger_windows(monkeypatch)
