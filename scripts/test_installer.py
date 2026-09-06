"""Exercise the exact configuration migration embedded in the curl installer."""
import contextlib
import io
from pathlib import Path
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
script = (ROOT / "install.sh").read_text()
source = script.split("<<'PY_CONFIG'\n", 1)[1].split("\nPY_CONFIG", 1)[0]
namespace = {"__name__": "installer_test"}
exec(compile(source, "install.sh:PY_CONFIG", "exec"), namespace)


class ConfigMigrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.target = Path(self.temp.name) / "config.toml"
        self.example = ROOT / "config.example.toml"

    def update(self):
        with contextlib.redirect_stdout(io.StringIO()):
            namespace["update_config"](self.target, self.example)

    def test_existing_values_comments_and_tables_survive_and_repeat_is_noop(self):
        original = b'# Keep this comment\r\nnut_host = "custom"\r\nnut_password = "test-only"\r\n"nut_connection_loss_shutdown_seconds" = 0\r\nbattery_service_life_days = 42\r\n[custom]\r\nvalue = 123'
        self.target.write_bytes(original)
        self.update()
        updated = self.target.read_bytes()
        parsed = tomllib.loads(updated.decode())
        self.assertTrue(updated.endswith(original))
        self.assertEqual(parsed["nut_connection_loss_shutdown_seconds"], 0)
        self.assertEqual(parsed["battery_service_life_days"], 42)
        self.assertEqual(parsed["custom"], {"value": 123})
        self.assertNotIn("runtime_shutdown_seconds", parsed)
        self.assertNotIn("charge_shutdown_percent", parsed)
        self.assertEqual(parsed["battery_state_path"], "/var/lib/unifi-ups-monitor/battery-state.toml")
        backups = list(self.target.parent.glob("config.toml.bak.*"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_bytes(), original)
        self.update()
        self.assertEqual(self.target.read_bytes(), updated)
        self.assertEqual(list(self.target.parent.glob("config.toml.bak.*")), backups)

    def test_commented_options_are_added_and_custom_command_is_retained(self):
        self.target.write_text('# battery_service_life_days = 7\nnotification_queue_command = "/custom/queue"\n')
        self.update()
        parsed = tomllib.loads(self.target.read_text())
        self.assertEqual(parsed["battery_service_life_days"], 1095)
        self.assertEqual(parsed["nut_connection_loss_shutdown_seconds"], 300)
        self.assertEqual(parsed["notification_queue_command"], "/custom/queue")

    def test_invalid_configuration_is_untouched(self):
        original = b'nut_host = "unfinished'
        self.target.write_bytes(original)
        with self.assertRaises(tomllib.TOMLDecodeError):
            self.update()
        self.assertEqual(self.target.read_bytes(), original)
        self.assertEqual(list(self.target.parent.iterdir()), [self.target])

    def test_fresh_install_copies_full_example(self):
        self.update()
        self.assertEqual(self.target.read_bytes(), self.example.read_bytes())
        self.assertFalse(list(self.target.parent.glob("*.bak.*")))

    def test_complete_custom_config_is_untouched(self):
        original = b'nut_connection_loss_shutdown_seconds = 0\nbattery_state_path = "/custom/state"\nbattery_service_life_days = 365\nnotification_queue_command = "/custom/mail"\n'
        self.target.write_bytes(original)
        self.update()
        self.assertEqual(self.target.read_bytes(), original)
        self.assertFalse(list(self.target.parent.glob("*.bak.*")))


if __name__ == "__main__":
    unittest.main()
