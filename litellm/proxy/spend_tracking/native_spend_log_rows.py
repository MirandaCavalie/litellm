import json
import os
import re
import sys
import time
from collections.abc import Mapping, Sequence
from datetime import datetime, timezone
from functools import lru_cache
from typing import TYPE_CHECKING, Final, Literal

from pydantic import BaseModel, TypeAdapter

import litellm
from litellm._uuid import uuid
from litellm.litellm_core_utils.classifier_logging import classifier_audit_fields
from litellm.litellm_core_utils.core_helpers import (
    get_litellm_metadata_from_kwargs,
    proxy_stamped_kubernetes_pod_routing,
    proxy_stamped_used_client_oauth_token,
)
from litellm.litellm_core_utils.hidden_params import served_from_cache
from litellm.litellm_core_utils.internal_call_metadata import is_unbilled_non_inference_call
from litellm.litellm_core_utils.litellm_logging import StandardLoggingPayloadSetup
from litellm.litellm_core_utils.ptu_pricing import azure_spillover
from litellm.litellm_core_utils.redact_messages import should_redact_message_logging
from litellm.llms.anthropic.common_utils import resolve_used_client_oauth_token
from litellm.proxy._types import SpendLogsMetadata, SpendLogsPayload
from litellm.proxy.route_llm_request import ProxyModelNotFoundError
from litellm.secret_managers.main import get_secret_bool
from litellm.types.utils import LiteLLMBatch, batch_chat_usage
from litellm.utils import get_end_user_id_for_cost_tracking

if TYPE_CHECKING:
    from litellm.router import Router

SPEND_LOG_ROW_INPUT_VERSION: Final = 1
_DATETIME_ROW_FIELDS: Final = ("startTime", "endTime", "completionStartTime")
_COMPLETION_START_TIME: Final[TypeAdapter[datetime | None]] = TypeAdapter(datetime | None)
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
RouteReason = Literal["message_redaction", "not_plain_data", "compaction_block"]


class RoutedToPython(Exception):
    def __init__(self, reason: RouteReason) -> None:
        super().__init__(reason)
        self.reason: Final = reason


def _plain(value: object) -> object:
    match value:
        case None | bool() | float():
            return value
        case int() if type(value) is int:
            return value
        case str() if type(value) is str:
            try:
                value.encode()
            except UnicodeEncodeError:
                raise RoutedToPython("not_plain_data") from None
            return value
        case dict() if type(value) is dict:
            return {_plain_key(key): _plain(item) for key, item in value.items()}
        case list() if type(value) is list:
            return [_plain(item) for item in value]
    raise RoutedToPython("not_plain_data")


def _plain_key(key: object) -> str:
    if type(key) is not str:
        raise RoutedToPython("not_plain_data")
    return str(_plain(key))


def _projected(source: Mapping[str, object], keys: frozenset[str] | Sequence[str]) -> dict[str, object]:
    return {key: _plain(value) for key, value in source.items() if key in keys}


def _holds_compaction_block(value: object) -> bool:
    if isinstance(value, Mapping):
        return value.get("type") == "compaction" or any(_holds_compaction_block(item) for item in value.values())
    if isinstance(value, list | tuple):
        return any(_holds_compaction_block(item) for item in value)
    return False


def _timestamp(value: datetime) -> Mapping[str, object]:
    return {
        "raw": value.isoformat(),
        "utc": value.astimezone(timezone.utc).isoformat(),
        "aware": value.tzinfo is not None,
    }


def _usage(call_type: object, metadata: Mapping[str, object], response_obj: object, response_obj_dict: dict) -> dict:
    from litellm.proxy.spend_tracking.spend_tracking_utils import _extract_usage_for_ocr_call

    if call_type in ["ocr", "aocr"]:
        return _extract_usage_for_ocr_call(response_obj, response_obj_dict)
    if is_unbilled_non_inference_call(call_type, metadata, response_obj_dict):  # pyright: ignore[reportArgumentType]
        return {}
    chat_usage_of_batch: Final = batch_chat_usage(response_obj) if isinstance(response_obj, LiteLLMBatch) else None
    usage: Final = (
        chat_usage_of_batch.model_dump()
        if chat_usage_of_batch is not None
        else StandardLoggingPayloadSetup.get_usage_as_dict(response_obj_dict)
    )
    if isinstance(usage, litellm.Usage):
        return dict(usage)
    return usage if isinstance(usage, dict) else {}


def _final_usage(kwargs: Mapping[str, object], usage: dict) -> dict:
    combined: Final = kwargs.get("combined_usage_object")
    if (
        not usage or "continued_partial_usage" in kwargs or "failed_attempts_billed_on_success" in kwargs
    ) and isinstance(combined, litellm.Usage):
        return combined.model_dump()
    return usage


