"""Typed A3S endpoint configuration helpers."""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Mapping
from urllib.parse import urlparse


@dataclass(frozen=True, slots=True)
class A3SConnectionConfig:
    """Connection values accepted by the pinned official E2B Python SDK."""

    api_url: str
    domain: str | None = None
    api_key: str | None = None
    sandbox_url: str | None = None

    def __post_init__(self) -> None:
        if not self.api_url.strip():
            raise ValueError("api_url cannot be empty")
        derived_domain = _domain_from_endpoint(self.api_url)
        domain = derived_domain if self.domain is None else self.domain
        if not domain.strip():
            raise ValueError("domain cannot be empty when provided")
        object.__setattr__(self, "domain", domain)
        if self.api_key is not None and not self.api_key.strip():
            raise ValueError("api_key cannot be empty when provided")
        if self.sandbox_url is not None and not self.sandbox_url.strip():
            raise ValueError("sandbox_url cannot be empty when provided")

    @classmethod
    def from_environment(
        cls,
        environment: Mapping[str, str] | None = None,
    ) -> A3SConnectionConfig:
        """Read A3S Box endpoint variables without mutating the process."""

        values = os.environ if environment is None else environment
        api_url = values.get("A3S_BOX_ENDPOINT")
        if not api_url:
            raise ValueError("A3S_BOX_ENDPOINT is required")
        return cls(
            api_url=api_url,
            domain=values.get("A3S_BOX_DOMAIN"),
            api_key=values.get("A3S_BOX_API_KEY"),
            sandbox_url=values.get("A3S_BOX_SANDBOX_URL"),
        )

    def python_options(self) -> dict[str, str | bool]:
        """Return keyword arguments for Python Sandbox create/connect calls."""

        assert self.domain is not None
        options: dict[str, str | bool] = {
            "api_url": self.api_url,
            "domain": self.domain,
            "validate_api_key": False,
        }
        if self.api_key is not None:
            options["api_key"] = self.api_key
        if self.sandbox_url is not None:
            options["sandbox_url"] = self.sandbox_url
        return options


def _domain_from_endpoint(endpoint: str) -> str:
    parsed = urlparse(endpoint)
    if parsed.scheme not in {"http", "https"} or parsed.hostname is None:
        raise ValueError("api_url must be an absolute HTTP or HTTPS URL")
    hostname = parsed.hostname
    return hostname.removeprefix("api.")
