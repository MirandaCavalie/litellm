import pytest

from litellm.proxy.auth.customer_spend import settle_customer_spend_from_plan
from tests.unit.proxy.auth import test_customer_spend as scenarios
from tests.unit.proxy.spend_counters_ledger_twin import ledger_scenarios

globals().update(ledger_scenarios(scenarios))


@pytest.fixture(autouse=True)
def _ledger_settlement(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(scenarios, "settle_customer_spend", settle_customer_spend_from_plan)
