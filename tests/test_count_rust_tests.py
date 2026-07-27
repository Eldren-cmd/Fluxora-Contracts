"""Unit tests for script/count_rust_tests.py."""

from __future__ import annotations

import sys
from pathlib import Path

from script import count_rust_tests


def test_contract_rust_files_returns_list_of_paths():
    files = count_rust_tests.contract_rust_files()
    assert isinstance(files, list)
    assert len(files) > 0
    for p in files:
        assert isinstance(p, Path)
        assert p.suffix == ".rs"


def test_count_tests_returns_total_and_dict():
    total, by_crate = count_rust_tests.count_tests()
    assert isinstance(total, int)
    assert total > 0
    assert isinstance(by_crate, dict)
    assert len(by_crate) > 0
    assert sum(by_crate.values()) == total


def test_main_returns_zero(capsys):
    ret = count_rust_tests.main()
    assert ret == 0
    captured = capsys.readouterr()
    assert "Total #[test] attributes:" in captured.out
