"""Explicit configuration for a remote, self-hosted A3S Box service."""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Mapping
from urllib.parse import urlparse


@dataclass(frozen=True, slots=True)
class A3SRemoteConnection:
    """Connection values for an explicitly remote A3S Box deployment.

    Local :class:`a3s_box.Sandbox` use does not instantiate this class and does
    not read endpoint or API-key environment variables.
    """

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
    ) -> A3SRemoteConnection:
        """Read remote-only settings without mutating process state."""

        values = os.environ if environment is None else environment
        api_url = values.get("A3S_BOX_ENDPOINT")
        if not api_url:
            raise ValueError("A3S_BOX_ENDPOINT is required for remote mode")
        return cls(
            api_url=api_url,
            domain=values.get("A3S_BOX_DOMAIN"),
            api_key=values.get("A3S_BOX_API_KEY"),
            sandbox_url=values.get("A3S_BOX_SANDBOX_URL"),
        )

    def official_python_options(self) -> dict[str, str]:
        """Return options for an official E2B client used in remote mode."""

        assert self.domain is not None
        options = {
            "api_url": self.api_url,
            "domain": self.domain,
        }
        if self.api_key is not None:
            options["api_key"] = self.api_key
        if self.sandbox_url is not None:
            options["sandbox_url"] = self.sandbox_url
        return options

    def python_options(self) -> dict[str, str]:
        """Deprecated alias for :meth:`official_python_options`."""

        return self.official_python_options()

    def volume_options(self) -> dict[str, str]:
        """Return the remote endpoint option for official Volume calls."""

        return {"api_url": self.api_url}


# Backwards-compatible name. It remains remote-only; local Sandbox creation
# never constructs it.
A3SConnectionConfig = A3SRemoteConnection


def _domain_from_endpoint(endpoint: str) -> str:
    parsed = urlparse(endpoint)
    if parsed.scheme not in {"http", "https"} or parsed.hostname is None:
        raise ValueError("api_url must be an absolute HTTP or HTTPS URL")
    return parsed.hostname.removeprefix("api.")
