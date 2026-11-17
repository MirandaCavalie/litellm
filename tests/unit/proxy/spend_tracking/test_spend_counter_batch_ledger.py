from typing import Final

import pytest

from tests.unit.proxy.spend_tracking import test_spend_counter_batch as scenarios
from tests.unit.proxy.spend_counters_ledger_twin import force_ledger_windows, ledger_scenarios

# Scenarios whose store is FakeSpendCounters, which the Rust counter step cannot run on. The Rust store tests in
# test_rust_counter_step.py and the crate's tests/charge.rs cover them on the ledger path
_FAKE_STORE_SCENARIOS: Final = frozenset(
    {
        "test_post_call_cold_counters_seed_from_the_mget_miss_without_a_second_read",
        "test_post_call_increment_for_every_entity_costs_one_mget_and_one_pipeline",
    }
)

globals().update(ledger_scenarios(scenarios, lambda name: name not in _FAKE_STORE_SCENARIOS))


@pytest.fixture(autouse=True)
def ledger_windows(monkeypatch: pytest.MonkeyPatch) -> None:
    force_ledger_windows(monkeypatch)
