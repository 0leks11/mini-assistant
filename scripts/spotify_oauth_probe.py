#!/usr/bin/env python3
"""One local, read-only Spotify OAuth PKCE probe. Credentials stay in memory."""

import argparse
import base64
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import sys
import time
from urllib.error import HTTPError, URLError
from urllib.parse import parse_qs, urlencode, urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener
import webbrowser


REDIRECT_URI = "http://127.0.0.1:8888/callback"
SCOPE = "user-read-playback-state"
AUTHORIZE_URL = "https://accounts.spotify.com/authorize"
TOKEN_URL = "https://accounts.spotify.com/api/token"
PLAYER_URL = "https://api.spotify.com/v1/me/player"
ENV_PATH = Path(__file__).resolve().parents[1] / ".env"
CALLBACK_TIMEOUT = 240
HTTP_TIMEOUT = 10
MAX_RESPONSE = 128 * 1024


def read_config(path: Path) -> str:
    values = {}
    if path.is_file():
        for line in path.read_text(encoding="utf-8").splitlines():
            match = re.fullmatch(
                r"\s*(?:export\s+)?(SPOTIFY_CLIENT_ID|SPOTIFY_REDIRECT_URI)\s*=\s*(.*)", line
            )
            if not match:
                continue
            try:
                fields = shlex.split(match[2], comments=True)
            except ValueError:
                raise ValueError(f"Некорректная строка {match[1]} в .env.") from None
            if len(fields) != 1 or match[1] in values:
                raise ValueError(f"Нужно одно значение {match[1]} в .env.")
            values[match[1]] = fields[0]
    client_id = os.environ.get("SPOTIFY_CLIENT_ID", values.get("SPOTIFY_CLIENT_ID", ""))
    redirect = os.environ.get(
        "SPOTIFY_REDIRECT_URI", values.get("SPOTIFY_REDIRECT_URI", REDIRECT_URI)
    )
    if not re.fullmatch(r"[a-fA-F0-9]{32}", client_id):
        raise ValueError("Добавь SPOTIFY_CLIENT_ID из Spotify Dashboard в локальный .env.")
    if redirect != REDIRECT_URI:
        raise ValueError(f"Для этой пробы SPOTIFY_REDIRECT_URI должен быть {REDIRECT_URI}.")
    return client_id


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
            self.request.settimeout(3)
            super().setup()

        def log_message(self, _format, *_args):
            # The request path contains an authorization code.
            pass

        def do_GET(self):
            nonlocal result
            try:
                if self.headers.get_all("Host") != ["127.0.0.1:8888"]:
                    raise ValueError("Unexpected host")
                callback = parse_callback(self.path, state)
            except ValueError:
                self.respond(400, "Запрос отклонён. Вернись на страницу входа Spotify.")
                return
            result = callback
            self.respond(200, "Ответ Spotify получен. Результат проверки появится в терминале.")

        def respond(self, status: int, message: str):
            body = message.encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("Referrer-Policy", "no-referrer")
            self.end_headers()
            self.wfile.write(body)

    try:
        server = HTTPServer(("127.0.0.1", 8888), Callback)
    except OSError:
        raise ValueError("Не удалось открыть локальный callback на 127.0.0.1:8888.") from None
    with server:
        deadline = time.monotonic() + CALLBACK_TIMEOUT
        if not webbrowser.open(make_authorization(client_id, state, verifier)):
            raise ValueError("Не удалось открыть страницу входа Spotify в браузере.")
        print("Жду подтверждения в браузере. Окно действует четыре минуты.", flush=True)
        while result is None and time.monotonic() < deadline:
            server.timeout = min(1, max(0, deadline - time.monotonic()))
            server.handle_request()
    if result is None:
        raise ValueError("Окно авторизации истекло.")
    code, error = result
    if error is not None or code is None:
        raise ValueError("Вход в Spotify был отменён или отклонён.")
    return code


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


def request_json(request: Request, stage: str) -> tuple[int, dict]:
    try:
        with build_opener(NoRedirect()).open(request, timeout=HTTP_TIMEOUT) as response:
            if response.status == 204:
                return 204, {}
            raw = response.read(MAX_RESPONSE + 1)
            if len(raw) > MAX_RESPONSE:
                raise ValueError(f"{stage}. Ответ слишком большой.")
            payload = json.loads(raw)
            if not isinstance(payload, dict):
                raise ValueError(f"{stage}. Ожидался объект JSON.")
            return response.status, payload
    except HTTPError as error:
        with error:
            raise ValueError(f"{stage}. Spotify ответил HTTP {error.code}.") from None
    except (URLError, OSError):
        raise ValueError(f"{stage}. Сетевой запрос не завершился.") from None
    except (json.JSONDecodeError, UnicodeDecodeError):
        raise ValueError(f"{stage}. Некорректный JSON.") from None


def probe(client_id: str, *, show_track: bool) -> None:
    verifier = secrets.token_urlsafe(48)
    code = browser_code(client_id, verifier)
    form = urlencode({
        "grant_type": "authorization_code",
        "client_id": client_id,
        "code": code,
        "redirect_uri": REDIRECT_URI,
        "code_verifier": verifier,
    }).encode("ascii")
    _, token = request_json(Request(TOKEN_URL, data=form, method="POST", headers={
        "Content-Type": "application/x-www-form-urlencoded",
    }), "Обмен кода")
    access = token.pop("access_token", None)
    granted = token.get("scope", "")
    token.clear()
    if not isinstance(access, str) or not re.fullmatch(r"[A-Za-z0-9._~+/=-]{1,4096}", access):
        raise ValueError("Spotify не вернул подходящий access token.")
    if not isinstance(granted, str) or SCOPE not in granted.split():
        raise ValueError("Spotify не предоставил право чтения состояния плеера.")
    status, player = request_json(Request(PLAYER_URL, method="GET", headers={
        "Authorization": f"Bearer {access}",
    }), "Чтение плеера")
    if status == 204:
        print("OAuth принят. Плеер ответил 204, активное воспроизведение отсутствует.")
        return
    device = player.get("device") or {}
    item = player.get("item") or {}
    kind = device.get("type") if isinstance(device, dict) else None
    kind = kind if kind in ("Computer", "Smartphone", "Speaker", "Tablet", "TV") else "unknown"
    print(f"OAuth принят. Плеер ответил HTTP {status}. Тип устройства {kind}.")
    print(f"Воспроизведение активно — {'да' if player.get('is_playing') is True else 'нет'}. ")
    if show_track and isinstance(item, dict) and isinstance(item.get("name"), str):
        title = "".join(c for c in item["name"] if c.isprintable())[:180]
        print(f"Текущая композиция — {title}.")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--show-track", action="store_true", help="Show track title in your terminal")
    args = parser.parse_args()
    try:
        probe(read_config(ENV_PATH), show_track=args.show_track)
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 1
    except OSError:
        print("Локальная операция не завершилась.", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
