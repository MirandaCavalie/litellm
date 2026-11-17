"""The Rust spend log callback leaves the same queued spend log rows as the Python cost callback on the ledger
path. Each path writes into its own Rust spend log writer over an unreachable database, so the rows stay queued
and the test reads them back."""

import asyncio
import json
from collections.abc import AsyncIterator, Callable, Mapping, Sequence
from datetime import datetime, timezone
from typing import Final

import pytest
import pytest_asyncio

import litellm
import litellm.proxy.proxy_server as ps
from litellm.integrations.custom_logger_v2.enums import Field, Hook
from litellm.integrations.custom_logger_v2.field_sources import HookArgs
from litellm.integrations.custom_logger_v2.rust import RustLogger
from litellm.litellm_core_utils.litellm_logging import V2_ASYNC_LOGGING_LINE
from litellm.proxy._types import UserAPIKeyAuth
from litellm.proxy.db.native_database import ServiceLoggingEvents, native_endpoint
from litellm.proxy.db.rust_spend_log_writer import install_rust_spend_log_writer, uninstall_rust_spend_log_writer
from litellm.proxy.hooks.rust_spend_logger import CachedSpendBumps, rust_spend_loggers, spend_logger
from litellm.proxy.spend_tracking.rust_spend_log_inputs import spend_log_success_input
from litellm.proxy.utils import BilledFailedAttempt, PrismaClient, ProxyUpdateSpend
from litellm.rust_bridge._native import NativeDatabase, NativeSpendCounters, RustSpendLogWriter
from litellm.types.integrations.custom_logger_v2 import Outcome
from tests.unit.proxy.db.test_rust_spend_log_writer import FakePrismaClient
from tests.unit.proxy.hooks.rust_spend_logger_twin import (
    Completion,
    completion,
    legacy_failure,
    legacy_success,
    rust_failure,
    rust_success,
    without,
)

_UNREACHABLE: Final = "postgresql://litellm:secret@db.invalid:5432/proxy?pool_timeout=1"
_LEDGER_ON: Final[Mapping[str, object]] = {"use_spend_logs_ledger": True}
_VOLATILE: Final = ("accounting_delivery_id",)
_FAILURE_VOLATILE: Final = (
    "accounting_delivery_id",
    "endTime",
    "completionStartTime",
    "request_duration_ms",
    "session_id",
    "accounting_payload",
)
_KEYED: Final[Mapping[str, object]] = {
    "user_api_key": "hashed-key",
    "user_api_key_alias": "alias",
    "user_api_key_user_id": "user-1",
    "user_api_key_team_id": "team-1",
    "user_api_key_org_id": "org-1",
}
_STARTED: Final = datetime(2026, 11, 17, 9, 0, tzinfo=timezone.utc)


class Paths:
    def __init__(self, legacy_client: FakePrismaClient, legacy: RustSpendLogWriter, rust: RustSpendLogWriter) -> None:
        self.legacy_client: Final = legacy_client
        self.legacy: Final = legacy
        self.rust: Final = rust
        handle: Final = ps.spend_counter_store.handle
        assert isinstance(handle, NativeSpendCounters)
        self.logger: Final = spend_logger(rust, handle)

    def legacy_rows(self) -> Sequence[Mapping[str, object]]:
        return [json.loads(row) for row in self.legacy.dequeue(100)]

    def rust_rows(self) -> Sequence[Mapping[str, object]]:
        return [json.loads(row) for row in self.rust.dequeue(100)]


@pytest_asyncio.fixture
async def paths(monkeypatch: pytest.MonkeyPatch) -> AsyncIterator[Paths]:
    monkeypatch.setattr(PrismaClient, "spend_log_queue_bytes", 0)
    database: Final = await NativeDatabase.connect(
        ServiceLoggingEvents(loop=asyncio.get_running_loop(), service_logging=lambda: None),
        native_endpoint(_UNREACHABLE, tls_offered=False, max_connections=1).endpoint,
        None,
    )
    legacy_client: Final = FakePrismaClient()
    rust_client: Final = FakePrismaClient()
    legacy: Final = await install_rust_spend_log_writer(legacy_client, database, _LEDGER_ON, None)  # pyright: ignore[reportArgumentType]  # the fake stands in for the Prisma client
    rust: Final = await install_rust_spend_log_writer(rust_client, database, _LEDGER_ON, None)  # pyright: ignore[reportArgumentType]  # the fake stands in for the Prisma client
    assert legacy is not None and rust is not None
    monkeypatch.setattr(ps, "prisma_client", legacy_client)
    monkeypatch.setattr(ps, "general_settings", {"use_spend_logs_ledger": True})
    try:
        yield Paths(legacy_client, legacy, rust)
    finally:
        uninstall_rust_spend_log_writer(legacy_client)
        uninstall_rust_spend_log_writer(rust_client)


