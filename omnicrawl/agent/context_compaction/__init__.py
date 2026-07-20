"""上下文压缩子包的稳定公共入口。"""

from .evidence import (
    DEFAULT_EVIDENCE_MAX_ITEMS,
    DEFAULT_EVIDENCE_MAX_OUTPUT_TOKENS,
    RECALL_SESSION_EVIDENCE_TOOL_NAME,
    SessionEvidenceRecallService,
)
from .models import (
    AutoCompactionDecision,
    CompactionBatch,
    ContextBudgetSnapshot,
    ContextCompactionOutcome,
    ModelSummaryResult,
    SourceEvent,
    SummaryModelResponse,
    SummaryValidationResult,
    TokenUsageSample,
)
from .policy import ContextBudgetManager, estimate_json_tokens
from .projection import ContextAssembler, render_summary_markdown
from .service import (
    ContextCompactionMeasurementService,
    ContextCompactionService,
    ContextMeasurementResult,
)
from .summary import (
    ModelSummaryCompactor,
    RuntimeSummaryModelAdapter,
    SummaryGenerationError,
    load_summary_prompt,
)
from .validation import SummaryValidator

__all__ = [
    "AutoCompactionDecision",
    "CompactionBatch",
    "ContextAssembler",
    "ContextBudgetManager",
    "ContextBudgetSnapshot",
    "ContextCompactionMeasurementService",
    "ContextCompactionOutcome",
    "ContextCompactionService",
    "ContextMeasurementResult",
    "DEFAULT_EVIDENCE_MAX_ITEMS",
    "DEFAULT_EVIDENCE_MAX_OUTPUT_TOKENS",
    "ModelSummaryCompactor",
    "ModelSummaryResult",
    "RECALL_SESSION_EVIDENCE_TOOL_NAME",
    "RuntimeSummaryModelAdapter",
    "SourceEvent",
    "SummaryGenerationError",
    "SummaryModelResponse",
    "SummaryValidationResult",
    "SummaryValidator",
    "SessionEvidenceRecallService",
    "TokenUsageSample",
    "estimate_json_tokens",
    "load_summary_prompt",
    "render_summary_markdown",
]
