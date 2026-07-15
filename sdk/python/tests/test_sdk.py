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
                "E2B_API_URL": "https://api.box.example.com",
                "E2B_DOMAIN": "box.example.com",
                "E2B_API_KEY": "e2b_a1b2c3",
            }
        )
        self.assertEqual(
            connection.python_options(),
            {
                "api_url": "https://api.box.example.com",
                "domain": "box.example.com",
                "api_key": "e2b_a1b2c3",
            },
        )


if __name__ == "__main__":
    unittest.main()
