from __future__ import annotations

from collections.abc import Iterable, Sequence
from typing import TYPE_CHECKING, Annotated, Final, Literal

from pydantic import BaseModel, ConfigDict, Field, TypeAdapter

from litellm.rust_bridge._native import (
    budget_window_counters,
    committed_spend_counter_keys,
    customer_spend_settlement,
    spend_counter_keys,
    spend_counter_plan,
)

if TYPE_CHECKING:
    from litellm.proxy.spend_tracking.budget_reservation import BudgetCounter

WindowedEntity = Literal["key", "team", "user"]


class CounterTask(BaseModel):
    model_config = ConfigDict(frozen=True)

    task: Literal["counter"]
    counter_key: str
    source_cache_keys: tuple[str, ...]

    @property
    def source_cache_key(self) -> str | list[str]:
        return self.source_cache_keys[0] if len(self.source_cache_keys) == 1 else list(self.source_cache_keys)


class CachedWindowsTask(BaseModel):
    model_config = ConfigDict(frozen=True)

    task: Literal["cached_windows"]
    entity_type: WindowedEntity
    entity_id: str
    cache_key: str


class CustomerWindowTask(BaseModel):
    model_config = ConfigDict(frozen=True)

    task: Literal["customer_window"]
    index: int
    counter_key: str
    entity_label: str
    reserved: bool


class CommittedTask(BaseModel):
    model_config = ConfigDict(frozen=True)

    task: Literal["committed"]
    counter_keys: tuple[str, ...]


SpendCounterTask = Annotated[
    CounterTask | CachedWindowsTask | CustomerWindowTask | CommittedTask, Field(discriminator="task")
]


class SpendCounterScope(BaseModel):
    model_config = ConfigDict(frozen=True)

    concurrent: bool
    tasks: tuple[SpendCounterTask, ...]


class SpendCounterPlan(BaseModel):
    model_config = ConfigDict(frozen=True)

    scopes: tuple[SpendCounterScope, ...]


class WindowCounter(BaseModel):
    model_config = ConfigDict(frozen=True)

    counter_key: str
    entity_label: str
    reserved: bool


class CustomerSettlement(BaseModel):
    model_config = ConfigDict(frozen=True)

    seeds: tuple[tuple[str, float], ...]
    increments: tuple[tuple[str, float], ...]


_KEYS: Final = TypeAdapter(tuple[str, ...])
_WINDOW_COUNTERS: Final = TypeAdapter(tuple[WindowCounter, ...])


def _text(value: object) -> str | None:
    return value if isinstance(value, str) else None


def _names(values: Iterable[object] | None) -> list[str | None] | None:
    return None if values is None else [_text(value) for value in values]


def _charge(
    token: object,
    team_id: object,
    user_id: object,
    org_id: object,
    end_user_id: object,
    project_id: object,
    tags: Iterable[object] | None,
    model_access_groups: Iterable[object] | None,
    customer_counters: Sequence[BudgetCounter],
) -> dict[str, object]:  # mutable-ok: handed straight to the native call as its keyword record
    return {
        "token": _text(token),
        "team_id": _text(team_id),
        "user_id": _text(user_id),
        "org_id": _text(org_id),
        "end_user_id": _text(end_user_id),
        "project_id": _text(project_id),
        "tags": _names(tags),
        "model_access_groups": _names(model_access_groups),
        "customer_windows": [
            {
                "counter_key": counter.counter_key,
                "entity_type": counter.entity_type,
                "window_duration": counter.window_duration,
            }
            for counter in customer_counters
        ],
    }


def post_call_counter_keys(
    token: object,
    team_id: object,
    user_id: object,
    org_id: object,
    end_user_id: object,
    tags: Iterable[object] | None,
    model_access_groups: Iterable[object] | None,
    project_id: object = None,
    customer_counters: Sequence[BudgetCounter] = (),
) -> frozenset[str]:
    return frozenset(
        _KEYS.validate_python(
            spend_counter_keys(
                _charge(
                    token,
                    team_id,
                    user_id,
                    org_id,
                    end_user_id,
                    project_id,
                    tags,
                    model_access_groups,
                    customer_counters,
                )
            )
        )
    )


def plan_spend_counters(
    token: str | None,
    team_id: str | None,
    user_id: str | None,
    org_id: str | None,
    end_user_id: str | None,
    project_id: str | None,
    tags: Iterable[object] | None,
    model_access_groups: Iterable[object] | None,
    reserved_counter_keys: Iterable[str],
    committed_counter_keys: Sequence[str],
    customer_counters: Sequence[BudgetCounter],
) -> SpendCounterPlan:
    return SpendCounterPlan.model_validate(
        spend_counter_plan(
            {
                **_charge(
                    token,
                    team_id,
                    user_id,
                    org_id,
                    end_user_id,
                    project_id,
                    tags,
                    model_access_groups,
                    customer_counters,
                ),
                "reserved_counter_keys": list(reserved_counter_keys),
                "committed_counter_keys": list(committed_counter_keys),
            }
        )
    )


def committed_counter_keys(tracked: Iterable[str], response_cost: float | None) -> tuple[str, ...]:
    return tuple(committed_spend_counter_keys(list(tracked), response_cost))


def window_counter(
    entity_type: WindowedEntity, entity_id: str, duration: str, reserved_counter_keys: Iterable[str]
) -> WindowCounter:
    return _WINDOW_COUNTERS.validate_python(
        budget_window_counters(entity_type, entity_id, [duration], list(reserved_counter_keys))
    )[0]


def customer_settlement(
    scopes: Sequence[tuple[str, float]], readings: Sequence[float | None], cost: float
) -> CustomerSettlement:
    return CustomerSettlement.model_validate(
        customer_spend_settlement(
            [{"counter_key": counter_key, "spend": spend} for counter_key, spend in scopes], list(readings), cost
        )
    )
