#!/usr/bin/env python3
"""Exercise Chrome native-host registration for Windows, macOS, and Linux."""

import importlib.util
import json
import os
import pathlib
import tempfile
from types import SimpleNamespace
from unittest.mock import patch


root = pathlib.Path(__file__).resolve().parents[1]
installer_path = root / "extensions" / "comptrol-browser-bridge" / "install.py"
spec = importlib.util.spec_from_file_location("bridge_installer", installer_path)
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


def test_platform_manifest_directories(temporary):
    with patch.object(installer.platform, "system", return_value="Darwin"):
        mac = installer.get_manifest_dir()
    assert mac[0].endswith(
        "Library/Application Support/Google/Chrome/NativeMessagingHosts"
    )

    with patch.object(installer.platform, "system", return_value="Linux"):
        linux = installer.get_manifest_dir()
    assert linux[0].endswith(".config/google-chrome/NativeMessagingHosts")

    local_app_data = str(temporary / "windows-app-data")
    with patch.object(installer.platform, "system", return_value="Windows"):
        with patch.dict(os.environ, {"LOCALAPPDATA": local_app_data}):
            windows = installer.get_manifest_dir()
    assert windows == [
        str(
            pathlib.Path(local_app_data)
            / "Google"
            / "Chrome"
            / "NativeMessagingHosts"
        )
    ]


def run_install(system, fixture_root):
    fixture_root.mkdir(parents=True)
    host = fixture_root / "native_host.py"
    host.write_text("#!/usr/bin/env python3\n# fixture host\n", encoding="utf-8")
    state = fixture_root / "state"
    state.mkdir()
    runtime_config = state / "browser-bridge-config.json"
    runtime_config.write_text(
        '{"preserved":true,"native_host_script":"stale"}', encoding="utf-8"
    )
    manifests = fixture_root / "manifests"

    registry_calls = []
    registry = SimpleNamespace(
        HKEY_CURRENT_USER="current-user",
        REG_SZ="string",
        CreateKey=lambda hive, path: (
            registry_calls.append(("create", hive, path)) or object()
        ),
        SetValueEx=lambda *args: registry_calls.append(("set", args[1:])),
        CloseKey=lambda key: registry_calls.append(("close",)),
    )
    env = {
        "COMPTROL_STATE_DIR": str(state),
        "COMPTROL_BRIDGE_CONFIG": str(runtime_config),
        "COMPTROL_DAEMON_URL": "http://127.0.0.1:7317",
    }
    with patch.dict(os.environ, env, clear=False):
        with patch.object(installer, "get_manifest_dir", return_value=[str(manifests)]):
            with patch.object(installer.platform, "system", return_value=system):
                with patch.object(installer, "winreg", registry):
                    with patch.object(
                        installer.sys,
                        "argv",
                        [
                            "install.py",
                            "--host-path",
                            str(host),
                            "--extension-id",
                            "a" * 32,
                        ],
                    ):
                        installer.main()

    manifest_path = manifests / "comptrol_browser_bridge.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    config = json.loads(runtime_config.read_text(encoding="utf-8"))
    assert config["preserved"] is True
    assert config["extension_id"] == "a" * 32
    assert config["native_host_script"] == str(host)
    assert (state / "browser-bridge.token").is_file()

    if system == "Windows":
        launcher = fixture_root / "native_host.bat"
        assert launcher.is_file()
        assert manifest["path"] == str(launcher)
        assert len(registry_calls) == 3
    else:
        assert manifest["path"] == str(host)
        assert os.access(host, os.X_OK)
    assert manifest["allowed_origins"] == ["chrome-extension://" + "a" * 32 + "/"]


with tempfile.TemporaryDirectory(prefix="comptrol-install-conformance-") as directory:
    temporary = pathlib.Path(directory)
    test_platform_manifest_directories(temporary)
    for system in ("Windows", "Darwin", "Linux"):
        run_install(system, temporary / system.lower())
        print(f"PASS {system} Chrome native-host registration")
print("PASS platform manifest paths, extension origin, and host configuration")
