import time
import traceback
from collections.abc import Mapping, Sequence
from datetime import datetime, timezone
from typing import Final, Literal

import litellm
from litellm._logging import redact_string
from litellm._uuid import uuid
from litellm.constants import BACKGROUND_INTERACTION_COST_POLLING_ENABLED
from litellm.integrations.custom_logger_v2.field_sources import HookArgs
from litellm.litellm_core_utils.batch_utils import defers_batch_cost, repeat_poll_cost
from litellm.litellm_core_utils.core_helpers import (
    budget_reservation_from_metadata,
    finite_cost,
    get_litellm_metadata_from_kwargs,  # pyright: ignore[reportUnknownVariableType]  # the helper's dict parameter is untyped upstream
)
from litellm.litellm_core_utils.litellm_logging import StandardLoggingPayloadSetup
from litellm.proxy._types import UserAPIKeyAuth
from litellm.proxy.auth.route_checks import RouteChecks
from litellm.proxy.db.db_lookup_gate import DBLookupDeadlineExceeded
from litellm.proxy.db.db_spend_update_writer import get_llm_router
from litellm.proxy.hooks.proxy_track_cost_callback import (
    _CAPTURED_IDENTITY_CALL_TYPES,  # pyright: ignore[reportPrivateUsage]  # the cost callback's own call type sets
    _UNATTRIBUTED_TRACKABLE_CALL_TYPES,  # pyright: ignore[reportPrivateUsage]  # the cost callback's own call type sets
    ProxyDBLogger,
    _committed_spend_scope,  # pyright: ignore[reportPrivateUsage]  # the failure writer's committed twins
    _failure_snapshot_with_guardrails_recorded_after_it,  # pyright: ignore[reportPrivateUsage]  # the failure writer's own rows
    _failure_spend_rows,  # pyright: ignore[reportPrivateUsage]  # the failure writer's own rows
    _get_request_tags_for_cost_tracking,  # pyright: ignore[reportPrivateUsage, reportUnknownVariableType]  # the cost callback's tag read
    _guardrail_info_lifted_from_litellm_metadata,  # pyright: ignore[reportPrivateUsage]  # the failure writer's own metadata
    _is_unbilled_in_progress_interaction,  # pyright: ignore[reportPrivateUsage]  # the cost callback's interaction checks
    _is_unbilled_interaction_response,  # pyright: ignore[reportPrivateUsage]  # the cost callback's interaction checks
    _proxy_deployment,  # pyright: ignore[reportPrivateUsage]  # the failure writer's deployment lookup
    _proxy_stamped_request_fields,  # pyright: ignore[reportPrivateUsage]  # the failure writer's own metadata
    _proxy_stamped_used_client_oauth_token,  # pyright: ignore[reportPrivateUsage]  # the failure writer's own metadata
    _redaction_opt_in_lifted_from_litellm_metadata,  # pyright: ignore[reportPrivateUsage]  # the failure writer's own metadata
    _routed_metadata_fallback,  # pyright: ignore[reportPrivateUsage]  # the failure writer's own metadata
    failure_row_response_cost,
    is_pre_routing_rejection,
)
from litellm.proxy.litellm_pre_call_utils import LiteLLMProxyRequestSetup
from litellm.proxy.spend_tracking.rust_spend_log_rows import spend_log_row_config, spend_log_row_input
from litellm.proxy.spend_tracking.spend_log_error_logger import should_suppress_spend_log_tracebacks
from litellm.proxy.spend_tracking.spend_tracking_utils import get_request_model_access_groups
from litellm.proxy.utils import ProxyUpdateSpend
from litellm.types.proxy.auth.customer_hierarchy import (
    customer_budget_scopes_from_request,
    customer_hierarchy_ids_from_request,
)
from litellm.types.proxy.auth.user_api_key_auth import committed_budget_spend_of_row
from litellm.utils import (
    get_end_user_id_for_cost_tracking,  # pyright: ignore[reportUnknownVariableType]  # the helper's dict parameter is untyped upstream
)

Disposition = Literal[
    "charge", "defer_batch", "interaction_in_progress", "interaction_unbilled", "unpriced", "released"
]


def _local_now() -> datetime:
    return datetime.now(timezone.utc).astimezone().replace(tzinfo=None)


def _mapping(value: object) -> Mapping[str, object]:
    return value if isinstance(value, Mapping) else {}  # pyright: ignore[reportUnknownVariableType]  # logging dicts are str-keyed


def _text(value: object) -> str | None:
    return value if isinstance(value, str) else None


