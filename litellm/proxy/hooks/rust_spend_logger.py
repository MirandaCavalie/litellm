import asyncio
import json
from collections.abc import Coroutine, Mapping, Sequence
from typing import Final

from pydantic import TypeAdapter

from litellm._internal_context import service_target
from litellm._logging import verbose_proxy_logger
from litellm.constants import MINIMUM_CUSTOM_KEY_LENGTH
from litellm.integrations.custom_logger_v2.call import CallContext
from litellm.integrations.custom_logger_v2.enums import Activation, Field, Hook
from litellm.integrations.custom_logger_v2.logger import CustomLoggerV2, on
from litellm.integrations.custom_logger_v2.outcomes import Abort, Patch
from litellm.integrations.custom_logger_v2.rust import RustLogger, rust_logger
from litellm.integrations.custom_logger_v2.selection import Selection
from litellm.litellm_core_utils.safe_json_dumps import safe_json_structure
from litellm.proxy.auth.auth_object_prefetch import AUTH_OBJECTS_TARGET
from litellm.proxy.db.db_spend_update_writer import debitable_model_access_groups, get_llm_router
from litellm.proxy.db.rust_spend_log_writer import rust_spend_log_writer
from litellm.proxy.hooks.proxy_track_cost_callback import ProxyDBLogger
from litellm.proxy.spend_tracking.rust_spend_counters import (
    _CounterHost,  # pyright: ignore[reportPrivateUsage]  # the counter step's host, which the callback's host extends
)
from litellm.proxy.spend_tracking.rust_spend_log_rows import decode_row, token_word_ranges
from litellm.responses.streaming_iterator import spawn_logging_task
from litellm.rust_bridge._native import NativeSpendCounters, RustSpendLog, RustSpendLogWriter
from litellm.types.proxy.auth.customer_hierarchy import CustomerHierarchyIds, CustomerScopeAuth

_FIELDS: Final = TypeAdapter(dict[str, object])
_SCOPES: Final = TypeAdapter(tuple[CustomerScopeAuth, ...])
_KEY_IDENTITY: Final = (
    ("key_alias", "user_api_key_alias"),
    ("user_id", "user_api_key_user_id"),
    ("team_id", "user_api_key_team_id"),
    ("org_id", "user_api_key_org_id"),
    ("project_id", "user_api_key_project_id"),
    ("team_alias", "user_api_key_team_alias"),
)


def _mapping(value: object) -> Mapping[str, object]:
    return value if isinstance(value, Mapping) else {}  # pyright: ignore[reportUnknownVariableType]  # host requests are JSON objects


def _text(value: object) -> str | None:
    return value if isinstance(value, str) else None


def _cost(value: object) -> float:
    return float(value) if isinstance(value, int | float) else 0.0


def _texts(value: object) -> Sequence[str]:
    return [item for item in value if isinstance(item, str)] if isinstance(value, list) else []  # pyright: ignore[reportUnknownVariableType]  # a JSON array


def _customer(entities: object) -> CustomerHierarchyIds | None:
    ids: Final = _mapping(entities)
    org_id: Final = _text(ids.get("customer_org_id"))
    if org_id is None:
        return None
    return CustomerHierarchyIds(
        customer_org_id=org_id,
        customer_team_id=_text(ids.get("customer_team_id")),
        customer_member_id=_text(ids.get("customer_member_id")),
    )


def _spawn(coroutine: Coroutine[object, object, object], task_name: str) -> None:
    _ = spawn_logging_task(asyncio.get_running_loop(), coroutine, task_name=task_name)


async def _soft_budget_alert(token: str, response_cost: float) -> None:
    from litellm.proxy.proxy_server import projected_soft_budget_alert, user_api_key_cache

    with service_target(AUTH_OBJECTS_TARGET):
        await projected_soft_budget_alert(user_api_key_cache, token, response_cost)


async def _proxy_spend(response_cost: float) -> None:
    from litellm.proxy.proxy_server import add_spend_to_budgeted_global_proxy_spend, user_api_key_cache

    await add_spend_to_budgeted_global_proxy_spend(user_api_key_cache, response_cost)


