from typing import Final

import pytest

from tests.unit.proxy.proxy_server import test_spend_counters as scenarios
from tests.unit.proxy.spend_counters_ledger_twin import force_ledger_windows, ledger_scenarios

# Scenarios whose store is FakeSpendCounters, which the Rust counter step cannot run on. The Rust store tests in
# test_rust_counter_step.py and the crate's tests/charge.rs cover them on the ledger path
_FAKE_STORE_SCENARIOS: Final = frozenset(
    {
        "test_a_budgeted_team_whose_id_names_a_window_keeps_its_charge_and_gets_no_twin",
        "test_a_charge_outside_the_committed_scope_keeps_no_twin_and_reads_no_stored_spend",
        "test_increment_spend_counters_failing_scope_propagates_after_siblings_settle",
        "test_increment_spend_counters_increments_all_buckets",
        "test_increment_spend_counters_pipeline_failure_invalidates_all_counters",
        "test_increment_spend_counters_pipelines_all_scopes_in_one_redis_call",
        "test_increment_spend_counters_runs_scopes_concurrently",
        "test_increment_spend_counters_skips_reserved_counter_keys",
        "test_increment_spend_counters_timeout_invalidates_all_counters_without_failing",
        "test_increment_spend_counters_zero_cost_is_noop_finalizes_reservation",
        "test_only_the_entities_in_the_committed_scope_take_the_charge_on_their_twin",
        "test_user_spend_increments_both_windows",
    }
)

globals().update(ledger_scenarios(scenarios, lambda name: name not in _FAKE_STORE_SCENARIOS))


@pytest.fixture(autouse=True)
def ledger_windows(monkeypatch: pytest.MonkeyPatch) -> None:
    force_ledger_windows(monkeypatch)
