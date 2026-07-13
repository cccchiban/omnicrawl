"""模型、推理强度与审批模式配置路由。"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Request

from ...config.approval import normalize_approval_mode, save_approval_mode
from ...config.llm import ActiveModelRef, save_active_model_ref, save_reasoning_effort
from ...config.model_catalog import (
    ModelCatalogError,
    build_catalog,
    clear_discovery_cache,
    detect_model_options,
    ensure_current_model_option,
    save_llm_model,
)
from ..deps import data, service
from ..models import (
    APIServiceError,
    ApprovalChangeRequest,
    ModelChangeRequest,
    ReasoningChangeRequest,
)


router = APIRouter(tags=["configuration"])


@router.get("/models")
def list_models(request: Request) -> dict[str, Any]:
    agent = service(request).agent
    try:
        options = ensure_current_model_option(
            detect_model_options(agent.config.llm),
            agent.current_model,
        )
    except ModelCatalogError as exc:
        raise APIServiceError("MODEL_LIST_FAILED", str(exc), status_code=502) from exc
    return data([option.to_ui_dict() for option in options])


@router.get("/models/catalog")
def get_model_catalog(request: Request) -> dict[str, Any]:
    """双列模型目录：custom + detected + diagnostics。"""

    agent = service(request).agent
    try:
        catalog = build_catalog(config=agent.config.llm, refresh=False)
    except ModelCatalogError as exc:
        raise APIServiceError("MODEL_CATALOG_FAILED", str(exc), status_code=502) from exc
    custom_payload = []
    for item in catalog.get("custom", []):
        row = item.to_option().to_ui_dict()
        row.update(
            {
                "source": item.source,
                "key": item.key,
                "profile": item.profile_id,
                "protocol": item.protocol,
                "model_id": item.model_id,
                "display_name": item.display_name,
                "availability": item.availability,
            }
        )
        custom_payload.append(row)

    detected_payload = []
    for item in catalog.get("detected", []):
        row = item.to_option().to_ui_dict()
        row.update(
            {
                "source": item.source,
                "key": item.key,
                "profile": item.profile_id,
                "protocol": item.protocol,
                "model_id": item.model_id,
                "display_name": item.display_name,
                "availability": item.availability,
                "matched_custom_key": item.matched_custom_key,
            }
        )
        detected_payload.append(row)

    return data(
        {
            "current": catalog.get("current") or {},
            "custom": custom_payload,
            "detected": detected_payload,
            "diagnostics": catalog.get("diagnostics") or [],
        }
    )


@router.post("/models/refresh")
def refresh_model_catalog(request: Request) -> dict[str, Any]:
    """强制刷新远端模型发现缓存。"""

    current = service(request)
    current.ensure_mutation_allowed()
    clear_discovery_cache()
    try:
        catalog = build_catalog(config=current.agent.config.llm, refresh=True)
    except ModelCatalogError as exc:
        raise APIServiceError("MODEL_CATALOG_FAILED", str(exc), status_code=502) from exc
    return data(
        {
            "refreshed": True,
            "detected_count": len(catalog.get("detected") or []),
            "custom_count": len(catalog.get("custom") or []),
            "diagnostics": catalog.get("diagnostics") or [],
        }
    )


@router.put("/models/current")
def set_model(payload: ModelChangeRequest, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    selection = _resolve_model_selection(payload)

    def persist() -> None:
        if selection.get("ref") is not None:
            save_active_model_ref(selection["ref"])
        else:
            save_llm_model(selection["model"])

    current.agent.set_model(selection["model"], persist=persist)
    return data(
        {
            "model": current.agent.current_model,
            "source": selection.get("source") or "legacy",
            "key": selection.get("key") or "",
            "profile": selection.get("profile") or "",
            "protocol": selection.get("protocol") or "",
        }
    )


def _resolve_model_selection(payload: ModelChangeRequest) -> dict[str, Any]:
    """把规范字段或旧 model 字段解析为可切换目标。"""

    source = (payload.source or "").strip().lower()
    if source == "custom":
        key = (payload.key or payload.model or "").strip()
        if not key:
            raise APIServiceError("INVALID_MODEL", "source=custom 时必须提供 key。")
        return {
            "model": key,
            "source": "custom",
            "key": key,
            "ref": ActiveModelRef(source="custom", key=key),
        }
    if source == "detected":
        profile = (payload.profile or "").strip()
        model_id = (payload.model_id or payload.model or "").strip()
        protocol = (payload.protocol or "").strip()
        if not profile or not model_id:
            raise APIServiceError(
                "INVALID_MODEL",
                "source=detected 时必须提供 profile 与 model_id。",
            )
        token = f"{profile}/{model_id}"
        return {
            "model": token,
            "source": "detected",
            "profile": profile,
            "protocol": protocol,
            "ref": ActiveModelRef(
                source="detected",
                profile=profile,
                model_id=model_id,
                protocol=protocol,
            ),
        }

    model = (payload.model or payload.model_id or payload.key or "").strip()
    if not model:
        raise APIServiceError(
            "INVALID_MODEL",
            "请提供 model，或使用 source/key 或 source/profile/model_id。",
        )
    return {"model": model, "source": "legacy"}


@router.put("/reasoning")
def set_reasoning(payload: ReasoningChangeRequest, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    effort = current.agent.set_reasoning_effort(payload.effort)
    save_reasoning_effort(effort)
    return data({"reasoning_effort": effort})


@router.put("/approval")
def set_approval(payload: ApprovalChangeRequest, request: Request) -> dict[str, Any]:
    current = service(request)
    current.ensure_mutation_allowed()
    mode = normalize_approval_mode(payload.mode)
    current.agent.set_approval_mode(mode)
    save_approval_mode(mode)
    return data({"approval_mode": mode})