async def _committed_tracked(request: Mapping[str, object]) -> Sequence[str]:
    from litellm.proxy.proxy_server import spend_counter_store
    from litellm.proxy.spend_tracking.committed_spend import committed_spend_tracked, warm_committed_counter_keys
    from litellm.types.proxy.carried_budget_state import EntitySpendCounterKeys

    carried: Final = request.get("committed")
    if isinstance(carried, list):
        return _texts(carried)  # pyright: ignore[reportUnknownArgumentType]  # a JSON array from the host request
    cost: Final = request.get("response_cost")
    if not isinstance(cost, int | float) or cost <= 0 or not committed_spend_tracked():
        return []
    warmed: Final = await warm_committed_counter_keys(
        spend_counter_store,
        EntitySpendCounterKeys.of(
            token=_text(request.get("token")),
            team_id=_text(request.get("team_id")),
            user_id=_text(request.get("user_id")),
            org_id=_text(request.get("org_id")),
            end_user_id=_text(request.get("end_user_id")),
        ),
    )
    return list(warmed.tracked())


class SpendLogHost(_CounterHost):
    async def enrich_key_identity(self, request: Mapping[str, object]) -> Mapping[str, object]:
        metadata: Final[dict[str, object]] = {  # mutable-ok: the enrichment fills it in place
            "user_api_key": request.get("api_key"),
            **{logged: request.get(member) for member, logged in _KEY_IDENTITY},
        }
        _ = await ProxyDBLogger._enrich_failure_metadata_with_key_info(  # pyright: ignore[reportPrivateUsage, reportUnknownMemberType, reportUnknownVariableType]  # the cost callback's own enrichment fills the dict in place
            metadata=metadata, resolve_missing_key_identity=request.get("resolve_missing_key_identity") is True
        )
        return {member: _text(metadata.get(logged)) for member, logged in _KEY_IDENTITY}

    async def accounting_context(self, request: Mapping[str, object]) -> Mapping[str, object]:
        from litellm.accounting.snapshot import (
            _savings_or_zero,  # pyright: ignore[reportPrivateUsage]  # savings pricing stays in Python and feeds the Rust row
        )
        from litellm.proxy.proxy_server import accounting_window_keys, customer_accounting_counters

        row_text: Final = request.get("row")
        if not isinstance(row_text, str):
            raise TypeError("an accounting context request carries the built row as JSON text")
        row: Final = decode_row(row_text)
        scopes: Final = request.get("customer_scopes")
        token: Final = _text(request.get("token"))
        team_id: Final = _text(request.get("team_id"))
        user_id: Final = _text(request.get("user_id"))
        charges: Final = request.get("windows") is True
        customer_counters: Final = (
            await customer_accounting_counters(
                _customer(request.get("customer")), None if scopes is None else _SCOPES.validate_python(scopes)
            )
            if charges
            else ()
        )
        windows: Final = (
            await accounting_window_keys(token, team_id, customer_counters, user_id=user_id) if charges else ()
        )
        return {
            "windows": json.loads(json.dumps(safe_json_structure(list(windows)))),
            "savings": _savings_or_zero(row, _FIELDS.validate_json(row["metadata"]), get_llm_router)._asdict(),
            "model_access_groups": list(
                debitable_model_access_groups(
                    _texts(request.get("model_access_groups")), row["model_id"], get_llm_router()
                )
            ),
            "customer_windows": [
                {
                    "counter_key": counter.counter_key,
                    "entity_type": counter.entity_type,
                    "entity_id": counter.entity_id,
                    "window_duration": counter.window_duration,
                    "window_start": None if counter.window_start is None else counter.window_start.isoformat(),
                }
                for counter in customer_counters
            ],
            "committed_tracked": await _committed_tracked(request),
        }

    def increment_proxy_spend(self, request: Mapping[str, object]) -> None:
        _spawn(_proxy_spend(_cost(request.get("response_cost"))), "global proxy spend")

    def soft_budget_alert(self, request: Mapping[str, object]) -> None:
        token: Final = _text(request.get("token"))
        if token is not None:
            _spawn(_soft_budget_alert(token, _cost(request.get("response_cost"))), "soft budget alert")

    def customer_spend_alert(self, request: Mapping[str, object]) -> None:
        from litellm.proxy.proxy_server import proxy_logging_obj

        max_budget: Final = request.get("max_budget")
        _spawn(
            proxy_logging_obj.slack_alerting_instance.customer_spend_alert(
                token=_text(request.get("token")),
                key_alias=_text(request.get("key_alias")),
                end_user_id=_text(request.get("end_user_id")),
                response_cost=_cost(request.get("response_cost")),
                max_budget=float(max_budget) if isinstance(max_budget, int | float) else None,
            ),
            "customer spend alert",
        )