def _settings() -> Mapping[str, object]:
    from litellm.proxy.proxy_server import (
        disable_spend_logs,  # pyright: ignore[reportUnknownVariableType]  # the legacy untyped global
        general_settings_view,
        litellm_proxy_budget_name,
    )

    general: Final = general_settings_view()
    return {
        "disable_spend_updates": ProxyUpdateSpend.disable_spend_updates() is True,
        "disable_spend_logs": disable_spend_logs is True,
        "disable_error_logs": general.get("disable_error_logs") is True,
        "disable_responses_id_security": general.get("disable_responses_id_security") is True,
        "row_config": spend_log_row_config(),
        "proxy_budget_name": litellm_proxy_budget_name if litellm.max_budget > 0 else None,
    }


def _window_resets(reservation: object) -> Sequence[str | None]:
    def reset(entry: object) -> str | None:
        value: Final = _mapping(entry).get("window_reset_at")
        return value.isoformat() if isinstance(value, datetime) else _text(value)

    entries: Final = _mapping(reservation).get("entries")
    return [reset(entry) for entry in entries] if isinstance(entries, list) else []  # pyright: ignore[reportUnknownVariableType, reportUnknownArgumentType]  # the reservation's entry dicts


def _carried_committed(kwargs: Mapping[str, object]) -> Sequence[str] | None:
    carried: Final = committed_budget_spend_of_row(
        _mapping(get_litellm_metadata_from_kwargs(dict(kwargs))),  # pyright: ignore[reportUnknownArgumentType]  # the metadata helper returns an untyped dict
        _mapping(kwargs.get("litellm_params")),
    )
    return None if carried is None else list(carried.counter_keys.tracked())


def _customer_hierarchy(kwargs: Mapping[str, object]) -> Sequence[Mapping[str, object]]:
    from litellm.proxy.spend_tracking.rust_spend_counters import (
        _customer_scopes,  # pyright: ignore[reportPrivateUsage]  # the counter step's own hierarchy reader
    )

    metadata: Final = _mapping(get_litellm_metadata_from_kwargs(dict(kwargs)))  # pyright: ignore[reportUnknownArgumentType]  # the metadata helper returns an untyped dict
    return list(_customer_scopes(metadata.get("user_api_key_customer_hierarchy")))


def _row(
    kwargs: Mapping[str, object],
    response: object,
    start_time: datetime,
    end_time: datetime,
    cost: float | None,
    committed: Sequence[str] | None,
) -> Mapping[str, object]:
    groups: Final = get_request_model_access_groups(kwargs)
    customer: Final = customer_hierarchy_ids_from_request(kwargs)
    scopes: Final = customer_budget_scopes_from_request(kwargs)
    try:
        row: object = spend_log_row_input(
            dict(kwargs),
            response,
            start_time,
            end_time,
            get_llm_router(),
            time.time(),
            str(uuid.uuid4()),  # pyright: ignore[reportUnknownMemberType, reportUnknownArgumentType]  # litellm's uuid shim is untyped
        )
    except Exception as error:  # noqa: BLE001  # update_database logs a row it cannot build and still charges it
        row = {"unbuildable": str(error)}
    return {
        "row": row,
        "response_cost": cost,
        "model_access_groups": list(groups or ()),
        "customer_entities": None if customer is None else dict(customer.entities()),
        "customer_scopes": None if scopes is None else [scope.model_dump(mode="json") for scope in scopes],
        "committed_tracked": committed,
        "customer_hierarchy": _customer_hierarchy(kwargs),
    }


def _disposition(kwargs: Mapping[str, object], response: object, cost: float | None, payload: object) -> Disposition:
    if defers_batch_cost(kwargs, response):
        return "defer_batch"
    if cost is not None:
        return "charge"
    if _is_unbilled_interaction_response(response):
        if BACKGROUND_INTERACTION_COST_POLLING_ENABLED and _is_unbilled_in_progress_interaction(response):
            return "interaction_in_progress"
        return "interaction_unbilled"
    if payload is None and (not kwargs.get("model") or kwargs.get("call_type") == "_arealtime"):
        return "released"
    if kwargs.get("stream") is True and "complete_streaming_response" not in kwargs:
        return "released"
    return "unpriced"


