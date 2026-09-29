"""Runs ops/tests/backup_restore_drill.sh (real S3 API via `rclone serve s3`) when the tools exist."""

from __future__ import annotations

import shutil
import subprocess

import pytest

from .conftest import REPO


@pytest.mark.skipif(
    not (shutil.which("rclone") and shutil.which("zstd") and shutil.which("curl")),
    reason="needs rclone >= 1.65, zstd and curl",
)
def test_backup_restore_drill() -> None:
    out = subprocess.run(
        [str(REPO / "ops" / "tests" / "backup_restore_drill.sh")],
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    )
    assert out.returncode == 0, out.stdout + out.stderr
    assert "drill passed" in out.stdout
