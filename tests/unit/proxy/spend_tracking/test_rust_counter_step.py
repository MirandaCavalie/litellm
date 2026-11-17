"""The ledger's post-call counter step, which ``increment_spend_counters`` hands to Rust, run on the Rust spend
counter store: in memory, and over a Redis server."""

from __future__ import annotations

import threading
from collections.abc import Iterator, Mapping
from typing import Final
from unittest.mock import AsyncMock, MagicMock

import pytest
import redis
from fakeredis import TcpFakeServer
from pydantic import TypeAdapter

import litellm.proxy.proxy_server as ps
from litellm.caching.redis_cache import RedisCache
from litellm.models.budget import LiteLLM_BudgetTable
from litellm.models.end_user import LiteLLM_EndUserTable
from litellm.models.tag import LiteLLM_TagTable
from litellm.models.user import LiteLLM_UserTable
from litellm.proxy._types import LiteLLM_OrganizationTable, UserAPIKeyAuth
from litellm.proxy.common_utils.user_api_key_cache import UserApiKeyCache, model_access_group_cache_key
from litellm.proxy.spend_tracking.committed_spend import committed_spend_key
from litellm.proxy.spend_tracking.rust_spend_counters import runs_on_rust_store
from litellm.redis.stores.connection import connect
from litellm.redis.stores.spend_counters import (
    SpendCounterStore,
    model_access_group_counter,
    user_window_counter,
)
from litellm.types.proxy.auth.customer_hierarchy import CustomerHierarchyAuth, CustomerScopeAuth
from litellm.types.proxy.carried_budget_state import EntitySpendCounterKeys
from litellm.types.proxy.model_access_group_budget import ModelAccessGroupBudget
from tests.unit.proxy.spend_tracking.native_budgets_replica import native_reservation

pytestmark = pytest.mark.usefixtures("native_budgets_on")

_ENTRIES: Final = TypeAdapter(tuple[Mapping[str, object], ...])
_EVERY_BUCKET: Final = (
    "spend:key:hashed",
    "spend:team:team-1",
    "spend:team_member:user-1:team-1",
    "spend:user:user-1",
    "spend:end_user:eu-1",
    "spend:tag:tag-a",
    "spend:tag:tag-b",
    "spend:org:org-1",
)


@pytest.fixture
def key_cache(monkeypatch: pytest.MonkeyPatch) -> UserApiKeyCache:
    cache: Final = UserApiKeyCache()
    monkeypatch.setattr(ps, "user_api_key_cache", cache)
    monkeypatch.setattr(ps, "prisma_client", None)
    return cache


@pytest.fixture
def store(monkeypatch: pytest.MonkeyPatch, key_cache: UserApiKeyCache) -> SpendCounterStore:
    installed: Final = SpendCounterStore(connect(None).spend_counters())
    monkeypatch.setattr(ps, "spend_counter_store", installed)
    return installed


def _stored_spend(monkeypatch: pytest.MonkeyPatch, spend: Mapping[str, float]) -> AsyncMock:
    async def from_db(prisma_client: object, counter_key: str) -> float | None:
        if counter_key == "spend:team:fails":
            raise RuntimeError("reseed failed")
        return spend.get(counter_key)

    read: Final = AsyncMock(side_effect=from_db)
    monkeypatch.setattr(ps.SpendCounterReseed, "from_db", read)
    return read


async def _charge_every_bucket(cost: float, **options: object) -> None:
    await ps.increment_spend_counters(
        token="hashed",
        team_id="team-1",
        user_id="user-1",
        response_cost=cost,
        org_id="org-1",
        end_user_id="eu-1",
        tags=["tag-a", "tag-b"],
        windows_from_ledger=True,
        **options,  # pyright: ignore[reportArgumentType]  # each test passes the step's typed options
    )


@pytest.mark.asyncio
async def test_cold_counters_seed_from_stored_spend_and_take_the_charge(
    monkeypatch: pytest.MonkeyPatch, store: SpendCounterStore
) -> None:
    read: Final = _stored_spend(monkeypatch, {"spend:key:hashed": 3.0, "spend:team:team-1": 4.0})

    await _charge_every_bucket(0.5)

    assert store.snapshot() == (
        {
            "spend:key:hashed": 3.5,
            "spend:team:team-1": 4.5,
            "spend:team_member:user-1:team-1": 0.5,
            "spend:user:user-1": 0.5,
            "spend:end_user:eu-1": 0.5,
            "spend:tag:tag-a": 0.5,
            "spend:tag:tag-b": 0.5,
            "spend:org:org-1": 0.5,
        }
    )
    assert {call.kwargs["counter_key"] for call in read.await_args_list} >= {"spend:key:hashed", "spend:team:team-1"}


