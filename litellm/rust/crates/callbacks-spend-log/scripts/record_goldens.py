import contextvars
import copy
import hashlib
import json
import math
import sys
import uuid as std_uuid
from collections.abc import Mapping
from datetime import datetime
from pathlib import Path
from unittest import mock

CRATE = Path(__file__).resolve().parents[1]
REPO = CRATE.parents[3]
GOLDENS = CRATE / "tests" / "goldens"
TEST_FILES = (
    "tests/unit/proxy/spend_tracking/test_spend_tracking_utils.py",
    "tests/unit/proxy/db/test_db_spend_update_writer.py",
    "tests/unit/accounting",
    "tests/unit/proxy/hooks/test_proxy_track_cost_callback.py",
)
FIXED_NOW = 1763337600.123456
FIXED_SESSION = "7d1c9f9e-1d61-4cf4-9a43-c1b5e5d6c0f1"
FIXED_DELIVERY = std_uuid.UUID("0f0e0d0c-0b0a-4908-8706-050403020100")

_records: dict[str, list[dict]] = {"rows": [], "accounting": [], "ledger_rows": []}
_seen: set[str] = set()
_ledger_call: contextvars.ContextVar[dict | None] = contextvars.ContextVar("ledger_call", default=None)
_current_test = ["<collection>"]


class Unencodable(Exception):
    pass


def _jsonable(value: object, *, lenient: bool = False) -> object:
    if isinstance(value, datetime):
        return value.isoformat()
    if value is None or type(value) in (bool, int, float):
        return value
    if type(value) is str:
        return value
    if lenient and isinstance(value, str):
        return str.__str__(value)
    if type(value) is dict:
        if not all(type(key) is str for key in value):
            raise Unencodable(repr(value)[:200])
        return {key: _jsonable(item, lenient=lenient) for key, item in value.items()}
    if type(value) is list or (lenient and type(value) is tuple):
        return [_jsonable(item, lenient=lenient) for item in value]
    if lenient and isinstance(value, Mapping):
        return {str(key): _jsonable(item, lenient=lenient) for key, item in value.items()}
    raise Unencodable(f"{type(value).__name__}: {repr(value)[:200]}")


def _row_text(row: Mapping[str, object]) -> str:
    return json.dumps(_jsonable(dict(row), lenient=True))


def _add(kind: str, record: dict) -> None:
    digest = hashlib.sha256(json.dumps({**record, "case": None}, sort_keys=True).encode()).hexdigest()
    if (kind, digest) in _seen:
        return
    _seen.add((kind, digest))
    _records[kind].append({"case": _current_test[0], **record})