async def _both_succeed(
    paths: Paths, done: Completion
) -> tuple[Sequence[Mapping[str, object]], Sequence[Mapping[str, object]]]:
    await legacy_success(done)
    await rust_success(paths.logger, done)
    return paths.legacy_rows(), paths.rust_rows()


@pytest.mark.asyncio
async def test_a_priced_completion_queues_the_same_ledger_row(paths: Paths) -> None:
    legacy, rust = await _both_succeed(paths, await completion(_KEYED))

    assert len(rust) == 1
    assert [without(row, _VOLATILE) for row in rust] == [without(row, _VOLATILE) for row in legacy]
    assert rust[0]["spend"] == legacy[0]["spend"]
    assert rust[0]["accounting_delivery_id"]


@pytest.mark.asyncio
async def test_request_logs_off_queues_the_same_accounting_only_row(
    paths: Paths, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(ps, "disable_spend_logs", True)

    legacy, rust = await _both_succeed(paths, await completion(_KEYED))

    assert len(rust) == 1
    assert [without(row, _VOLATILE) for row in rust] == [without(row, _VOLATILE) for row in legacy]


@pytest.mark.asyncio
async def test_an_end_user_and_tags_land_on_the_same_row(paths: Paths) -> None:
    done: Final = await completion(
        {**_KEYED, "tags": ["blue", "green"], "user_api_key_end_user_id": "end-user-1"}, user="end-user-1"
    )

    legacy, rust = await _both_succeed(paths, done)

    assert len(rust) == 1
    assert [without(row, _VOLATILE) for row in rust] == [without(row, _VOLATILE) for row in legacy]
    assert json.loads(str(rust[0]["request_tags"])) == ["blue", "green"]


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "metadata",
    [pytest.param({}, id="nobody_to_charge"), pytest.param(_KEYED, id="spend_updates_disabled")],
)
async def test_an_untracked_completion_queues_no_row_on_either_path(
    paths: Paths, monkeypatch: pytest.MonkeyPatch, metadata: Mapping[str, object]
) -> None:
    if metadata:
        monkeypatch.setattr(ProxyUpdateSpend, "disable_spend_updates", staticmethod(lambda: True))

    legacy, rust = await _both_succeed(paths, await completion(metadata))

    assert (legacy, rust) == ([], [])


def _auth() -> UserAPIKeyAuth:
    return UserAPIKeyAuth(
        api_key="hashed-key",
        key_alias="alias",
        user_id="user-1",
        team_id="team-1",
        org_id="org-1",
        team_alias="team alias",
    )


async def _both_fail(
    paths: Paths, request_data: Mapping[str, object], error: Exception
) -> tuple[Sequence[Mapping[str, object]], Sequence[Mapping[str, object]]]:
    await legacy_failure(request_data, error, _auth())
    await rust_failure(paths.logger, request_data, error, _auth())
    return paths.legacy_rows(), paths.rust_rows()


@pytest.mark.asyncio
async def test_a_failed_request_queues_the_same_failure_row(paths: Paths) -> None:
    request_data: Final = {
        "model": "gpt-4o",
        "litellm_call_id": "failed-call",
        "start_time": _STARTED,
        "messages": [{"role": "user", "content": "hi"}],
        "metadata": {"caller": "value"},
        "proxy_server_request": {"body": {"model": "gpt-4o"}},
    }

    legacy, rust = await _both_fail(paths, request_data, ValueError("the provider refused"))

    assert len(rust) == 1
    assert rust[0]["status"] == "failure"
    assert [without(row, _FAILURE_VOLATILE) for row in rust] == [without(row, _FAILURE_VOLATILE) for row in legacy]


