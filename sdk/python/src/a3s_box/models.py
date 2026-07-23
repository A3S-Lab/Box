"""Typed values returned by the native A3S Box SDK."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal


@dataclass(frozen=True, slots=True)
class CommandResult:
    stdout: str
    stderr: str
    exit_code: int
    truncated: bool = False


@dataclass(frozen=True, slots=True)
class WriteInfo:
    path: str
    size: int


@dataclass(frozen=True, slots=True)
class EntryInfo:
    name: str
    type: Literal["file", "directory", "unspecified"]
    path: str
    size: int
    mode: int
    permissions: str
    owner: str
    group: str
    modified_seconds: int
    modified_nanos: int
    symlink_target: str | None = None
