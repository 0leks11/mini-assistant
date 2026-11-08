"""Verify Spotify OAuth input handling without network or account access."""

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
from pathlib import Path
import shlex
import sys
import tempfile
import unittest
from urllib.parse import parse_qs, urlencode, urlsplit


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "spotify_oauth_probe.py"
spec = importlib.util.spec_from_file_location("spotify_oauth_probe", SCRIPT)
probe = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = probe
spec.loader.exec_module(probe)

CLIENT_ID = "0123456789abcdef0123456789abcdef"
STATE = "fixture-state"
VERIFIER = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
RFC_7636_CHALLENGE = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"


def callback_target(fields):
    return f"{urlsplit(probe.REDIRECT_URI).path}?{urlencode(fields)}"


class SpotifyOAuthProbeTests(unittest.TestCase):
    def test_authorization_uses_rfc_7636_s256_vector(self):
        url = urlsplit(probe.make_authorization(CLIENT_ID, STATE, VERIFIER))
        self.assertEqual((url.scheme, url.netloc, url.path), (
            "https", "accounts.spotify.com", "/authorize",
        ))
        query = parse_qs(url.query, strict_parsing=True)
        self.assertEqual(query["client_id"], [CLIENT_ID])
        self.assertEqual(query["response_type"], ["code"])
        self.assertEqual(query["redirect_uri"], [probe.REDIRECT_URI])
        self.assertEqual(query["state"], [STATE])
        self.assertEqual(query["code_challenge_method"], ["S256"])
        self.assertEqual(query["code_challenge"], [RFC_7636_CHALLENGE])
        self.assertNotIn("code_verifier", query)
        self.assertNotIn("client_secret", query)

    def test_callback_decodes_code_and_denial_without_printing(self):
        stdout, stderr = io.StringIO(), io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            accepted = probe.parse_callback(
                callback_target([("state", STATE), ("code", "fixture-code")]), STATE
            )
            denied = probe.parse_callback(
                callback_target([("error", "access_denied"), ("state", STATE)]), STATE
            )
        self.assertEqual(accepted, ("fixture-code", None))
        self.assertEqual(denied, (None, "access_denied"))
        self.assertEqual(stdout.getvalue(), "")
        self.assertEqual(stderr.getvalue(), "")

    def test_callback_rejects_wrong_origin_form_path_or_state(self):
        path = urlsplit(probe.REDIRECT_URI).path
        target = callback_target([("state", STATE), ("code", "fixture-code")])
        for candidate in (
            f"/unexpected?state={STATE}&code=fixture-code",
            f"https://foreign.invalid{target}",
            f"//foreign.invalid{target}",
            f"{path}?code=fixture-code",
            f"{path}?state=wrong-state&code=fixture-code",
            f"{path}?state=&code=fixture-code",
        ):
            with self.subTest(candidate=candidate), self.assertRaises(ValueError):
                probe.parse_callback(candidate, STATE)

    def test_callback_rejects_duplicate_or_conflicting_fields(self):
        for fields in (
            [("state", STATE), ("state", STATE), ("code", "fixture-code")],
            [("state", STATE), ("code", "a"), ("code", "b")],
            [("state", STATE), ("error", "access_denied"), ("error", "access_denied")],
            [("state", STATE), ("code", "a"), ("error", "access_denied")],
            [("state", STATE), ("code", "")],
            [("state", STATE), ("error", "")],
            [("state", STATE)],
        ):
            with self.subTest(fields=fields), self.assertRaises(ValueError):
                probe.parse_callback(callback_target(fields), STATE)

    def test_config_reads_only_selected_fields_and_does_not_execute(self):
        with tempfile.TemporaryDirectory(prefix="spotify probe test ") as directory:
            root = Path(directory)
            config = root / ".env"
            marker = root / "shell executed"
            command = f"$(touch {shlex.quote(str(marker))})"
            config.write_text(
                f'SPOTIFY_CLIENT_ID="{CLIENT_ID}"\n'
                f'SPOTIFY_REDIRECT_URI="{probe.REDIRECT_URI}"\n'
                f"UNRELATED_SETTING={command}\n",
                encoding="utf-8",
            )
            self.assertEqual(probe.read_config(config), CLIENT_ID)
            self.assertFalse(marker.exists())

            config.write_text(f"SPOTIFY_CLIENT_ID={command}\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                probe.read_config(config)
            self.assertFalse(marker.exists())

    def test_config_requires_client_id_and_exact_configured_redirect(self):
        with tempfile.TemporaryDirectory(prefix="spotify probe test ") as directory:
            config = Path(directory) / ".env"
            config.write_text(f"SPOTIFY_CLIENT_ID={CLIENT_ID}\n", encoding="utf-8")
            self.assertEqual(probe.read_config(config), CLIENT_ID)
            for content in (
                f"SPOTIFY_REDIRECT_URI={probe.REDIRECT_URI}\n",
                "SPOTIFY_CLIENT_ID=not-a-spotify-client-id\n",
                f"SPOTIFY_CLIENT_ID={CLIENT_ID}\n"
                f"SPOTIFY_REDIRECT_URI={probe.REDIRECT_URI}?unexpected=1\n",
                f"SPOTIFY_CLIENT_ID={CLIENT_ID}\n"
                f"SPOTIFY_REDIRECT_URI={probe.REDIRECT_URI.replace('127.0.0.1', 'localhost')}\n",
            ):
                with self.subTest(content=content), self.assertRaises(ValueError):
                    config.write_text(content, encoding="utf-8")
                    probe.read_config(config)
