"""Error contract (HS2 §7.2): every error carries a stable `extensions.code`."""

from __future__ import annotations

SCHEMA_INVALID = "SCHEMA_INVALID"
UNSUPPORTED_OPERATION = "UNSUPPORTED_OPERATION"
GRAPHQL_VALIDATION_FAILED = "GRAPHQL_VALIDATION_FAILED"   # added at HS3, see ISSUE-008
BACKEND_UNAVAILABLE = "BACKEND_UNAVAILABLE"
CAPABILITY = "CAPABILITY"
INTERNAL = "INTERNAL"


class EngineError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message

    def as_graphql(self) -> dict:
        return {"message": self.message, "extensions": {"code": self.code}}


class SchemaInvalid(EngineError):
    def __init__(self, message: str):
        super().__init__(SCHEMA_INVALID, message)


class Unsupported(EngineError):
    def __init__(self, message: str):
        super().__init__(UNSUPPORTED_OPERATION, message)


class CapabilityError(EngineError):
    """An adapter cannot express a fragment feature. Raised at plan time, never run time."""

    def __init__(self, message: str):
        super().__init__(CAPABILITY, message)