@pytest.mark.asyncio
async def test_each_billed_attempt_queues_the_same_attempt_row(paths: Paths) -> None:
    usage: Final = litellm.Usage(prompt_tokens=24, completion_tokens=0, total_tokens=24)
    logged: Final = (await completion(_KEYED)).kwargs["standard_logging_object"]
    assert isinstance(logged, Mapping)
    request_data: Final = {
        "model": "gpt-4.1",
        "litellm_call_id": "call-1",
        "start_time": _STARTED,
        "standard_logging_object": {**logged, "id": "call-1", "model_id": "dep-final"},
        "metadata": {
            "attempted_retries": 1,
            "model_info": {"id": "dep-final", "input_cost_per_token": 0.5},
            "previous_models": ({"exception_type": "Timeout"},),
        },
        "combined_usage_object": usage,
        "response_cost": 0.5,
        "billed_failed_attempts": (
            BilledFailedAttempt(attempted_retries=0, deployment_id="dep-a", usage=usage, response_cost=0.25),
        ),
    }

    legacy, rust = await _both_fail(
        paths, request_data, litellm.Timeout("Request timed out.", model="gpt-4.1", llm_provider="openai")
    )

    assert sorted(str(row["request_id"]) for row in rust) == ["call-1", "call-1-attempt-0"]
    by_id: Final[Callable[[Sequence[Mapping[str, object]]], Sequence[Mapping[str, object]]]] = lambda rows: sorted(
        (without(row, _FAILURE_VOLATILE) for row in rows), key=lambda row: str(row["request_id"])
    )
    assert by_id(rust) == by_id(legacy)


@pytest.mark.asyncio
async def test_an_untracked_failure_route_queues_no_row(paths: Paths) -> None:
    auth: Final = _auth()
    auth.request_route = "/key/generate"

    await legacy_failure({"model": "gpt-4o"}, ValueError("no"), auth)
    await rust_failure(paths.logger, {"model": "gpt-4o"}, ValueError("no"), auth)

    assert (paths.legacy_rows(), paths.rust_rows()) == ([], [])


@pytest.mark.asyncio
async def test_the_cached_spend_bump_adds_the_charge_to_the_cached_team(
    paths: Paths, monkeypatch: pytest.MonkeyPatch
) -> None:
    added: Final[list[tuple[object, ...]]] = []
    monkeypatch.setattr(ps, "add_spend_to_cached_spend_objects", lambda *args: added.append(args[1:]))
    done: Final = await completion(_KEYED)
    document: Final = spend_log_success_input(
        HookArgs(request=dict(done.kwargs), result=done.response, start_time=done.start_time, end_time=done.end_time)
    )
    view: Final = type("View", (), {Field.spend_log_input.value: document, Field.outcome.value: Outcome.success})()
    line: Final = V2_ASYNC_LOGGING_LINE.set(True)
    try:
        await CachedSpendBumps().on_attempt_ended(view, None)  # pyright: ignore[reportArgumentType]  # the handler reads no context
    finally:
        V2_ASYNC_LOGGING_LINE.reset(line)

    assert document is not None
    assert added == [("user-1", None, "team-1", [], _row_cost(document))]


def _row_cost(document: Mapping[str, object]) -> object:
    row: Final = document["row"]
    assert isinstance(row, Mapping)
    return row["response_cost"]  # pyright: ignore[reportUnknownVariableType]  # the derivation's row request


@pytest.mark.asyncio
async def test_the_proxy_registers_the_rust_callback_only_over_a_rust_writer(paths: Paths) -> None:
    registered: Final = rust_spend_loggers(paths.legacy_client)
    missing: Final = rust_spend_loggers(object())

    assert registered is not None and missing is None
    rust, bumps = registered
    assert isinstance(rust, RustLogger) and isinstance(bumps, CachedSpendBumps)
    assert rust.selection.name == "spend_log"
    assert callable(getattr(rust, f"on_{Hook.attempt_ended.value}"))
    assert callable(getattr(rust, f"on_{Hook.post_call_failure.value}"))
