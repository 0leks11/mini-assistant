"""Default host checks must never contact a real GPT Mini device."""

import importlib.util
import os
from pathlib import Path
import unittest
from unittest import mock


LIVE_TEST = Path(__file__).with_name("test_live_device_acoustic_release.py")


def load_live_test(environment):
    spec = importlib.util.spec_from_file_location("live_acoustic_opt_in_fixture", LIVE_TEST)
    module = importlib.util.module_from_spec(spec)
    with mock.patch.dict(os.environ, environment, clear=True):
        spec.loader.exec_module(module)
    return module.LiveDeviceAcousticReleaseTest


class LiveDeviceOptInTests(unittest.TestCase):
    def test_default_discovery_never_contacts_a_device(self):
        for environment in (
            {},
            {"GPT_MINI_RUN_LIVE_ACOUSTIC_TEST": "1"},
            {"GPT_MINI_URL": "http://127.0.0.1:1"},
            {
                "GPT_MINI_RUN_LIVE_ACOUSTIC_TEST": "true",
                "GPT_MINI_URL": "http://127.0.0.1:1",
            },
        ):
            with self.subTest(environment=environment):
                case = load_live_test(environment)
                suite = unittest.defaultTestLoader.loadTestsFromTestCase(case)
                result = unittest.TestResult()
                with mock.patch(
                    "urllib.request.urlopen", side_effect=RuntimeError("network forbidden")
                ) as network:
                    suite.run(result)
                network.assert_not_called()
                self.assertEqual(result.testsRun, 1)
                self.assertEqual(len(result.skipped), 1)
                self.assertFalse(result.failures)
                self.assertFalse(result.errors)

    def test_explicit_opt_in_and_target_make_live_test_selectable(self):
        case = load_live_test(
            {
                "GPT_MINI_RUN_LIVE_ACOUSTIC_TEST": "1",
                "GPT_MINI_URL": "http://127.0.0.1:1",
            }
        )
        self.assertFalse(getattr(case, "__unittest_skip__", False))
        # Selection is checked here; the actual live test remains unexecuted.


if __name__ == "__main__":
    unittest.main()