def spend_log_success_input(args: HookArgs) -> Mapping[str, object] | None:
    kwargs: Final = _mapping(args.request)
    if not kwargs or not isinstance(args.start_time, datetime) or not isinstance(args.end_time, datetime):
        return None
    response: Final = (
        kwargs.get("async_complete_streaming_response")
        if getattr(args.logging_obj, "stream", None) is True
        else args.result
    )
    metadata: Final = _mapping(get_litellm_metadata_from_kwargs(dict(kwargs)))  # pyright: ignore[reportUnknownArgumentType]  # the metadata helper returns an untyped dict
    payload: Final = kwargs.get("standard_logging_object")
    cost: Final = finite_cost(_mapping(payload if payload is not None else kwargs).get("response_cost"))
    call_type: Final = _text(kwargs.get("call_type"))
    end_user_max_budget: Final = metadata.get("user_api_end_user_max_budget")
    return {
        "kind": "success",
        "settings": _settings(),
        "identity": {
            "token": _text(metadata.get("user_api_key")),
            "user_id": _text(metadata.get("user_api_key_user_id")),
            "team_id": _text(metadata.get("user_api_key_team_id")),
            "org_id": _text(metadata.get("user_api_key_org_id")),
            "project_id": _text(metadata.get("user_api_key_project_id")),
            "end_user_id": get_end_user_id_for_cost_tracking(dict(_mapping(kwargs.get("litellm_params")))),
            "key_alias": _text(metadata.get("user_api_key_alias")),
            "agent_id": _text(metadata.get("billing_agent_id") or metadata.get("agent_id")),
        },
        "enrich": {"resolve_missing_key_identity": str(call_type) not in _CAPTURED_IDENTITY_CALL_TYPES}
        if metadata.get("user_api_key") and not metadata.get("user_api_key_user_id")
        else None,
        "disposition": _disposition(kwargs, response, cost, payload),
        "call_type": call_type,
        "tracks_unattributed": call_type in _UNATTRIBUTED_TRACKABLE_CALL_TYPES,
        "tags": _get_request_tags_for_cost_tracking(payload, dict(metadata)),  # pyright: ignore[reportArgumentType]  # the logged payload dict
        "end_user_max_budget": float(end_user_max_budget) if isinstance(end_user_max_budget, int | float) else None,
        "repeat_poll_cost": repeat_poll_cost(call_type, cost, payload) if cost is not None else 0.0,
        "window_resets": _window_resets(budget_reservation_from_metadata(dict(metadata))),
        "row": _row(kwargs, response, args.start_time, args.end_time, cost, _carried_committed(kwargs)),
    }


def _route_is_trackable(request_route: str | None) -> bool:
    from litellm.proxy.pass_through_endpoints.pass_through_endpoints import InitPassThroughEndpointHelpers

    return (
        request_route is None
        or RouteChecks.runs_inference_checks(request_route)
        or RouteChecks.is_info_route(route=request_route)
        or InitPassThroughEndpointHelpers.has_configured_route_ignoring_trailing_slash(route=request_route)
    )


def _tracks_failure(
    auth: UserAPIKeyAuth, request: Mapping[str, object], exception: Exception, completed_cost: bool
) -> bool:
    request_route: Final = auth.request_route
    return not (
        ProxyDBLogger._should_track_errors_in_db() is False  # pyright: ignore[reportPrivateUsage]  # the failure writer's own setting read
        and not completed_cost
        or ProxyDBLogger._skips_pre_routing_rejections()  # pyright: ignore[reportPrivateUsage]  # the failure writer's own setting read
        and is_pre_routing_rejection(exception, request)
        or not _route_is_trackable(request_route)
        or request_route is not None
        and RouteChecks.is_dashboard_session_info_route(request_route, auth.team_id)
    )


def _failure_metadata(auth: UserAPIKeyAuth, exception: Exception) -> Mapping[str, object]:
    error_information: Final = dict(
        StandardLoggingPayloadSetup.get_error_information(
            original_exception=exception,
            traceback_str=redact_string("".join(traceback.format_exception(exception))),
        )
    )
    return {
        **LiteLLMProxyRequestSetup.get_sanitized_user_information_from_key(user_api_key_dict=auth),
        "user_api_key": LiteLLMProxyRequestSetup.get_logged_api_key(auth),
        **LiteLLMProxyRequestSetup.get_tenant_logging_metadata(auth),
        "status": "failure",
        "error_information": {
            key: value
            for key, value in error_information.items()
            if key != "traceback" or not should_suppress_spend_log_tracebacks()
        },
    }


