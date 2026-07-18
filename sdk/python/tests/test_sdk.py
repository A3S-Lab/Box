from __future__ import annotations

import unittest

import e2b
import e2b_code_interpreter

import a3s_box
from a3s_box import A3SConnectionConfig
from a3s_box import code_interpreter


class SdkTests(unittest.TestCase):
    def test_reexports_pinned_official_clients(self) -> None:
        self.assertIs(a3s_box.Sandbox, e2b.Sandbox)
        self.assertIs(a3s_box.AsyncSandbox, e2b.AsyncSandbox)
        self.assertIs(code_interpreter.Sandbox, e2b_code_interpreter.Sandbox)
        self.assertIs(
            code_interpreter.AsyncSandbox,
            e2b_code_interpreter.AsyncSandbox,
        )

    def test_connection_options_use_standard_e2b_names(self) -> None:
        connection = A3SConnectionConfig.from_environment(
            {
                "A3S_BOX_ENDPOINT": "https://api.box.example.com",
                "A3S_BOX_API_KEY": "a3s_a1b2c3",
                "A3S_BOX_SANDBOX_URL": "https://sandbox.box.example.com",
            }
        )
        self.assertEqual(
            connection.python_options(),
            {
                "api_url": "https://api.box.example.com",
                "domain": "box.example.com",
                "validate_api_key": False,
                "api_key": "a3s_a1b2c3",
                "sandbox_url": "https://sandbox.box.example.com",
            },
        )

    def test_connection_domain_can_be_overridden_for_self_hosting(self) -> None:
        connection = A3SConnectionConfig.from_environment(
            {
                "A3S_BOX_ENDPOINT": "https://gateway.internal.example",
                "A3S_BOX_DOMAIN": "sandboxes.internal.example",
            }
        )
        self.assertEqual(connection.domain, "sandboxes.internal.example")

    def test_native_config_does_not_require_e2b_environment_names(self) -> None:
        with self.assertRaisesRegex(ValueError, "A3S_BOX_ENDPOINT is required"):
            A3SConnectionConfig.from_environment(
                {
                    "E2B_API_URL": "https://api.box.example.com",
                    "E2B_DOMAIN": "box.example.com",
                }
            )

    def test_connection_rejects_a_non_http_endpoint(self) -> None:
        with self.assertRaisesRegex(
            ValueError,
            "api_url must be an absolute HTTP or HTTPS URL",
        ):
            A3SConnectionConfig(api_url="unix:///run/a3s-box.sock")


if __name__ == "__main__":
    unittest.main()
