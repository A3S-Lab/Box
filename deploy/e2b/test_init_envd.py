import importlib.util
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("init-envd.py")
SPEC = importlib.util.spec_from_file_location("a3s_box_init_envd", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
INIT_ENVD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(INIT_ENVD)


class BuildEnvironmentTests(unittest.TestCase):
    def test_default_user_identity_replaces_root_process_values(self) -> None:
        environment = INIT_ENVD.build_environment(
            {
                "HOME": "/root",
                "LOGNAME": "root",
                "PATH": "/usr/bin:/bin",
                "PWD": "/root",
                "SHELL": "/bin/sh",
                "USER": "root",
            }
        )

        self.assertEqual(environment["HOME"], "/home/user")
        self.assertEqual(environment["LOGNAME"], "user")
        self.assertEqual(environment["SHELL"], "/bin/bash")
        self.assertEqual(environment["USER"], "user")
        self.assertEqual(environment["PATH"], "/usr/bin:/bin")
        self.assertNotIn("PWD", environment)


if __name__ == "__main__":
    unittest.main()