def _install() -> None:
    from litellm.accounting import snapshot
    from litellm.proxy.db import db_spend_update_writer
    from litellm.proxy.spend_tracking import native_spend_log_rows, spend_tracking_utils

    original_payload = spend_tracking_utils.get_logging_payload
    original_payload_async = spend_tracking_utils.get_logging_payload_async
    original_accounting = snapshot.build_accounting_payload
    original_update = db_spend_update_writer.DBSpendUpdateWriter.update_database

    def recording_payload(kwargs, response_obj, start_time, end_time, llm_router=None):
        record: dict = {}
        try:
            config = native_spend_log_rows.spend_log_row_config()
            row_input = native_spend_log_rows.spend_log_row_input(
                copy.copy(kwargs) if kwargs is not None else {},
                response_obj,
                start_time,
                end_time,
                llm_router,
                FIXED_NOW,
                FIXED_SESSION,
            )
            record = {"config": config, "input": row_input}
        except native_spend_log_rows.RoutedToPython as routed:
            record = {"routed": routed.reason}
        except Exception as error:  # noqa: BLE001
            record = {"input_error": f"{type(error).__name__}: {error}"}
        fixed_uuid = mock.Mock(return_value=FIXED_SESSION)
        with (
            mock.patch("time.time", return_value=FIXED_NOW),
            mock.patch.object(spend_tracking_utils_uuid(), "uuid4", fixed_uuid),
        ):
            result = original_payload(kwargs, response_obj, start_time, end_time, llm_router)
        if "input" in record:
            try:
                record["output"] = _row_text(result)
            except Unencodable as error:
                record = {"output_error": str(error)}
        _add("rows", record)
        return result

    async def recording_payload_async(kwargs, response_obj, start_time, end_time, llm_router=None, **rest):
        result = await original_payload_async(kwargs, response_obj, start_time, end_time, llm_router, **rest)
        call = _ledger_call.get()
        if call is not None:
            call["row"] = copy.deepcopy(dict(result))
        return result

    def recording_accounting(payload, *, entities, model_access_groups, windows, router):
        savings = snapshot._savings_or_zero(payload, json.loads(payload["metadata"]), router)
        result = original_accounting(
            payload, entities=entities, model_access_groups=model_access_groups, windows=windows, router=router
        )
        try:
            accounting_input = {
                "entities": _jsonable(dict(entities), lenient=True),
                "model_access_groups": _jsonable(list(model_access_groups), lenient=True),
                "windows": _jsonable(list(windows), lenient=True),
                "savings": _jsonable(savings._asdict()),
            }
            _add(
                "accounting",
                {
                    "input": {"row": _jsonable(dict(payload), lenient=True), **accounting_input},
                    "output": json.dumps(_jsonable(result, lenient=True)),
                },
            )
            call = _ledger_call.get()
            if call is not None and "row" in call:
                call["accounting"] = accounting_input
        except Unencodable as error:
            _add("accounting", {"output_error": str(error)})
        return result

    async def recording_update(
        self,
        token,
        user_id,
        end_user_id,
        team_id,
        org_id,
        kwargs,
        completion_response,
        start_time,
        end_time,
        response_cost,
        *args,
        **rest,
    ):
        call: dict = {"response_cost": response_cost, "org_id": org_id, "team_id": team_id}
        reset = _ledger_call.set(call)
        recorded: list = []
        original_record = self._record_spend_log

        async def capture(*, payload, **inner):
            recorded.append(copy.deepcopy(dict(payload)))
            return await original_record(payload=payload, **inner)

        try:
            with (
                mock.patch.object(self, "_record_spend_log", capture),
                mock.patch.object(db_spend_update_writer.uuid, "uuid4", return_value=FIXED_DELIVERY),
            ):
                result = await original_update(
                    self,
                    token,
                    user_id,
                    end_user_id,
                    team_id,
                    org_id,
                    kwargs,
                    completion_response,
                    start_time,
                    end_time,
                    response_cost,
                    *args,
                    **rest,
                )
        finally:
            _ledger_call.reset(reset)
        if "row" in call and "accounting" in call and recorded:
            try:
                _add(
                    "ledger_rows",
                    {
                        "input": {
                            "row": _jsonable(call["row"], lenient=True),
                            "response_cost": _finite_json(response_cost),
                            "org_id": org_id,
                            "team_id": team_id,
                            "delivery_id": FIXED_DELIVERY.hex,
                            **call["accounting"],
                        },
                        "output": _row_text(recorded[0]),
                    },
                )
            except Unencodable as error:
                _add("ledger_rows", {"output_error": str(error)})
        return result

    spend_tracking_utils.get_logging_payload = recording_payload
    spend_tracking_utils.get_logging_payload_async = recording_payload_async
    snapshot.build_accounting_payload = recording_accounting
    db_spend_update_writer.DBSpendUpdateWriter.update_database = recording_update


def _finite_json(value: object) -> object:
    if isinstance(value, float) and not math.isfinite(value):
        return {"non_finite": repr(value)}
    try:
        return _jsonable(value)
    except Unencodable:
        return {"unencodable": repr(value)}


def spend_tracking_utils_uuid():
    from litellm import _uuid

    return _uuid.uuid


def pytest_configure(config):
    _install()


def pytest_runtest_setup(item):
    _current_test[0] = item.nodeid


def pytest_sessionfinish(session, exitstatus):
    from litellm.proxy.spend_tracking.native_spend_log_rows import token_word_ranges

    GOLDENS.mkdir(parents=True, exist_ok=True)
    for kind, records in _records.items():
        with open(GOLDENS / f"{kind}.jsonl", "w", encoding="ascii") as out:
            for line in sorted(json.dumps(record) for record in records):
                out.write(line + "\n")
        sys.stderr.write(f"{kind}: {len(records)} records\n")
    (GOLDENS / "token_word_ranges.json").write_text(json.dumps([list(pair) for pair in token_word_ranges()]) + "\n")


if __name__ == "__main__":
    import pytest

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    sys.exit(
        pytest.main(
            [
                "-q",
                "-p",
                "no:cacheprovider",
                "-p",
                "no:xdist",
                "-p",
                "record_goldens",
                *(str(REPO / p) for p in TEST_FILES),
            ]
        )
    )
