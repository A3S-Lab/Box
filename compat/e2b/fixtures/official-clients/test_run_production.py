import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import run_production


class NativeEnvironmentTests(unittest.TestCase):
    def test_removes_every_official_connection_override(self) -> None:
        self.assertEqual(
            set(run_production.E2B_CONNECTION_ENVIRONMENT),
            {
                "E2B_API_KEY",
                "E2B_API_URL",
                "E2B_DEBUG",
                "E2B_DOMAIN",
                "E2B_SANDBOX_URL",
                "E2B_VALIDATE_API_KEY",
                "E2B_VOLUME_API_URL",
            },
        )


class PrepareNativeTypescriptTests(unittest.TestCase):
    def test_compiler_resolves_pinned_official_dependencies(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            temp = Path(directory)
            sdk_root = temp / "sdk"
            source = sdk_root / "typescript"
            (source / "src").mkdir(parents=True)
            (source / "src" / "index.ts").write_text(
                "export { Sandbox } from 'e2b'\n",
                encoding="utf-8",
            )
            (source / "package.json").write_text(
                '{"name":"@a3s-lab/box","version":"0.1.0"}\n',
                encoding="utf-8",
            )
            (source / "tsconfig.json").write_text("{}\n", encoding="utf-8")

            environment = temp / "typescript"
            environment.mkdir()
            client = environment / "production_typescript_client.mjs"
            client.touch()
            modules = environment / "node_modules"
            (modules / ".bin").mkdir(parents=True)
            compiler = modules / ".bin" / "tsc"
            compiler.touch()

            calls = 0

            def run(command: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
                nonlocal calls
                calls += 1
                if calls == 2:
                    build_source = Path(str(kwargs["cwd"]))
                    dependencies = build_source / "node_modules"
                    self.assertTrue(dependencies.is_symlink())
                    self.assertEqual(dependencies.resolve(), modules.resolve())
                if calls == 3:
                    tarball = temp / "a3s-lab-box-0.1.0.tgz"
                    tarball.touch()
                    return subprocess.CompletedProcess(
                        command,
                        0,
                        stdout=f"{tarball.name}\n",
                    )
                return subprocess.CompletedProcess(command, 0)

            with (
                mock.patch.object(run_production, "SDK_ROOT", sdk_root),
                mock.patch.object(run_production.subprocess, "run", side_effect=run),
            ):
                run_production.prepare_native_typescript(temp, client)

            self.assertEqual(calls, 4)
            self.assertFalse((temp / "a3s-typescript-sdk" / "node_modules").exists())


if __name__ == "__main__":
    unittest.main()
