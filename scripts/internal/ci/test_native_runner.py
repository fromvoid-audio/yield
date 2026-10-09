"""Evidence checks for the isolated native production selector runner."""

from __future__ import annotations

import importlib.util
import pathlib
import unittest


RUNNER = pathlib.Path(__file__).with_name("run_native_production_case.py")
SPEC = importlib.util.spec_from_file_location("yield_native_runner", RUNNER)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class NativeRunnerEvidenceTests(unittest.TestCase):
    def test_accepts_one_exact_successful_selector(self) -> None:
        selector = MODULE.CASES["capacity"]
        output = (
            "running 1 test\n"
            f"test {selector} ... ok\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 82 filtered out; finished in 0.10s\n"
        )
        self.assertTrue(MODULE.selector_output_is_exactly_one_success(selector, output))

    def test_rejects_empty_skipped_failed_or_different_selector_output(self) -> None:
        selector = MODULE.CASES["capacity"]
        self.assertFalse(MODULE.selector_output_is_exactly_one_success(selector, ""))
        self.assertFalse(MODULE.selector_output_is_exactly_one_success(
            selector,
            "running 1 test\n"
            f"test {selector} ... ignored\n"
            "test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 82 filtered out; finished in 0.00s\n",
        ))
        self.assertFalse(MODULE.selector_output_is_exactly_one_success(
            selector,
            "running 1 test\n"
            f"test {MODULE.CASES['control']} ... ok\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 82 filtered out; finished in 0.00s\n",
        ))


if __name__ == "__main__":
    unittest.main()
