from typing import Final

import pytest

from tests.unit.proxy import test_budget_reservation as scenarios
from tests.unit.proxy.spend_counters_ledger_twin import force_ledger_windows, ledger_scenarios

# Scenarios whose store is FakeSpendCounters, which the Rust counter step cannot run on. The Rust store tests in
# test_rust_counter_step.py and the crate's tests/charge.rs cover them on the ledger path
_FAKE_STORE_SCENARIOS: Final = frozenset(
    {
        "test_model_access_group_counter_accumulates_across_calls_without_a_reservation",
        "test_should_seed_and_update_end_user_and_tag_counters_without_reservation",
        "test_should_seed_org_counter_from_plain_org_cache",
        "test_should_seed_org_counter_from_with_budget_cache",
    }
)

globals().update(ledger_scenarios(scenarios, lambda name: name not in _FAKE_STORE_SCENARIOS))


@pytest.fixture(autouse=True)
def ledger_windows(monkeypatch: pytest.MonkeyPatch) -> None:
    force_ledger_windows(monkeypatch)
