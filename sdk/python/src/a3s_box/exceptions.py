"""Errors raised by the native A3S Box SDK."""

from __future__ import annotations


class A3SBoxError(RuntimeError):
    """Base error returned by the local A3S Box runtime."""

    def __init__(self, message: str, *, code: str = "runtime_error") -> None:
        super().__init__(message)
        self.code = code


class A3SBoxNotInstalledError(A3SBoxError):
    """The local ``a3s-box`` executable could not be found."""

    def __init__(self, binary: str) -> None:
        super().__init__(
            f"Cannot find the local A3S Box executable {binary!r}. "
            "Install a3s-box or set A3S_BOX_BINARY to its path.",
            code="binary_not_found",
        )
