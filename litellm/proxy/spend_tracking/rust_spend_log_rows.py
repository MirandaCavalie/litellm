import json
import os
import re
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone
from functools import lru_cache
from typing import TYPE_CHECKING, Final, Literal, TypeAlias

from pydantic import BaseModel, TypeAdapter
from typing_extensions import TypeIs  # noqa: TID251  # TypeIs reaches typing only on 3.13

import litellm
from litellm._uuid import uuid
from litellm.constants import MINIMUM_CUSTOM_KEY_LENGTH
from litellm.litellm_core_utils.classifier_logging import classifier_audit_fields
from litellm.litellm_core_utils.core_helpers import (
    get_litellm_metadata_from_kwargs,  # pyright: ignore[reportUnknownVariableType]  # the helper's dict parameter is untyped upstream
    proxy_stamped_kubernetes_pod_routing,
    proxy_stamped_used_client_oauth_token,
)
from litellm.litellm_core_utils.hidden_params import served_from_cache
from litellm.litellm_core_utils.internal_call_metadata import is_unbilled_non_inference_call
from litellm.litellm_core_utils.litellm_logging import StandardLoggingPayloadSetup
from litellm.litellm_core_utils.ptu_pricing import azure_spillover
from litellm.litellm_core_utils.redact_messages import (
    should_redact_message_logging,  # pyright: ignore[reportUnknownVariableType]  # the helper's dict parameter is untyped upstream
)
from litellm.litellm_core_utils.safe_json_dumps import safe_json_structure
from litellm.llms.anthropic.common_utils import resolve_used_client_oauth_token
from litellm.proxy._types import SpendLogsMetadata, SpendLogsPayload
from litellm.proxy.route_llm_request import ProxyModelNotFoundError
from litellm.rust_bridge import _native
from litellm.secret_managers.main import get_secret_bool
from litellm.types.utils import LiteLLMBatch, batch_chat_usage
from litellm.utils import (
    get_end_user_id_for_cost_tracking,  # pyright: ignore[reportUnknownVariableType]  # the helper's dict parameter is untyped upstream
)

if TYPE_CHECKING:
    from litellm.router import Router

SPEND_LOG_ROW_INPUT_VERSION: Final = 1
_COMPLETION_START_TIME: Final[TypeAdapter[datetime | None]] = TypeAdapter(datetime | None)
_ROW_METADATA: Final = TypeAdapter(dict[str, object])
_METADATA_KEYS: Final = frozenset(
    {
        *SpendLogsMetadata.__annotations__,
        "model_info",
        "model_group",
        "tags",
        "user_api_key_hash",
        "status",
        "session_id",
        "litellm_session_id_omitted",
        "agent_id",
        "deployment",
        "user_api_key_metadata",
        "user_api_key_team_metadata",
        "standard_logging_guardrail_information",
    }
)
_PAYLOAD_KEYS: Final = frozenset(
    {
        "id",
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "request_tags",
        "model_id",
        "model_group",
        "api_base",
        "custom_llm_provider",
        "model",
        "model_map_information",
        "guardrail_information",
        "cost_breakdown",
        "autorouter_savings",
        "autorouter_savings_estimate",
        "autorouter_baseline_observation",
        "call_type",
        "response",
        "trace_id",
        "originating_request_masked",
    }
)
_PAYLOAD_METADATA_KEYS: Final = frozenset(
    {
        "user_api_key_hash",
        "user_api_key_alias",
        "user_api_key_end_user_id",
        "is_silent_experiment",
        "applied_guardrails",
        "mcp_tool_call_metadata",
        "vector_store_request_metadata",
        "usage_object",
        "cold_storage_object_key",
    }
)
_HIDDEN_PARAMS_KEYS: Final = frozenset(
    {
        "litellm_overhead_time_ms",
        "upstream_provider",
        "batch_models",
        "batch_successful_requests",
        "batch_failed_requests",
    }
)
_LITELLM_PARAMS_KEYS: Final = ("api_base", "litellm_call_id", "proxy_server_request")
_KWARGS_KEYS: Final = (
    "model",
    "custom_llm_provider",
    "response_cost",
    "agent_id",
    "litellm_call_id",
    "litellm_trace_id",
    "realtime_tools",
    "realtime_tool_calls",
)
RouteReason: TypeAlias = Literal["message_redaction", "not_plain_data", "compaction_block"]


class RoutedToPython(Exception):
    def __init__(self, reason: RouteReason) -> None:
        super().__init__(reason)
        self.reason: Final = reason


def _is_exact_dict(value: object) -> TypeIs[Mapping[object, object]]:  # guard-ok: the exact type decides it
    return type(value) is dict


