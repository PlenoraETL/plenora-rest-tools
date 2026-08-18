from typing import Optional

SCHEMA_VERSION: int
__version__: str


class NativeRestEngineError(Exception): ...


class NativeEngine:
    def __init__(self, config_json: Optional[str] = None) -> None: ...
    def execute(self, request_json: str) -> str: ...