@pytest.mark.asyncio
async def test_warm_counters_take_the_charge_without_reading_stored_spend(
    monkeypatch: pytest.MonkeyPatch, store: SpendCounterStore
) -> None:
    for counter in _EVERY_BUCKET:
        store.set_local(counter, 1.0)
    read: Final = _stored_spend(monkeypatch, {})

    await _charge_every_bucket(0.5)

    assert store.snapshot() == dict.fromkeys(_EVERY_BUCKET, 1.5)
    read.assert_not_awaited()


@pytest.mark.asyncio
async def test_end_user_and_tag_counters_seed_from_the_auth_cache(
    store: SpendCounterStore, key_cache: UserApiKeyCache
) -> None:
    await key_cache.async_set_cache(
        key="end_user_id:customer-1",
        value=LiteLLM_EndUserTable(
            user_id="customer-1", blocked=False, spend=4.0, litellm_budget_table=LiteLLM_BudgetTable(max_budget=10.0)
        ).model_dump(),
    )
    await key_cache.async_set_cache(
        key="tag:paid-tag", value=LiteLLM_TagTable(tag_name="paid-tag", spend=7.0).model_dump()
    )
    await key_cache.async_set_cache(
        key="tag:other-tag", value=LiteLLM_TagTable(tag_name="other-tag", spend=2.0).model_dump()
    )

    await ps.increment_spend_counters(
        token=None,
        team_id=None,
        user_id=None,
        response_cost=0.5,
        end_user_id="customer-1",
        tags=["paid-tag", "paid-tag", "other-tag", ""],
        windows_from_ledger=True,
    )

    assert store.snapshot() == (
        {"spend:end_user:customer-1": 4.5, "spend:tag:paid-tag": 7.5, "spend:tag:other-tag": 2.5}
    )


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("cache_key", "budget"),
    [
        ("org_id:org-1:with_budget", LiteLLM_BudgetTable(max_budget=10.0)),
        ("org_id:org-1", None),
    ],
)
async def test_org_counter_seeds_from_either_org_cache_entry(
    store: SpendCounterStore, key_cache: UserApiKeyCache, cache_key: str, budget: LiteLLM_BudgetTable | None
) -> None:
    await key_cache.async_set_cache(
        key=cache_key,
        value=LiteLLM_OrganizationTable(
            organization_id="org-1",
            organization_alias="shared-org",
            budget_id="org-budget-id",
            spend=2.0,
            models=[],
            created_by="test",
            updated_by="test",
            litellm_budget_table=budget,
        ).model_dump(),
    )

    await ps.increment_spend_counters(
        token=None, team_id=None, user_id=None, org_id="org-1", response_cost=0.25, windows_from_ledger=True
    )

    assert store.snapshot() == {"spend:org:org-1": 2.25}


@pytest.mark.asyncio
async def test_model_access_group_counter_accumulates_across_calls(
    store: SpendCounterStore, key_cache: UserApiKeyCache
) -> None:
    await key_cache.async_set_cache(
        key=model_access_group_cache_key("premium"),
        value=ModelAccessGroupBudget(access_group_name="premium", spend=1.0, max_budget=25.0),
        model_type=ModelAccessGroupBudget,
    )

    await ps.increment_spend_counters(
        token=None,
        team_id=None,
        user_id=None,
        response_cost=0.25,
        model_access_groups=["premium"],
        windows_from_ledger=True,
    )
    await ps.increment_spend_counters(
        token=None,
        team_id=None,
        user_id=None,
        response_cost=0.75,
        model_access_groups=["premium", "premium", ""],
        windows_from_ledger=True,
    )

    assert store.snapshot() == {model_access_group_counter("premium"): 2.0}