def _is_exact_list(value: object) -> TypeIs[Sequence[object]]:  # guard-ok: the exact type decides it
    return type(value) is list


def _is_mapping(value: object) -> TypeIs[Mapping[object, object]]:  # guard-ok: isinstance decides it
    return isinstance(value, Mapping)


def _is_str_mapping(value: object) -> TypeIs[Mapping[str, object]]:  # guard-ok: logging mappings are str-keyed
    return isinstance(value, Mapping)


def _is_array(value: object) -> TypeIs[Sequence[object]]:  # guard-ok: isinstance decides it
    return isinstance(value, list | tuple)


def _is_dict(value: object) -> TypeIs[dict[str, object]]:  # guard-ok: str-keyed  # mutable-ok: main's dict
    return isinstance(value, dict)


def _plain(value: object) -> object:
    if value is None or isinstance(value, bool | float) or type(value) is int:
        return value
    if isinstance(value, str) and type(value) is str:
        try:
            value.encode()
        except UnicodeEncodeError:
            raise RoutedToPython("not_plain_data") from None
        return value
    if _is_exact_dict(value):
        return {_plain_key(key): _plain(item) for key, item in value.items()}
    if _is_exact_list(value):
        return [_plain(item) for item in value]
    raise RoutedToPython("not_plain_data")


def _plain_key(key: object) -> str:
    if type(key) is not str:
        raise RoutedToPython("not_plain_data")
    return str(_plain(key))


def _projected(source: Mapping[str, object], keys: frozenset[str] | Sequence[str]) -> Mapping[str, object]:
    return {key: _plain(value) for key, value in source.items() if key in keys}


def _str_mapping(value: object) -> Mapping[str, object] | None:
    return value if _is_str_mapping(value) else None


def _routed_unless_mapping(value: object) -> Mapping[str, object]:
    mapping: Final = _str_mapping(value)
    if mapping is None:
        raise RoutedToPython("not_plain_data")
    return mapping


def _holds_compaction_block(value: object) -> bool:
    if _is_mapping(value):
        return value.get("type") == "compaction" or any(_holds_compaction_block(item) for item in value.values())
    if _is_array(value):
        return any(_holds_compaction_block(item) for item in value)
    return False


def _timestamp(value: datetime) -> Mapping[str, object]:
    return {
        "raw": value.isoformat(),
        "utc": value.astimezone(timezone.utc).isoformat(),
        "aware": value.tzinfo is not None,
    }


def _usage(
    call_type: object,
    metadata: Mapping[str, object],
    response_obj: object,
    response_obj_dict: dict[str, object],  # mutable-ok: forwarded to the OCR usage extractor, which takes a dict
) -> Mapping[str, object]:
    from litellm.proxy.spend_tracking.spend_tracking_utils import (
        _extract_usage_for_ocr_call,  # pyright: ignore[reportPrivateUsage, reportUnknownVariableType]  # the row input reuses the logging payload's own OCR usage extraction
    )

    if call_type in ["ocr", "aocr"]:
        return _str_mapping(_extract_usage_for_ocr_call(response_obj, response_obj_dict)) or {}  # pyright: ignore[reportUnknownArgumentType]  # the OCR extractor returns an untyped dict
    if is_unbilled_non_inference_call(call_type, metadata, response_obj_dict):  # pyright: ignore[reportArgumentType]  # call_type is the logged kwargs value, passed as main passes it
        return {}
    chat_usage_of_batch: Final = batch_chat_usage(response_obj) if isinstance(response_obj, LiteLLMBatch) else None
    usage: Final = (
        chat_usage_of_batch.model_dump()
        if chat_usage_of_batch is not None
        else StandardLoggingPayloadSetup.get_usage_as_dict(response_obj_dict)
    )
    if isinstance(usage, litellm.Usage):
        return dict(usage)
    return usage


def _final_usage(kwargs: Mapping[str, object], usage: Mapping[str, object]) -> Mapping[str, object]:
    combined: Final = kwargs.get("combined_usage_object")
    replaced: Final = not usage or "continued_partial_usage" in kwargs or "failed_attempts_billed_on_success" in kwargs
    if replaced and isinstance(combined, litellm.Usage):
        return combined.model_dump()
    return usage


def _usage_values(usage: Mapping[str, object]) -> Mapping[str, object]:
    return {key: value.model_dump() if isinstance(value, BaseModel) else value for key, value in usage.items()}