def _on_async_line(hook: Hook) -> bool:
    from litellm.litellm_core_utils.litellm_logging import V2_ASYNC_LOGGING_LINE

    return hook is not Hook.attempt_ended or V2_ASYNC_LOGGING_LINE.get()


class _AsyncLineRustLogger(RustLogger):
    async def run_rust(
        self, hook: Hook, reads: frozenset[Field], view: object, ctx: CallContext[object]
    ) -> Patch | Abort | None:
        if not _on_async_line(hook):
            return None
        return await super().run_rust(hook, reads, view, ctx)


def _retention() -> tuple[tuple[str, ...] | None, tuple[str, ...] | None, tuple[str, ...]] | None:
    from litellm.proxy._types import SPEND_LOGS_METADATA_ALWAYS_KEPT_FIELDS
    from litellm.proxy.spend_tracking.spend_tracking_utils import configured_spend_logs_metadata_fields

    fields: Final = configured_spend_logs_metadata_fields()
    if fields is None:
        return None
    return fields.include, fields.exclude, tuple(sorted(SPEND_LOGS_METADATA_ALWAYS_KEPT_FIELDS))


def spend_logger(writer: RustSpendLogWriter, spend_counters: NativeSpendCounters) -> RustLogger:
    rust: Final = RustSpendLog(
        writer, spend_counters, MINIMUM_CUSTOM_KEY_LENGTH, token_word_ranges(), _retention()
    ).callback()
    return rust_logger(rust, host=SpendLogHost(), base=_AsyncLineRustLogger)


class CachedSpendBumps(CustomLoggerV2):
    selection = Selection(name="cached_spend_bumps", activation=Activation.required)

    @on(Hook.attempt_ended, reads=frozenset({Field.spend_log_input, Field.outcome}), awaited=True)
    async def on_attempt_ended(self, view: object, ctx: CallContext[object]) -> None:
        from litellm.proxy.hooks.proxy_track_cost_callback import (
            _should_track_cost_callback,  # pyright: ignore[reportPrivateUsage]  # the cost callback's own gate
        )
        from litellm.proxy.proxy_server import add_spend_to_cached_spend_objects, user_api_key_cache
        from litellm.types.integrations.custom_logger_v2 import Outcome

        if not _on_async_line(Hook.attempt_ended):
            return
        if getattr(view, Field.outcome.value, None) in (Outcome.failure, Outcome.fallback_failure):
            return
        document: Final = _mapping(getattr(view, Field.spend_log_input.value, None))
        if document.get("kind") != "success" or document.get("disposition") != "charge":
            return
        cost: Final = _mapping(document.get("row")).get("response_cost")
        identity: Final = _mapping(document.get("identity"))
        if not isinstance(cost, int | float) or not _should_track_cost_callback(
            user_api_key=_text(identity.get("token")),
            user_id=_text(identity.get("user_id")),
            team_id=_text(identity.get("team_id")),
            end_user_id=_text(identity.get("end_user_id")),
            call_type=_text(document.get("call_type")),
            agent_id=_text(identity.get("agent_id")),
        ):
            return
        tags: Final = document.get("tags")
        add_spend_to_cached_spend_objects(
            user_api_key_cache,
            _text(identity.get("user_id")),
            _text(identity.get("end_user_id")),
            _text(identity.get("team_id")),
            _texts(tags),
            float(cost),
        )


def rust_spend_loggers(prisma_client: object) -> tuple[RustLogger, CachedSpendBumps] | None:
    from litellm.proxy.proxy_server import spend_counter_store

    writer: Final = rust_spend_log_writer(prisma_client)
    handle: Final = spend_counter_store.handle
    if writer is None or not isinstance(handle, NativeSpendCounters):
        verbose_proxy_logger.debug("Spend tracking - no Rust spend log writer, keeping the Python cost callback")
        return None
    return spend_logger(writer, handle), CachedSpendBumps()


__all__ = ["CachedSpendBumps", "SpendLogHost", "rust_spend_loggers", "spend_logger"]
