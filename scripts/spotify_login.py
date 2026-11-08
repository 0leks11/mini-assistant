#!/usr/bin/env python3
"""Spotify OAuth PKCE helper — authenticates on Mac (127.0.0.1:8888) and deploys token to Mini."""

import base64
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import subprocess
import sys
import time
from urllib.parse import parse_qs, urlencode, urlsplit
from urllib.request import Request, build_opener, HTTPRedirectHandler
import webbrowser

REDIRECT_URI = "http://127.0.0.1:8888/callback"
SCOPE = "user-read-playback-state user-modify-playback-state user-read-currently-playing"
AUTHORIZE_URL = "https://accounts.spotify.com/authorize"
TOKEN_URL = "https://accounts.spotify.com/api/token"
PROJECT_DIR = Path(__file__).resolve().parents[1]
ENV_PATH = PROJECT_DIR / ".env"
CALLBACK_TIMEOUT = 240
HTTP_TIMEOUT = 10
MAX_RESPONSE = 128 * 1024


def read_config(path: Path) -> tuple[str, str, int, str]:
    values = {}
    if path.is_file():
        for line in path.read_text(encoding="utf-8").splitlines():
            match = re.fullmatch(
                r"\s*(?:export\s+)?(SPOTIFY_CLIENT_ID|MINI_HOST|MINI_PORT|MINI_SSH_KEY)\s*=\s*(.*)",
                line,
            )
            if not match:
                continue
            try:
                fields = shlex.split(match[2], comments=True)
            except ValueError:
                continue
            if len(fields) == 1:
                values[match[1]] = fields[0]

    client_id = os.environ.get("SPOTIFY_CLIENT_ID", values.get("SPOTIFY_CLIENT_ID", ""))
    mini_host = os.environ.get("MINI_HOST", values.get("MINI_HOST", "192.168.1.110"))
    mini_port = int(os.environ.get("MINI_PORT", values.get("MINI_PORT", 2222)))
    mini_key = os.environ.get("MINI_SSH_KEY", values.get("MINI_SSH_KEY", ""))

    if not re.fullmatch(r"[a-fA-F0-9]{32}", client_id):
        raise ValueError("SPOTIFY_CLIENT_ID не найден или имеет неверный формат в .env.")
    return client_id, mini_host, mini_port, mini_key


def make_authorization(client_id: str, state: str, verifier: str) -> str:
    digest = hashlib.sha256(verifier.encode("ascii")).digest()
    challenge = base64.urlsafe_b64encode(digest).decode("ascii").rstrip("=")
    return AUTHORIZE_URL + "?" + urlencode({
        "client_id": client_id,
        "response_type": "code",
        "redirect_uri": REDIRECT_URI,
        "scope": SCOPE,
        "state": state,
        "code_challenge_method": "S256",
        "code_challenge": challenge,
        "show_dialog": "true",
    })


def parse_callback(target: str, expected_state: str) -> tuple[str | None, str | None]:
    if len(target) > 8192:
        raise ValueError("Callback is too large")
    path = urlsplit(target)
    if path.scheme or path.netloc or path.path != "/callback" or path.fragment:
        raise ValueError("Unexpected callback path")
    params = parse_qs(path.query, keep_blank_values=True, strict_parsing=True, max_num_fields=12)
    state = params.get("state", [])
    if len(state) != 1 or not state[0].isascii() or not secrets.compare_digest(state[0], expected_state):
        raise ValueError("Callback state mismatch")
    code = params.get("code", [])
    error = params.get("error", [])
    if len(code) == 1 and code[0] and not error:
        return code[0], None
    if len(error) == 1 and error[0] and not code:
        return None, error[0]
    raise ValueError("Expected one callback code or error")