def spend_log_row_config() -> Mapping[str, object]:
    from litellm.proxy.proxy_server import general_settings_view

    settings: Final = general_settings_view()
    return {
        "version": SPEND_LOG_ROW_INPUT_VERSION,
        "store_prompts_in_spend_logs": _plain_setting(settings.get("store_prompts_in_spend_logs")),
        "store_prompts_in_spend_logs_env": get_secret_bool("STORE_PROMPTS_IN_SPEND_LOGS") is True,
        "spend_logs_prompt_storage": _plain_setting(settings.get("spend_logs_prompt_storage")),
        "redact_tokens_in_spend_logs": settings.get("redact_tokens_in_spend_logs") is True,
        "disable_requester_ip_address_logging": _plain_setting(settings.get("disable_requester_ip_address_logging")),
        "missing_session_id": _plain_setting(settings.get("missing_session_id")),
        "max_string_length_prompt_in_db": os.getenv("MAX_STRING_LENGTH_PROMPT_IN_DB"),
        "disable_end_user_cost_tracking": bool(litellm.disable_end_user_cost_tracking),
        "request_correlation_in_logs": bool(litellm.request_correlation_in_logs),
    }


@lru_cache(maxsize=1)
def token_word_ranges() -> tuple[tuple[int, int], ...]:
    characters: Final = "".join(map(chr, range(128, sys.maxunicode + 1)))
    return tuple((match.start() + 128, match.end() + 127) for match in re.finditer(r"\w+", characters))


def _plain_setting(value: object) -> object:
    try:
        return _plain(value)
    except RoutedToPython:
        return str(value)