@pytest.mark.asyncio
async def test_a_reserved_counter_is_reconciled_and_the_reservation_stamped_and_finalized(
    monkeypatch: pytest.MonkeyPatch, store: SpendCounterStore
) -> None:
    store.set_local("spend:key:hashed", 10.0)
    store.set_local("spend:team:team-1", 1.0)
    _stored_spend(monkeypatch, {})
    reservation: Final = await native_reservation(
        store,
        UserAPIKeyAuth(token="hashed"),
        [{"counter_key": "spend:key:hashed", "entity_type": "Key", "entity_id": "hashed"}],
        2.0,
    )
    assert store.get_local("spend:key:hashed") == 12.0

    await ps.increment_spend_counters(
        token="hashed",
        team_id="team-1",
        user_id=None,
        response_cost=0.5,
        budget_reservation=reservation,
        windows_from_ledger=True,
    )

    assert store.get_local("spend:key:hashed") == 10.5
    assert store.get_local("spend:team:team-1") == 1.5
    assert reservation["finalized"] is True
    assert [entry["applied_adjustment"] for entry in _ENTRIES.validate_python(reservation["entries"])] == [-1.5]


@pytest.mark.asyncio
async def test_a_zero_cost_charge_writes_nothing_and_finalizes_the_reservation(store: SpendCounterStore) -> None:
    reservation: Final[dict[str, object]] = {"finalized": False}

    await ps.increment_spend_counters(
        token="hashed",
        team_id="team-1",
        user_id="user-1",
        response_cost=0,
        budget_reservation=reservation,
        windows_from_ledger=True,
    )

    assert reservation == {"finalized": True}
    assert store.snapshot() == {}


_USER_WINDOWS: Final = [
    {"budget_duration": "1h", "max_budget": 5.0, "reset_at": "2026-08-01T01:00:00Z"},
    {"budget_duration": "30d", "max_budget": 100.0, "reset_at": "2026-08-31T00:00:00Z"},
]


@pytest.mark.asyncio
async def test_user_windows_from_the_auth_cache_take_the_charge(
    store: SpendCounterStore, key_cache: UserApiKeyCache
) -> None:
    for counter in ("spend:user:u", user_window_counter("u", "1h"), user_window_counter("u", "30d")):
        store.set_local(counter, 0.0)
    await key_cache.async_set_cache(
        key="u", value=LiteLLM_UserTable.model_validate({"user_id": "u", "budget_limits": _USER_WINDOWS})
    )

    await ps.increment_spend_counters(
        token=None, team_id=None, user_id="u", response_cost=2.0, windows_from_ledger=True
    )

    assert store.snapshot() == (
        {"spend:user:u": 2.0, user_window_counter("u", "1h"): 2.0, user_window_counter("u", "30d"): 2.0}
    )


@pytest.mark.asyncio
async def test_user_windows_missing_from_the_auth_cache_come_from_the_stored_row(
    monkeypatch: pytest.MonkeyPatch, store: SpendCounterStore
) -> None:
    for counter in ("spend:user:u", user_window_counter("u", "1h"), user_window_counter("u", "30d")):
        store.set_local(counter, 1.0)
    stored: Final = AsyncMock(return_value=_USER_WINDOWS)
    monkeypatch.setattr(ps, "_stored_budget_limits", stored)

    await ps.increment_spend_counters(
        token=None, team_id=None, user_id="u", response_cost=2.0, windows_from_ledger=True
    )

    assert store.snapshot() == (
        {"spend:user:u": 3.0, user_window_counter("u", "1h"): 3.0, user_window_counter("u", "30d"): 3.0}
    )
    assert [call.args[1] for call in stored.await_args_list] == ["u"]


@pytest.mark.asyncio
async def test_a_charge_outside_the_committed_scope_keeps_no_twin_and_reads_no_stored_spend(
    monkeypatch: pytest.MonkeyPatch, store: SpendCounterStore
) -> None:
    store.set_local("spend:key:hashed", 10.0)
    read: Final = _stored_spend(monkeypatch, {"spend:key:hashed": 10.0})

    await ps.increment_spend_counters(
        token="hashed", team_id=None, user_id=None, response_cost=0.5, windows_from_ledger=True
    )

    assert store.snapshot() == {"spend:key:hashed": 10.5}
    read.assert_not_awaited()


@pytest.mark.asyncio
async def test_only_the_entities_in_the_committed_scope_take_the_charge_on_their_twin(
    monkeypatch: pytest.MonkeyPatch, store: SpendCounterStore
) -> None:
    store.set_local("spend:key:hashed", 10.0)
    store.set_local("spend:team:t1", 10.0)
    _stored_spend(monkeypatch, {"spend:key:hashed": 10.0})

    await ps.increment_spend_counters(
        token="hashed",
        team_id="t1",
        user_id=None,
        response_cost=0.5,
        committed_spend_scope=EntitySpendCounterKeys(key="spend:key:hashed"),
        windows_from_ledger=True,
    )

    assert store.snapshot() == (
        {"spend:key:hashed": 10.5, "spend:team:t1": 10.5, committed_spend_key("spend:key:hashed"): 10.5}
    )