def _failure_request(request: Mapping[str, object], auth: UserAPIKeyAuth, exception: Exception) -> Mapping[str, object]:
    """The request dict ``async_post_call_failure_hook`` writes its rows from, built on a copy."""
    metadata: Final = _failure_metadata(auth, exception)
    request_route: Final = auth.request_route
    litellm_metadata: Final = _mapping(request.get("litellm_metadata"))
    stamped: Final = {
        **_mapping(request.get("metadata")),
        **_routed_metadata_fallback(request),
        **_proxy_stamped_request_fields(request, request_route),
        **metadata,
        "used_client_oauth_token": _proxy_stamped_used_client_oauth_token(request, request_route),
    }
    with_guardrails: Final = {
        **stamped,
        **_guardrail_info_lifted_from_litellm_metadata(
            litellm_metadata, guardrail_info_already_set="standard_logging_guardrail_information" in stamped
        ),
    }
    params: Final = _mapping(request.get("litellm_params"))
    params_metadata: Final = _mapping(params.get("metadata"))
    existing: Final = {**with_guardrails, **({"tags": params_metadata["tags"]} if params_metadata.get("tags") else {})}
    logging_obj: Final = request.get("litellm_logging_obj")
    return {
        **request,
        "litellm_params": {
            **params,
            "proxy_server_request": request.get("proxy_server_request") or params.get("proxy_server_request") or {},
            "metadata": {
                **existing,
                **_redaction_opt_in_lifted_from_litellm_metadata(
                    litellm_metadata, caller_set_headers="headers" in existing
                ),
            },
        },
        "metadata": existing,
        "model": request["model"] if "model" in request else params.get("model") or "",
        "custom_llm_provider": request["custom_llm_provider"]
        if "custom_llm_provider" in request
        else params.get("custom_llm_provider") or "",
        "standard_logging_object": request.get("standard_logging_object")
        or (
            None
            if logging_obj is None
            else _mapping(getattr(logging_obj, "model_call_details", {})).get("standard_logging_object")
        ),
        "litellm_trace_id": request.get("litellm_trace_id")
        if request.get("litellm_trace_id") is not None or logging_obj is None
        else getattr(logging_obj, "litellm_trace_id", None),
    }


def _failure_start(request: Mapping[str, object]) -> datetime:
    lifted: Final = request.get("start_time")
    if isinstance(lifted, datetime):
        return lifted
    started: Final = getattr(request.get("litellm_logging_obj"), "start_time", None)
    return started if isinstance(started, datetime) else _local_now()


def spend_log_failure_input(args: HookArgs) -> Mapping[str, object] | None:
    request: Final = _mapping(args.request)
    auth: Final = args.auth
    exception: Final = args.result
    if not isinstance(auth, UserAPIKeyAuth) or not isinstance(exception, Exception):
        return None
    completed_cost: Final = _mapping(request.get("standard_logging_object")).get("cost_finalized") is True
    track: Final = _tracks_failure(auth, request, exception, completed_cost)
    built: Final = _failure_request(request, auth, exception) if track else None
    start_time: Final = _failure_start(request)
    end_time: Final = datetime.now(timezone.utc) if completed_cost else _local_now()
    rows: Final = (
        ()
        if built is None
        else _failure_spend_rows(
            built,
            failure_row_response_cost(request),
            _failure_snapshot_with_guardrails_recorded_after_it(
                built.get("standard_logging_object"),
                _mapping(built.get("metadata")).get("standard_logging_guardrail_information"),
            ),
            _proxy_deployment,
        )
    )
    return {
        "kind": "failure",
        "settings": _settings(),
        "identity": {
            "token": LiteLLMProxyRequestSetup.get_logged_api_key(auth),
            "user_id": auth.user_id,
            "team_id": auth.team_id,
            "org_id": auth.org_id,
            "project_id": auth.project_id,
            "end_user_id": None if litellm.disable_end_user_cost_tracking else auth.end_user_id,
            "key_alias": auth.key_alias,
        },
        "enrich": {"resolve_missing_key_identity": True}
        if not isinstance(exception, DBLookupDeadlineExceeded)
        else None,
        "completed_cost": completed_cost,
        "track": track,
        "tags": _get_request_tags_for_cost_tracking(
            built.get("standard_logging_object") if built is not None else None,  # pyright: ignore[reportArgumentType]  # the lifted failure payload dict
            dict(_mapping(built.get("metadata"))) if built is not None else {},
        ),
        "window_resets": _window_resets(auth.budget_reservation),
        "rows": [
            _row(row, exception, start_time, end_time, cost, list(_committed_spend_scope(auth).tracked()))
            for cost, row in rows
        ],
    }


__all__ = ["spend_log_failure_input", "spend_log_success_input"]