def browser_code(client_id: str, verifier: str) -> str:
    state = secrets.token_urlsafe(24)
    result = None

    class Callback(BaseHTTPRequestHandler):
        def setup(self):
            self.request.settimeout(5)
            super().setup()

        def log_message(self, _format, *_args):
            pass

        def do_GET(self):
            nonlocal result
            try:
                if self.headers.get_all("Host") != ["127.0.0.1:8888"]:
                    raise ValueError("Unexpected host")
                callback = parse_callback(self.path, state)
            except ValueError as e:
                self.respond(400, f"Ошибка: {e}")
                return
            result = callback
            html = """<!DOCTYPE html><html><head><meta charset='utf-8'></head>
            <body style='font-family:sans-serif;text-align:center;padding:50px;background:#0d1117;color:#fff;'>
            <h1 style='color:#1db954;'>Авторизация Spotify успешна!</h1>
            <p>Токен отправляется на Яндекс Станцию Mini. Можете закрыть эту вкладку.</p>
            </body></html>"""
            body = html.encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def respond(self, status: int, message: str):
            body = message.encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    try:
        server = HTTPServer(("127.0.0.1", 8888), Callback)
    except OSError:
        raise ValueError("Порт 127.0.0.1:8888 занят другим процессом.") from None

    with server:
        auth_url = make_authorization(client_id, state, verifier)
        print(f"Открываю браузер для входа в Spotify...\nURL: {auth_url}\n", flush=True)
        webbrowser.open(auth_url)
        print("Ожидаю нажатия Agree в браузере (до 4 минут)...", flush=True)
        deadline = time.monotonic() + CALLBACK_TIMEOUT
        while result is None and time.monotonic() < deadline:
            server.timeout = min(1, max(0, deadline - time.monotonic()))
            server.handle_request()

    if result is None:
        raise ValueError("Окно авторизации истекло.")
    code, error = result
    if error is not None or code is None:
        raise ValueError(f"Spotify вернул ошибку: {error}")
    return code


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


def exchange_code(client_id: str, code: str, verifier: str) -> str:
    form = urlencode({
        "grant_type": "authorization_code",
        "client_id": client_id,
        "code": code,
        "redirect_uri": REDIRECT_URI,
        "code_verifier": verifier,
    }).encode("ascii")

    req = Request(TOKEN_URL, data=form, method="POST", headers={
        "Content-Type": "application/x-www-form-urlencoded",
    })

    with build_opener(NoRedirect()).open(req, timeout=HTTP_TIMEOUT) as response:
        payload = json.loads(response.read(MAX_RESPONSE))

    refresh_token = payload.get("refresh_token")
    if not refresh_token:
        raise ValueError(f"Spotify не вернул refresh_token: {payload}")
    return refresh_token


def deploy_token_to_mini(host: str, port: int, key: str, token: str) -> None:
    print(f"Отправка токена на Станцию Mini ({host}:{port})...", flush=True)
    ssh_cmd = [
        "ssh", "-p", str(port),
        "-o", "StrictHostKeyChecking=accept-new",
        "-o", "BatchMode=yes",
    ]
    if key and os.path.isfile(key):
        ssh_cmd.extend(["-i", key])
    ssh_cmd.append(f"root@{host}")

    remote_script = f"""
set -e
mkdir -p /data/opt/mini-assistant /bkupgrade/data/opt/mini-assistant
cat > /data/opt/mini-assistant/spotify_token.txt << 'EOF'
{token}
EOF
chmod 600 /data/opt/mini-assistant/spotify_token.txt
cp /data/opt/mini-assistant/spotify_token.txt /bkupgrade/data/opt/mini-assistant/spotify_token.txt
chmod 600 /bkupgrade/data/opt/mini-assistant/spotify_token.txt
pkill -9 mini-assistant || true
"""
    subprocess.run(ssh_cmd + [remote_script], check=True, capture_output=True)
    print("Токен успешно сохранён на Станции! mini-assistant перезапущен.", flush=True)


def main() -> int:
    try:
        client_id, host, port, key = read_config(ENV_PATH)
        verifier = secrets.token_urlsafe(48)
        code = browser_code(client_id, verifier)
        print("Код получен! Обмениваю на постоянный refresh_token...", flush=True)
        token = exchange_code(client_id, code, verifier)
        deploy_token_to_mini(host, port, key, token)
        print("\n🎉 ГОТОВО! Станция Мини подключена к Spotify Web API!")
        print(f"Проверить статус: http://{host}:8080/")
        return 0
    except Exception as e:
        print(f"\n❌ Ошибка: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
