#!/usr/bin/env python3
"""Tests of the output parsers of measure.py, on samples in Cargo's formats.

Run: python3 scripts/test_measure.py
"""

import importlib.util
import os
import unittest

_spec = importlib.util.spec_from_file_location(
    "measure", os.path.join(os.path.dirname(os.path.abspath(__file__)), "measure.py"))
measure = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(measure)

FAILED = """error: test failed, to rerun pass `-p b --lib`
error: 1 target failed:
    `-p b --lib`
"""

DOCTEST_FAILED = """error: doctest failed, to rerun pass `-p a --doc`
error: 1 target failed:
    `-p a --doc`
"""

NOT_STARTED = """error: test failed, to rerun pass `-p a --lib`

Caused by:
  could not execute process `missing-runner '/work/target dir/debug/deps/a-0123'` (never executed)

Caused by:
  No such file or directory (os error 2)
error: doctest failed, to rerun pass `-p b --doc`

Caused by:
  could not execute process `/work/missing-rustdoc --test b/src/lib.rs` (never executed)

Caused by:
  No such file or directory (os error 2)
error: 2 targets failed:
    `-p a --lib`
    `-p b --doc`
"""

RUNNING = "     Running unittests src/lib.rs (target/debug/deps/b-0123)\n"

# Output of a Cargo that a test runs on a fixture: it comes after a test binary started and
# before the outer summary. The fixture can have the name of a workspace member.
NESTED = """error: could not compile `b` (lib) due to 1 previous error
error: test failed, to rerun pass `-p fixture --lib`

Caused by:
  could not execute process `/elsewhere/fixture-0123` (never executed)
"""


class Parsers(unittest.TestCase):
    def test_failing_tests_and_doctests(self):
        self.assertEqual(measure.failing_packages(FAILED), ["b"])
        self.assertFalse(measure.not_started(FAILED))
        self.assertEqual(measure.failing_packages(DOCTEST_FAILED), ["a"])
        self.assertFalse(measure.not_started(DOCTEST_FAILED))

    def test_targets_that_never_started(self):
        self.assertEqual(measure.failing_packages(NOT_STARTED), ["a", "b"])
        self.assertTrue(measure.not_started(NOT_STARTED))
        # A doctest alone, with the single-line summary of older Cargo.
        doctest = NOT_STARTED.split("error: doctest failed")[1].split("error: 2 targets")[0]
        doctest = "error: doctest failed" + doctest
        self.assertEqual(measure.failing_packages(doctest), ["b"])
        self.assertTrue(measure.not_started(doctest))

    def test_nested_cargo_output_does_not_count(self):
        nested = RUNNING + NESTED + FAILED
        self.assertEqual(measure.failing_packages(nested), ["b"])
        self.assertFalse(measure.not_started(nested))
        self.assertEqual(measure.compile_failures(nested), [])

    def test_compile_failures_of_the_build(self):
        build = "error: could not compile `b` (lib) due to 1 previous error\n"
        self.assertEqual(measure.compile_failures(build), ["b"])
        self.assertEqual(measure.compile_failures(build + RUNNING + NESTED), ["b"])

    def test_one_package_workspace(self):
        self.assertEqual(
            measure.failing_packages("error: test failed, to rerun pass `--lib`\n"), ["*"])

    def test_no_failure(self):
        self.assertEqual(measure.failing_packages("warning: unused variable\n"), [])
        self.assertFalse(measure.not_started(""))


if __name__ == "__main__":
    unittest.main()