def spend_log_row_input(
    kwargs: dict[str, object],  # mutable-ok: the logging kwargs as litellm hands them over, only read here
    response_obj: object,
    start_time: datetime,
    end_time: datetime,
    llm_router: "Router | None",
    now: float,
    fallback_session_id: str,
) -> Mapping[str, object]:
    from litellm.proxy.spend_tracking.spend_tracking_utils import (
        _caller_session_for_spend_log,  # pyright: ignore[reportPrivateUsage]  # the row input reuses the logging payload's own lookups
        _is_configured_model_group,  # pyright: ignore[reportPrivateUsage]  # the row input reuses the logging payload's own lookups
        _model_group_provider,  # pyright: ignore[reportPrivateUsage]  # the row input reuses the logging payload's own lookups
        _string_or_empty,  # pyright: ignore[reportPrivateUsage]  # the row input reuses the logging payload's own lookups
        get_spend_logs_id,
        should_store_prompts_and_responses_in_spend_logs,
    )

    rejected_as_unknown_model: Final = isinstance(response_obj, ProxyModelNotFoundError)
    unknown_model_error_message: Final = (
        response_obj.spend_log_error_message if isinstance(response_obj, ProxyModelNotFoundError) else None
    )
    response_served_from_cache: Final = served_from_cache(response_obj)
    response_obj_dict: Final[dict[str, object]] = (  # mutable-ok: the response dict main's extractors take
        {}
        if response_obj is None
        else response_obj
        if _is_dict(response_obj)
        else response_obj.model_dump()
        if isinstance(response_obj, BaseModel)
        else {"result": str(response_obj)}
    )
    response: Final = response_obj if isinstance(response_obj, BaseModel) else response_obj_dict
    litellm_params: Final = _routed_unless_mapping(kwargs.get("litellm_params", {}))
    metadata: Final = _routed_unless_mapping(get_litellm_metadata_from_kwargs(kwargs))  # pyright: ignore[reportUnknownArgumentType]  # the metadata helper returns an untyped dict
    routing_metadata: Final = litellm_params.get("metadata")
    completion_start_time: Final = _COMPLETION_START_TIME.validate_python(kwargs.get("completion_start_time", end_time))
    call_type: Final = kwargs.get("call_type")
    usage: Final = _final_usage(kwargs, _usage(call_type, metadata, response, response_obj_dict))
    request_id: Final = get_spend_logs_id(
        call_type or "acompletion",  # pyright: ignore[reportArgumentType]  # passed as the logging payload passes it
        response_obj_dict,
        kwargs,
        metadata,
    )
    payload: Final = _str_mapping(kwargs.get("standard_logging_object", None))

    stores_prompts: Final = should_store_prompts_and_responses_in_spend_logs(metadata)
    if stores_prompts and should_redact_message_logging(
        model_call_details={
            "litellm_params": litellm_params,
            "standard_callback_dynamic_params": kwargs.get("standard_callback_dynamic_params"),
        }
    ):
        raise RoutedToPython("message_redaction")
    proxy_server_request: Final = litellm_params.get("proxy_server_request")
    if stores_prompts and (
        _holds_compaction_block(payload.get("response") if payload is not None else None)
        or _holds_compaction_block(proxy_server_request)
    ):
        raise RoutedToPython("compaction_block")

    model_group: Final = _string_or_empty(metadata.get("model_group")) or (
        _string_or_empty(payload.get("model_group")) if payload is not None else ""
    )
    logged_provider: Final = (
        kwargs.get("custom_llm_provider")
        or (_string_or_empty(payload.get("custom_llm_provider")) if payload is not None else "")
        or None
    )
    model_group_provider: Final = None if logged_provider else _model_group_provider(model_group, llm_router)
    custom_llm_provider: Final = logged_provider or model_group_provider
    payload_metadata: Final = _str_mapping(payload.get("metadata")) if payload is not None else None
    hidden_params: Final = _str_mapping(payload.get("hidden_params")) if payload is not None else None
    response_headers: Final = _str_mapping(kwargs.get("response_headers"))

    cache_key: Final = (
        "Cache OFF"
        if litellm.cache is None
        else litellm_params["preset_cache_key"]
        if litellm_params.get("preset_cache_key") is not None
        else litellm.cache.get_cache_key(**kwargs)
    )

    return {
        "version": SPEND_LOG_ROW_INPUT_VERSION,
        "call_type": _plain(call_type),
        "cache_hit": _plain(kwargs.get("cache_hit", False)),
        "served_from_cache": response_served_from_cache,
        "rejected_as_unknown_model": rejected_as_unknown_model,
        "unknown_model_error_message": unknown_model_error_message,
        "request_id": _plain(request_id),
        "now": now,
        "fallback_session_id": fallback_session_id,
        "start_time": _timestamp(start_time),
        "end_time": _timestamp(end_time),
        "completion_start_time": _timestamp(completion_start_time) if completion_start_time is not None else None,
        "usage": _plain(_usage_values(usage)),
        "metadata": _projected(metadata, _METADATA_KEYS),
        "litellm_params": _projected(litellm_params, _LITELLM_PARAMS_KEYS),
        "kwargs": _projected(kwargs, _KWARGS_KEYS),
        "standard_logging_object": None
        if payload is None
        else {
            **_projected(payload, _PAYLOAD_KEYS),
            **(
                {"messages": _plain(payload.get("messages"))}
                if "messages" in payload and payload.get("call_type") == "_arealtime"
                else {}
            ),
            **(
                {"metadata": _projected(payload_metadata, _PAYLOAD_METADATA_KEYS)}
                if payload_metadata is not None
                else {}
            ),
            **({"hidden_params": _projected(hidden_params, _HIDDEN_PARAMS_KEYS)} if hidden_params is not None else {}),
        },
        "cache_key": _plain(cache_key),
        "end_user_id": _plain(get_end_user_id_for_cost_tracking(dict(litellm_params))),
        "model_group_provider": model_group_provider,
        "is_configured_model_group": _is_configured_model_group(model_group, llm_router)
        if metadata.get("status") == "failure"
        else False,
        "caller_session": _caller_session_for_spend_log(kwargs, metadata)
        if litellm.request_correlation_in_logs
        else None,
        "used_client_oauth_token": resolve_used_client_oauth_token(
            proxy_stamped_used_client_oauth_token(litellm_params.get("metadata"), litellm_params),
            custom_llm_provider,  # pyright: ignore[reportArgumentType]  # passed as the logging payload passes it
        ),
        "kubernetes_pod_routing": _plain(proxy_stamped_kubernetes_pod_routing(routing_metadata, litellm_params)),
        "azure_spillover": _plain(
            azure_spillover(
                response_headers=response_headers,
                additional_headers=hidden_params.get("additional_headers")  # pyright: ignore[reportArgumentType]  # passed as the logging payload passes it
                if hidden_params is not None
                else None,
            )
        ),
        "classifier_audit": _plain(dict(classifier_audit_fields(payload))) if payload is not None else None,
    }


def current_spend_log_row_input(
    kwargs: dict[str, object],  # mutable-ok: the logging kwargs as litellm hands them over, only read here
    response_obj: object,
    start_time: datetime,
    end_time: datetime,
    llm_router: "Router | None",
) -> Mapping[str, object]:
    return spend_log_row_input(kwargs, response_obj, start_time, end_time, llm_router, time.time(), str(uuid.uuid4()))


def encode_row_input(value: Mapping[str, object]) -> str:
    return json.dumps(value)