def _usage_values(usage: Mapping[str, object]) -> dict[str, object]:
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
    kwargs: dict,
    response_obj: object,
    start_time: datetime,
    end_time: datetime,
    llm_router: "Router | None",
    now: float,
    fallback_session_id: str,
) -> Mapping[str, object]:
    from litellm.proxy.spend_tracking.spend_tracking_utils import (
        _caller_session_for_spend_log,
        _is_configured_model_group,
        _model_group_provider,
        _string_or_empty,
        get_spend_logs_id,
        should_store_prompts_and_responses_in_spend_logs,
    )

    rejected_as_unknown_model: Final = isinstance(response_obj, ProxyModelNotFoundError)
    unknown_model_error_message: Final = (
        response_obj.spend_log_error_message if isinstance(response_obj, ProxyModelNotFoundError) else None
    )
    response_served_from_cache: Final = served_from_cache(response_obj)
    if response_obj is None:
        response_obj = {}
    elif not isinstance(response_obj, BaseModel) and not isinstance(response_obj, dict):
        response_obj = {"result": str(response_obj)}
    litellm_params: Final = kwargs.get("litellm_params", {})
    metadata: Final = get_litellm_metadata_from_kwargs(kwargs)
    routing_litellm_params: Final[Mapping[str, object] | None] = (
        litellm_params if isinstance(litellm_params, Mapping) else None
    )
    routing_metadata: Final[object] = (
        routing_litellm_params.get("metadata") if routing_litellm_params is not None else None
    )
    completion_start_time: Final = _COMPLETION_START_TIME.validate_python(kwargs.get("completion_start_time", end_time))
    call_type: Final = kwargs.get("call_type")
    response_obj_dict: Final = (
        response_obj
        if isinstance(response_obj, dict)
        else response_obj.model_dump()
        if isinstance(response_obj, BaseModel)
        else {}
    )
    usage: Final = _final_usage(kwargs, _usage(call_type, metadata, response_obj, response_obj_dict))
    request_id: Final = get_spend_logs_id(call_type or "acompletion", response_obj_dict, kwargs, metadata)
    standard_logging_payload: Final = kwargs.get("standard_logging_object", None)
    payload: Final = standard_logging_payload if isinstance(standard_logging_payload, Mapping) else None

    stores_prompts: Final = should_store_prompts_and_responses_in_spend_logs(metadata)
    if stores_prompts and should_redact_message_logging(
        model_call_details={
            "litellm_params": litellm_params,
            "standard_callback_dynamic_params": kwargs.get("standard_callback_dynamic_params"),
        }
    ):
        raise RoutedToPython("message_redaction")
    proxy_server_request: Final = (
        litellm_params.get("proxy_server_request") if isinstance(litellm_params, Mapping) else None
    )
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
    payload_metadata: Final = payload.get("metadata") if payload is not None else None
    hidden_params: Final = payload.get("hidden_params") if payload is not None else None
    response_headers: Final = kwargs.get("response_headers")

    if litellm.cache is None:
        cache_key: object = "Cache OFF"
    elif litellm_params.get("preset_cache_key") is not None:
        cache_key = litellm_params["preset_cache_key"]
    else:
        cache_key = litellm.cache.get_cache_key(**kwargs)

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
                if isinstance(payload_metadata, Mapping)
                else {}
            ),
            **(
                {"hidden_params": _projected(hidden_params, _HIDDEN_PARAMS_KEYS)}
                if isinstance(hidden_params, Mapping)
                else {}
            ),
        },
        "cache_key": _plain(cache_key),
        "end_user_id": _plain(
            get_end_user_id_for_cost_tracking(
                dict(litellm_params) if isinstance(litellm_params, Mapping) else litellm_params
            )
        ),
        "model_group_provider": model_group_provider,
        "is_configured_model_group": _is_configured_model_group(model_group, llm_router)
        if metadata.get("status") == "failure"
        else False,
        "caller_session": _caller_session_for_spend_log(kwargs, metadata)
        if litellm.request_correlation_in_logs
        else None,
        "used_client_oauth_token": resolve_used_client_oauth_token(
            proxy_stamped_used_client_oauth_token(litellm_params.get("metadata"), litellm_params), custom_llm_provider
        ),
        "kubernetes_pod_routing": _plain(
            proxy_stamped_kubernetes_pod_routing(routing_metadata, routing_litellm_params)
        ),
        "azure_spillover": _plain(
            azure_spillover(
                response_headers=response_headers if isinstance(response_headers, Mapping) else None,
                additional_headers=hidden_params.get("additional_headers")
                if isinstance(hidden_params, Mapping)
                else None,
            )
        ),
        "classifier_audit": _plain(dict(classifier_audit_fields(payload))) if payload is not None else None,
    }


def current_spend_log_row_input(
    kwargs: dict, response_obj: object, start_time: datetime, end_time: datetime, llm_router: "Router | None"
) -> Mapping[str, object]:
    return spend_log_row_input(kwargs, response_obj, start_time, end_time, llm_router, time.time(), str(uuid.uuid4()))


def encode_row_input(value: Mapping[str, object]) -> str:
    return json.dumps(value)


def decode_row(row_json: str) -> SpendLogsPayload:
    row: Final = json.loads(row_json)
    for field in _DATETIME_ROW_FIELDS:
        value = row.get(field)
        if isinstance(value, str):
            row[field] = datetime.fromisoformat(value)
    return row
