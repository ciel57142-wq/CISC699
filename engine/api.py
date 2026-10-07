"""Engine entry point (HS2 §7): Engine.execute(query, variables) -> ExecutionResult."""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from pathlib import Path

from engine.adapters.mongo import MongoAdapter
from engine.adapters.pg import PostgresAdapter
from engine.config import REPO
from engine.errors import INTERNAL, EngineError
from engine.exec.executor import BIND_BATCH, Executor, Trace
from engine.parse.validator import parse_and_validate
from engine.plan.builder import build, root_key
from engine.plan.logical import Node, explain
from engine.rewrite.physical import PhysNode, explain_physical, lower
from engine.rewrite.rules import RuleConfig, rewrite
from engine.schema.resolver import backend_params, load


@dataclass
class ExecutionResult:
    data: dict | None
    errors: list[dict] = field(default_factory=list)
    trace: Trace | None = None
    logical: Node | None = None
    physical: PhysNode | None = None

    def explain(self) -> str:
        return (explain(self.logical) + "\n--- physical ---\n" + explain_physical(self.physical)
                if self.logical and self.physical else "")


class Engine:
    def __init__(self, schema_path: Path | str = REPO / "schema.graphql",
                 backends_path: Path | str = REPO / "backends.toml",
                 rules: RuleConfig = RuleConfig(), batch: int = BIND_BATCH, connect: bool = True):
        self.schema, self.pm = load(schema_path, backends_path)
        self.rules = rules
        self.adapters: dict = {}
        if connect:
            for name, b in self.pm.backends.items():
                p = backend_params(b)
                if b.kind == "postgres":
                    self.adapters[name] = PostgresAdapter(name, p["dsn"])
                else:
                    self.adapters[name] = MongoAdapter(name, p["uri"], p["db"])
        self.executor = Executor(self.adapters, batch)

    def plan(self, query: str, variables: dict | None = None) -> tuple[Node, PhysNode, str]:
        doc = parse_and_validate(self.schema, query)
        logical = build(self.schema, self.pm, doc, variables)
        caps = {n: getattr(a, "capabilities", {}) for n, a in self.adapters.items()}
        physical = rewrite(lower(self.pm, logical), self.rules, caps)
        return logical, physical, root_key(doc)

    def execute(self, query: str, variables: dict | None = None) -> ExecutionResult:
        trace = Trace()
        try:
            t = time.perf_counter_ns()
            logical, physical, key = self.plan(query, variables)
            trace.add("parse_plan", time.perf_counter_ns() - t)
            rows = self.executor.run(physical, trace)
            t = time.perf_counter_ns()
            data = {key: self.executor.shape(physical, rows)}
            trace.add("shape", time.perf_counter_ns() - t)
            return ExecutionResult(data, [], trace, logical, physical)
        except EngineError as e:
            return ExecutionResult(None, [e.as_graphql()], trace)
        except Exception as e:   # planner invariant violated: always a bug (HS2 §7.2)
            return ExecutionResult(None, [EngineError(INTERNAL, f"{type(e).__name__}: {e}").as_graphql()], trace)

    def close(self) -> None:
        for a in self.adapters.values():
            a.close()