def decode_row(row_json: str) -> SpendLogsPayload:
    row: Final[SpendLogsPayload] = json.loads(row_json)  # pyright: ignore[reportAny]  # the native builder returns the row as a JSON object
    start_time: Final = row["startTime"]
    end_time: Final = row["endTime"]
    completion_start_time: Final = row["completionStartTime"]
    if isinstance(start_time, str):
        row["startTime"] = datetime.fromisoformat(start_time)
    if isinstance(end_time, str):
        row["endTime"] = datetime.fromisoformat(end_time)
    if isinstance(completion_start_time, str):
        row["completionStartTime"] = datetime.fromisoformat(completion_start_time)
    return row


@dataclass(frozen=True, slots=True)
class LedgerRowInput:
    billable_cost: float | None
    org_id: str | None
    team_id: str | None
    delivery_id: str
    entities: Mapping[str, str | None]
    model_access_groups: Sequence[str]
    windows: Sequence[Mapping[str, object]]
    router: "Callable[[], Router | None]"


def uses_spend_logs_ledger() -> bool:
    from litellm.proxy.proxy_server import general_settings_view

    return general_settings_view().get("use_spend_logs_ledger", True) is not False


@lru_cache(maxsize=8)
def _row_builder(settings_json: str) -> _native.NativeSpendLogRowBuilder:
    return _native.NativeSpendLogRowBuilder(settings_json, MINIMUM_CUSTOM_KEY_LENGTH, token_word_ranges())


def native_logging_payload(
    kwargs: dict[str, object],  # mutable-ok: the logging kwargs as litellm hands them over, only read here
    response_obj: object,
    start_time: datetime,
    end_time: datetime,
    llm_router: "Router | None",
) -> SpendLogsPayload | None:
    try:
        row_input: Final = current_spend_log_row_input(kwargs, response_obj, start_time, end_time, llm_router)
    except RoutedToPython:
        return None
    return decode_row(_row_builder(json.dumps(spend_log_row_config())).build_row(json.dumps(row_input)))


def _json_structure(value: object) -> object:
    return json.loads(json.dumps(safe_json_structure(value)))  # pyright: ignore[reportAny]  # a JSON value read back as object


def _ledger_row_plain(payload: Mapping[str, object]) -> Mapping[str, object]:
    return {key: value.isoformat() if isinstance(value, datetime) else _plain(value) for key, value in payload.items()}


def _python_ledger_row(payload: SpendLogsPayload, ledger: LedgerRowInput) -> SpendLogsPayload:
    from litellm.accounting.snapshot import build_accounting_payload
    from litellm.proxy.db.db_spend_update_writer import (
        _is_settle_once_cost_row,  # pyright: ignore[reportPrivateUsage]  # the fallback reuses the writer's own settle-once rule
        _iso_timestamp,  # pyright: ignore[reportPrivateUsage]  # the fallback reuses the writer's own timestamp form
    )

    row: Final = payload.copy()
    row["spend"] = ledger.billable_cost or 0.0
    row["startTime"] = _iso_timestamp(payload["startTime"])
    row["endTime"] = _iso_timestamp(payload["endTime"])
    if ledger.org_id:
        row["organization_id"] = ledger.org_id
    if ledger.team_id:
        row["team_id"] = ledger.team_id
    if not _is_settle_once_cost_row(row):
        row["accounting_delivery_id"] = ledger.delivery_id
    row["accounting_payload"] = build_accounting_payload(
        row,
        entities=ledger.entities,
        model_access_groups=ledger.model_access_groups,
        windows=ledger.windows,
        router=ledger.router,
    )
    return row


def ledger_spend_log_row(payload: SpendLogsPayload, ledger: LedgerRowInput) -> SpendLogsPayload:
    from litellm.accounting.snapshot import (
        _savings_or_zero,  # pyright: ignore[reportPrivateUsage]  # savings pricing stays in Python and feeds the native row
    )

    try:
        row: Final = _ledger_row_plain(payload)
    except RoutedToPython:
        return _python_ledger_row(payload, ledger)
    ledger_input: Final = {
        "response_cost": ledger.billable_cost,
        "org_id": ledger.org_id,
        "team_id": ledger.team_id,
        "delivery_id": ledger.delivery_id,
        "entities": _json_structure(dict(ledger.entities)),
        "model_access_groups": _json_structure(list(ledger.model_access_groups)),
        "windows": _json_structure(list(ledger.windows)),
        "savings": _savings_or_zero(payload, _ROW_METADATA.validate_json(payload["metadata"]), ledger.router)._asdict(),
    }
    finalized: Final[SpendLogsPayload] = json.loads(  # pyright: ignore[reportAny]  # the native builder returns the row as a JSON object
        _native.NativeSpendLogRowBuilder.finalize_ledger_row(json.dumps(row), json.dumps(ledger_input))
    )
    if "completionStartTime" in payload:
        finalized["completionStartTime"] = payload["completionStartTime"]
    return finalized
