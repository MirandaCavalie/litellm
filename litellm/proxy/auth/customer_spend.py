import asyncio
from collections.abc import Awaitable, Callable, Mapping
from typing import Final

from pydantic import ValidationError

from litellm.proxy.spend_tracking.native_spend_counters import customer_settlement
from litellm.redis.stores.spend_counters import SpendCounterStore
from litellm.types.proxy.auth.customer_hierarchy import CustomerHierarchyAuth, CustomerScopeAuth


def customer_spend_counter(scope: CustomerScopeAuth) -> str:
    period: Final = scope.budget_reset_at.isoformat() if scope.budget_reset_at is not None else "lifetime"
    return f"spend:{scope.key}:{scope.entity_id}:{period}"


async def current_customer_spend(
    hierarchy: CustomerHierarchyAuth,
    read_spend: Callable[[str, float, float | None], Awaitable[float]],
) -> CustomerHierarchyAuth:
    readings: Final = await asyncio.gather(
        *(read_spend(customer_spend_counter(scope), scope.spend, scope.max_budget) for scope in hierarchy.scopes)
    )
    scopes: Final = tuple(
        scope.model_copy(update={"spend": max(scope.spend, spend)})
        for scope, spend in zip(hierarchy.scopes, readings, strict=True)
    )
    return hierarchy.model_copy(update={"scopes": tuple(scopes)})


async def settle_customer_spend(metadata: Mapping[str, object], cost: float, store: SpendCounterStore) -> None:
    snapshot: Final = metadata.get("user_api_key_customer_hierarchy")
    if snapshot is None or cost == 0:
        return
    try:
        hierarchy: Final = CustomerHierarchyAuth.model_validate(snapshot)
    except ValidationError:
        return
    counters: Final = tuple((customer_spend_counter(scope), scope.spend) for scope in hierarchy.scopes)
    readings: Final = await asyncio.gather(*(store.read(counter_key) for counter_key, _ in counters))
    settlement: Final = customer_settlement(counters, tuple(reading[0] for reading in readings), cost)
    _ = await asyncio.gather(
        *(store.seed(counter_key, spend, increment_if_lost=False) for counter_key, spend in settlement.seeds)
    )
    _ = await store.increment_many(settlement.increments)
