"""Typed A3S endpoint configuration helpers."""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Mapping


@dataclass(frozen=True, slots=True)
class A3SConnectionConfig:
    """Connection values accepted by the pinned official E2B Python SDK."""

    api_url: str
    domain: str
    api_key: str | None = None

    def __post_init__(self) -> None:
        if not self.api_url.strip():
            raise ValueError("api_url cannot be empty")
        if not self.domain.strip():
            raise ValueError("domain cannot be empty")
        if self.api_key is not None and not self.api_key.strip():
            raise ValueError("api_key cannot be empty when provided")

    @classmethod
    def from_environment(
        cls,
        environment: Mapping[str, str] | None = None,
    ) -> A3SConnectionConfig:
        """Read standard E2B endpoint variables without mutating the process."""

        values = os.environ if environment is None else environment
        api_url = values.get("E2B_API_URL")
        domain = values.get("E2B_DOMAIN")
        if not api_url:
            raise ValueError("E2B_API_URL is required")
        if not domain:
            raise ValueError("E2B_DOMAIN is required")
        return cls(
            api_url=api_url,
            domain=domain,
            api_key=values.get("E2B_API_KEY"),
        )

    def python_options(self) -> dict[str, str]:
        """Return keyword arguments for Python Sandbox create/connect calls."""

        options = {"api_url": self.api_url, "domain": self.domain}
        if self.api_key is not None:
            options["api_key"] = self.api_key
        return options
