"""Regression checks for separately reset namespace native startup windows."""

from __future__ import annotations

import importlib.util
from importlib.machinery import SourceFileLoader
import pathlib
import sys
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[3]
CLI = ROOT / "scripts/yield-local-ci"
CLI_SPEC = importlib.util.spec_from_loader(
    "yield_local_ci_namespace_windows", SourceFileLoader("yield_local_ci_namespace_windows", str(CLI))
)
assert CLI_SPEC is not None and CLI_SPEC.loader is not None
CLI_MODULE = importlib.util.module_from_spec(CLI_SPEC)
CLI_SPEC.loader.exec_module(CLI_MODULE)

RUNNER_DIR = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(RUNNER_DIR))
import run_native_production_case as NATIVE_RUNNER  # noqa: E402


class NamespaceWindowRoutingTests(unittest.TestCase):
    def test_aggregate_attempts_poll_window_after_receive_window_failure(self) -> None:
        calls: list[tuple[str, str, str]] = []

        def check(args: object) -> None:
            calls.append((args.sha, args.lane, args.case))
            if args.case.endswith("receive-failure"):
                raise CLI_MODULE.CiError("simulated receive-window failure")

        with mock.patch.object(CLI_MODULE, "check", side_effect=check):
            with self.assertRaisesRegex(CLI_MODULE.CiError, "simulated receive-window failure"):
                CLI_MODULE.run_namespace_failure_aggregate("a" * 40)

        self.assertEqual(
            calls,
            [
                ("a" * 40, "native", "prerequisite:startup-namespace-receive-failure"),
                ("a" * 40, "native", "prerequisite:startup-namespace-poll-failure"),
            ],
        )

    def test_aggregate_succeeds_only_after_both_windows_return(self) -> None:
        calls: list[str] = []
        with mock.patch.object(CLI_MODULE, "check", side_effect=lambda args: calls.append(args.case)):
            CLI_MODULE.run_namespace_failure_aggregate("b" * 40)
        self.assertEqual(
            calls,
            [
                "prerequisite:startup-namespace-receive-failure",
                "prerequisite:startup-namespace-poll-failure",
            ],
        )

    def test_poll_window_is_an_exact_unique_native_selector(self) -> None:
        case = "namespace-poll"
        selector = NATIVE_RUNNER.CASES[case]
        self.assertEqual(
            selector,
            "operation_journal::owner::tests::namespace_backing_tests::namespace_backing_native_production_error_outlives_original_worker_join_poll",
        )
        output = (
            "running 1 test\n"
            f"test {selector} ... ok\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 88 filtered out; finished in 0.01s\n"
        )
        self.assertTrue(NATIVE_RUNNER.selector_output_is_exactly_one_success(selector, output))


if __name__ == "__main__":
    unittest.main()
