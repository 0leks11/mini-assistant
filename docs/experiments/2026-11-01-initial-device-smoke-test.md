# Опыт: Первый запуск mini-assistant на железе Яндекс Станции Мини

- Дата / версия кода / ответственный: 2026-11-01 / mini-assistant git:8ece520 / Antigravity & Oleksii
- Основание в задаче пользователя: Запрос пользователя на запуск и проверку автономного ассистента на Станции Мини.

## Гипотеза и измерение

- **Гипотеза:** Статический бинарник `mini-assistant` (ARM32 musl, 2.3 МБ):
  1. Успешно запускается в userspace Mini 1 (A113X, glibc 2.23 / Linux 3.10 kernel).
  2. Подключается к локальному `yio_audio:9850` IPC (member 159 `request_vqe`) и получает кадры PCM 16 кГц.
  3. Устанавливает WebSocket-сессию с Gemini Live API (`models/gemini-3.8-live`) и выполняет синхронизацию с Home Assistant.
  4. Воспроизводит входящий звук через `/system/workdir/bin/aplay`.
- **Что не проверяется:** Долговременная стабильность (тест рассчитан на короткий smoke-прогон до 2 минут), Spotify (librespot), аппаратные кнопки и LED.

## Preflight

- Устройство: Yandex Station Mini 1 (YNDX-0004), A113X, 236 МБ RAM.
- Сеть: Mini `192.168.77.110:2222` (Lab AP `shergo`), шлюз/Pi `172.17.0.248` (`192.168.77.2`).
- Службы: `maind` работает штатно (hybrid-режим, IPC `yio_audio` активен).
- Бинарник: `target/armv7-unknown-linux-musleabihf/release/mini-assistant` (SHA256: проверен, 2.3 МБ, static ELF32).

## Действия и пределы

- Изменяемый каталог на Mini: строго изолированный `/data/opt/mini-assistant/` (никакие системные разделы `/system`, `/etc` не модифицируются).
- Аудиопоток: PCM S16_LE 16 кГц моно (захват через IPC `vqe_0`) $\rightarrow$ base64 в Gemini Live API $\rightarrow$ PCM S16_LE 24 кГц моно в `/system/workdir/bin/aplay`.
- Таймаут опыта: ручной запуск, остановка по Ctrl+C через 1-2 минуты.
- Ограничение приватности: звук не пишется в файлы на диск, логи не содержат raw PCM и секретов.

## Восстановление

- Остановка процесса: сигнал SIGINT (Ctrl+C). При завершении `VqeCapture::drop` отправляет unsubscribe (field 6) в `yio_audio`.
- Откат: `rm -rf /data/opt/mini-assistant`.
- Штатные демоны Яндекса (`maind`, `platformd`) не перезапускаются и не затрагиваются.

## Результат

- Ожидает проведения на стенде.
