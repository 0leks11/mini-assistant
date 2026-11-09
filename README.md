# mini-assistant

Экспериментальный Rust-клиент для Mini 1 (A113X, ARM32). Цель — собственный
голосовой ассистент с Home Assistant и музыкой без сервисов Яндекса.
Полная замена `maind` и сквозной голосовой цикл пока не подтверждены.

## Что есть сейчас

| Компонент | Статус |
|---|---|
| VQE input | Клиент локального `yio_audio:9850`, parser tests; точный формат речи требует проверки |
| Gemini Live | Провайдер по умолчанию. Локальные WebSocket-тесты покрывают две реплики, переподключение и отмену |
| Playback | Поток в дочерний `aplay`; штатные ALSA libraries/config остаются runtime-зависимостями |
| Home Assistant | REST-клиент и ограниченный локальной политикой executor; реальный API/действия отдельно |
| Wake-word | Локальный персональный KWS. Без эталона остаётся ручная активация. Надёжность на обычной дистанции ожидает приёмки |
| Spotify Connect | Ручной режим с `librespot` ARM32 и PCM-рендерером. Проверка звука и привязки с телефона ожидает Mini |
| LED, кнопки, долговременная память | Не реализованы в текущем приложении |
| OpenAI Realtime | Опциональный провайдер с `OPENAI_API_KEY`, только разговор. Вход через ChatGPT и голосовые инструменты для OpenAI ещё не подключены |

Захват стартует после готовности выбранной сессии. Локальный детектор или
явная ручная активация открывает одну реплику. VAD определяет начало и конец
речи внутри разрешённого окна. Локальные тесты используют заглушки и
синтетический PCM. [Порядок проверки провайдеров и Spotify](docs/SETUP.md)

## Рабочий цикл

Прочитать [AGENTS.md](AGENTS.md), [CONTRIBUTING.md](CONTRIBUTING.md),
[архитектуру](docs/ARCHITECTURE.md) и [SETUP](docs/SETUP.md).

```sh
./scripts/check.sh
```

Настройка локальных файлов сохраняет существующие секреты:

```sh
if [ ! -e .env ]; then (umask 077; cp .env.example .env); fi
if [ ! -e config.toml ]; then (umask 077; cp config.example.toml config.toml); fi
```

`scripts/build.sh` запускает Docker image `messense/rust-musl-cross` для
`armv7-unknown-linux-musleabihf`. `scripts/deploy.sh` изменяет Mini и не является
тестом. Перед ним нужен актуальный preflight и план восстановления.

## Железо и границы

A113X/Cortex-A53, но исследованный userspace — **ELF32 ARM/armhf**, glibc 2.23,
около 236 MiB RAM. Проверять конкретный ELF и native runtime; статическая
линковка Rust не заменяет `aplay`, DSP, kernel driver и ALSA config.

Все настоящие credentials остаются в локальных `.env`/credential store.
Root SSH Mini и авторизация SSH шлюза Pi — разные подключения.
