"""Offline switch regressions against a real CLI with synthetic data only.

Run after building: python tests/native-switch-safety.py
Set PR31_TEST_EXE to select a different executable. Both storage roots are
isolated; sandbox mode disables real process control and network probes.
"""

import base64
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
TARGET = (ROOT / "src-tauri" / "target").resolve()
EXE_NAME = "zcode-switch.exe" if os.name == "nt" else "zcode-switch"
EXE = Path(os.environ.get("PR31_TEST_EXE", str(TARGET / "debug" / EXE_NAME))).resolve()


def jwt(subject, exp=None):
    payload = {"id": subject}
    if exp is not None:
        payload["exp"] = exp
    encoded = base64.urlsafe_b64encode(json.dumps(payload).encode()).decode().rstrip("=")
    return f"e30.{encoded}.synthetic"


def credentials(subject, exp=None):
    return {"oauth:active_provider": "zai", "zcodejwttoken": jwt(subject, exp)}


class NativeSwitchSafety(unittest.TestCase):
    def setUp(self):
        TARGET.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="native-switch-test-", dir=TARGET)
        self.root = Path(self.temp.name).resolve()
        self.assertTrue(self.root.is_relative_to(TARGET))
        self.home, self.data = self.root / "home", self.root / "data"
        self.accounts = self.home / ".zcode-switch" / "accounts"
        self.native = self.data / ".zcode" / "v2"
        self.arms = self.home / "arms-store-sandbox" / "ZGVmYXVsdA.json"
        self.accounts.mkdir(parents=True)
        self.native.mkdir(parents=True)
        self.arms.parent.mkdir(parents=True)
        (self.home / ".zcode-switch" / "settings.json").write_text('{"auto_claim":false}', encoding="utf-8")
        (self.native / "credentials.json").write_text(json.dumps(credentials("live", 4102444800)), encoding="utf-8")
        (self.native / "telemetry-state.json").write_text('{"deviceMid":"native-mid","other":"native"}', encoding="utf-8")
        self.arms.write_text('{"_arms_uid":"native-uid","_arms_session":"native-session"}', encoding="utf-8")
        self.env = os.environ.copy()
        self.env.update({
            "ZCODE_SWITCH_HOME": str(self.home),
            "ZCODE_SWITCH_DATA_ROOT": str(self.data),
            "ZCODE_CREDENTIAL_SECRET": "synthetic-secret",
        })

    def tearDown(self):
        self.assertTrue(self.root.is_relative_to(TARGET))
        self.temp.cleanup()

    def save_account(self, creds, account_id="target", mid="saved-claim-mid"):
        account = {
            "id": account_id, "name": f"Synthetic {account_id}", "created_at": "x", "updated_at": "x",
            "hash": "recomputed", "credentials": creds, "config": None,
            "virtual_device_mid": mid, "virtual_arms_uid": "saved-arms-uid",
        }
        (self.accounts / f"{account_id}.json").write_text(json.dumps(account), encoding="utf-8")

    def run_cli(self, *args, env=None):
        proc = subprocess.run(
            [str(EXE), "--cli", "--lang", "en", *args], env=env or self.env,
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=20,
        )
        return proc.returncode, json.loads(proc.stdout)

    def snapshot(self):
        return {p.relative_to(self.root): p.read_bytes() for p in self.root.rglob("*") if p.is_file()}

    def test_expired_target_is_rejected_without_changing_any_files(self):
        self.save_account(credentials("expired", 1))
        before = self.snapshot()
        code, result = self.run_cli("switch", "--id", "target", "--force", "--no-restart", "--no-hot")
        self.assertNotEqual(code, 0)
        self.assertIn("expired", result["error"].lower())
        self.assertIn("use add", result["error"].lower())
        self.assertEqual(before, self.snapshot())

    def test_cold_switch_preserves_native_device_and_monitoring_state(self):
        target = credentials("target", 4102444800)
        self.save_account(target)
        telemetry_before = (self.native / "telemetry-state.json").read_bytes()
        arms_before = self.arms.read_bytes()
        code, result = self.run_cli("switch", "--id", "target", "--no-restart", "--no-hot")
        self.assertEqual(code, 0, result)
        self.assertTrue(result["switched"])
        self.assertEqual(json.loads((self.native / "credentials.json").read_text()), target)
        self.assertEqual((self.native / "telemetry-state.json").read_bytes(), telemetry_before)
        self.assertEqual(self.arms.read_bytes(), arms_before)
        saved = json.loads((self.accounts / "target.json").read_text())
        self.assertEqual(saved["virtual_device_mid"], "saved-claim-mid")
        preserved = [json.loads(p.read_text()) for p in self.accounts.glob("*.json") if p.stem != "target"]
        self.assertEqual(len(preserved), 1)
        self.assertEqual(preserved[0]["credentials"], credentials("live", 4102444800))

    def test_switch_does_not_create_missing_native_telemetry(self):
        self.save_account(credentials("target", 4102444800))
        (self.native / "telemetry-state.json").unlink()
        code, result = self.run_cli("switch", "--id", "target", "--no-restart", "--no-hot")
        self.assertEqual(code, 0, result)
        self.assertFalse((self.native / "telemetry-state.json").exists())

    def test_switch_does_not_create_missing_monitoring_store(self):
        self.save_account(credentials("target", 4102444800))
        self.arms.unlink()
        self.arms.parent.rmdir()
        code, result = self.run_cli("switch", "--id", "target", "--no-restart", "--no-hot")
        self.assertEqual(code, 0, result)
        self.assertFalse(self.arms.parent.exists())

    def test_already_active_login_syncs_refreshed_credentials_without_changing_native_state(self):
        self.save_account(credentials("live", 1))
        fresh = credentials("live", 4102444800)
        (self.native / "credentials.json").write_text(json.dumps(fresh), encoding="utf-8")
        telemetry_before = (self.native / "telemetry-state.json").read_bytes()
        arms_before = self.arms.read_bytes()
        code, result = self.run_cli("switch", "--id", "target", "--no-restart", "--no-hot")
        self.assertEqual(code, 0, result)
        self.assertTrue(result["already_active"])
        saved = json.loads((self.accounts / "target.json").read_text())
        self.assertEqual(saved["credentials"], fresh)
        self.assertEqual(saved["virtual_device_mid"], "saved-claim-mid")
        self.assertEqual((self.native / "telemetry-state.json").read_bytes(), telemetry_before)
        self.assertEqual(self.arms.read_bytes(), arms_before)

    def test_snapshot_without_expiry_is_not_mistaken_for_expired(self):
        self.save_account(credentials("target"))
        code, result = self.run_cli("switch", "--id", "target", "--no-restart", "--no-hot")
        self.assertEqual(code, 0, result)
        self.assertTrue(result["switched"])

    def test_encrypted_bundle_roundtrip_keeps_claim_device_id(self):
        self.save_account(credentials("target", 4102444800))
        bundle = self.root / "synthetic.zsb"
        code, result = self.run_cli("export", "--id", "target", "--out", str(bundle),
                                    "--password", "synthetic-password")
        self.assertEqual(code, 0, result)
        destination_home, destination_data = self.root / "restored-home", self.root / "restored-data"
        destination_home.mkdir()
        destination_data.mkdir()
        env = self.env.copy()
        env.update({"ZCODE_SWITCH_HOME": str(destination_home), "ZCODE_SWITCH_DATA_ROOT": str(destination_data)})
        code, result = self.run_cli("import", "--file", str(bundle), "--password", "synthetic-password", env=env)
        self.assertEqual(code, 0, result)
        imported = list((destination_home / ".zcode-switch" / "accounts").glob("*.json"))
        self.assertEqual(len(imported), 1)
        saved = json.loads(imported[0].read_text(encoding="utf-8"))
        self.assertEqual(saved["virtual_device_mid"], "saved-claim-mid")

    def test_export_all_keeps_each_accounts_distinct_claim_device_id(self):
        self.save_account(credentials("first", 4102444800), "first", "first-mid")
        self.save_account(credentials("second", 4102444800), "second", "second-mid")
        self.save_account(credentials("legacy", 4102444800), "legacy", None)
        bundle = self.root / "synthetic-all.zsb"
        code, result = self.run_cli("export-all", "--out", str(bundle), "--password", "synthetic-password")
        self.assertEqual(code, 0, result)
        restored_home, restored_data = self.root / "restored-all-home", self.root / "restored-all-data"
        restored_home.mkdir()
        restored_data.mkdir()
        env = self.env.copy()
        env.update({"ZCODE_SWITCH_HOME": str(restored_home), "ZCODE_SWITCH_DATA_ROOT": str(restored_data)})
        code, result = self.run_cli("import", "--file", str(bundle), "--password", "synthetic-password", env=env)
        self.assertEqual(code, 0, result)
        imported = [json.loads(p.read_text(encoding="utf-8"))
                    for p in (restored_home / ".zcode-switch" / "accounts").glob("*.json")]
        contexts = {a["credentials"]["zcodejwttoken"]: a["virtual_device_mid"] for a in imported}
        self.assertEqual(contexts, {
            jwt("first", 4102444800): "first-mid",
            jwt("second", 4102444800): "second-mid",
            jwt("legacy", 4102444800): None,
        })


if __name__ == "__main__":
    unittest.main(verbosity=2)