@pytest.mark.asyncio
async def test_a_budgeted_team_whose_id_names_a_window_keeps_its_charge_and_gets_no_twin(
    monkeypatch: pytest.MonkeyPatch, store: SpendCounterStore
) -> None:
    team_counter_key: Final = "spend:team:t1:window:1d"
    store.set_local(team_counter_key, 1.0)
    monkeypatch.setattr(ps, "prisma_client", MagicMock())

    await ps.increment_spend_counters(
        token=None,
        team_id="t1:window:1d",
        user_id=None,
        response_cost=0.5,
        committed_spend_scope=EntitySpendCounterKeys(team=team_counter_key),
        windows_from_ledger=True,
    )

    assert store.snapshot() == {team_counter_key: 1.5}


@pytest.mark.asyncio
async def test_a_failing_stored_spend_read_propagates_after_every_other_counter_is_charged(
    monkeypatch: pytest.MonkeyPatch, store: SpendCounterStore
) -> None:
    warm: Final = ("spend:key:hashed", "spend:user:user-1", "spend:end_user:eu-1", "spend:tag:tag-a", "spend:org:org-1")
    for counter in (*warm, "spend:team_member:user-1:fails"):
        store.set_local(counter, 1.0)
    _stored_spend(monkeypatch, {})
    reservation: Final[dict[str, object]] = {"finalized": False}

    with pytest.raises(RuntimeError, match="reseed failed"):
        await ps.increment_spend_counters(
            token="hashed",
            team_id="fails",
            user_id="user-1",
            response_cost=5.0,
            org_id="org-1",
            end_user_id="eu-1",
            tags=["tag-a"],
            budget_reservation=reservation,
            windows_from_ledger=True,
        )

    assert reservation["finalized"] is False
    assert store.snapshot() == dict.fromkeys((*warm, "spend:team_member:user-1:fails"), 6.0)


@pytest.mark.asyncio
async def test_the_customer_hierarchy_settles_before_the_charge(store: SpendCounterStore) -> None:
    hierarchy: Final = CustomerHierarchyAuth(
        customer_org_id="c",
        customer_team_id="ct",
        scopes=(
            CustomerScopeAuth(key="customer_org", entity_id="c", spend=2.0),
            CustomerScopeAuth(key="customer_team", entity_id="ct", spend=1.0),
        ),
    )

    await ps.increment_spend_counters(
        token=None,
        team_id=None,
        user_id=None,
        response_cost=0.5,
        customer_hierarchy=hierarchy,
        windows_from_ledger=True,
    )

    assert store.snapshot() == ({"spend:customer_org:c:lifetime": 2.5, "spend:customer_team:ct:lifetime": 1.5})


def test_only_the_rust_store_runs_the_rust_step(monkeypatch: pytest.MonkeyPatch, store: SpendCounterStore) -> None:
    assert runs_on_rust_store()

    monkeypatch.setattr(ps, "spend_counter_store", SpendCounterStore(MagicMock()))

    assert not runs_on_rust_store()


@pytest.fixture
def redis_port() -> Iterator[int]:
    server: Final = TcpFakeServer(("127.0.0.1", 0), server_type="redis")
    thread: Final = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    yield server.server_address[1]
    server.shutdown()
    server.server_close()


@pytest.mark.asyncio
async def test_on_redis_every_bucket_is_charged_in_the_shared_counters(
    monkeypatch: pytest.MonkeyPatch, key_cache: UserApiKeyCache, redis_port: int
) -> None:
    shared: Final = redis.Redis(host="127.0.0.1", port=redis_port, decode_responses=True)
    for counter in _EVERY_BUCKET:
        shared.set(counter, 1.0)
    installed: Final = SpendCounterStore(connect(RedisCache(host="127.0.0.1", port=redis_port)).spend_counters())
    monkeypatch.setattr(ps, "spend_counter_store", installed)

    await _charge_every_bucket(0.5)

    assert {counter: float(shared.get(counter) or "nan") for counter in _EVERY_BUCKET} == (
        dict.fromkeys(_EVERY_BUCKET, 1.5)
    )
    assert installed.snapshot() == dict.fromkeys(_EVERY_BUCKET, 1.5)
