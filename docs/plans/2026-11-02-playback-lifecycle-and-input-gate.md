# План: Устранение blockers playback cancel, handshake и input gate

- Статус: prototype integrated / release acceptance incomplete
- Дата: 2026-11-02
- Репозиторий и владелец: mini-assistant (Antigravity & Oleksii)

## Уточнение по независимому ревью 02.11

Сквозные generation IDs, post-spawn cancel guard и расчёт InputGate по времени
есть в текущем коде. Штатные проверки снимка 14:24: 42 Rust + 8 deployment
tests, fmt/clippy — PASS. Это не закрывает весь lifecycle.

Диагностические fake-process/WS tests воспроизвели: старый END timer снимает
is_playing во время следующего ответа; incoming reader задерживает interruption
пока ждёт tool reply; private transcript попадает в INFO. Доставка не включает
native helper и не сравнивает hashes. Открытые задачи и критерии:
`gpt-mini/docs/GENERAL_PLAN.md`, R1–R8.

Ниже сохранены результаты предыдущего этапа. Время жизни PID/RSS-снимок не
доказывают отсутствие утечек или живую WS-сессию; число принятых VQE кадров
не определяет процент потерь без ожидаемого sequence/counter.

## Задача и критерии готовности

1. **Немедленная остановка воспроизведения (Playback Cancel & Drain):**
   - Вызов `interrupt()` немедленно убивает активный дочерний процесс `aplay` и сбрасывает все накопленные в очереди аудиочанки текущего ответа.
   - Потокобезопасная сквозная передача поколений: `(turn_id, Vec<u8>)` передаются от `GeminiLiveClient` через `tx_speaker` в `AudioPlayback::play`.
   - Устранена гонка spawn/cancel: проверка поколения после `cmd.spawn()` мгновенно завершает дочерний процесс, если прерывание произошло во время spawn.
2. **Ожидание `setupComplete` перед отправкой аудио:**
   - Клиент `GeminiLiveClient` дожидается официального подтверждения `setupComplete` от сервера с таймаутом (10 с) перед передачей потока микрофона.
   - Ошибка авторизации (неверный ключ) или неверная модель возвращают контролируемый `Err`, предотвращая передачу данных.
3. **Локальный шлюз активации аудио (Input Gate / Wake Word Gate):**
   - Расчет размеров pre-roll буфера (400 мс) и таймаута тишины (2500 мс) по времени и sample rate (`bytes_per_ms = (sample_rate * 2) / 1000`).
   - Инвариантность к размеру аудиокадров (протестировано на 256, 512 и 1024 байтах).
   - Подготовлена дорожная карта перехода от VAD к оффлайн KWS («Штучка» / «Штучка-Интеллект»), см. `docs/WAKEWORD.md`.

## Исходное состояние и ограничения

- Хост Mini 1: ARM32 musl static, A113X, Linux 4.9.68.
- Сеть Mini: Wi-Fi `192.168.77.110:2222`, gateway `192.168.77.2` (Pi), DNS `8.8.8.8` проверены.
- SSH-автозапуск `S99zzmini_ssh`: успешно подтверждён на холодном старте (boot_id `d428e9f4-b612-4dc1-85bd-a9cbb638e786`, PID 1847).
- Исходно планировался `aplay` через ALSA `default`; текущий runtime предпочитает
  `mini_pcm_sink` с vendor libraries. Совместимость helper и его lab-limits
  проверяются отдельно; прямой `hw:0,0` не принят как безопасная замена.

## Шаги

- [x] Шаг 1: Подтверждение холодного старта Dropbear SSH на устройстве (boot_id `d428e9f4-b612-4dc1-85bd-a9cbb638e786`, PID 1847).
- [x] Шаг 2: Реализация generation-based playback cancellation со сквозной типизацией `(turn_id, Vec<u8>)` и устранением spawn/cancel race в `src/audio/playback.rs` + unit tests.
- [x] Шаг 3: Реализация ожидания `setupComplete` в `src/gemini/live_client.rs` + contract tests.
- [x] Шаг 4: Рефакторинг `InputGate` на расчет по миллисекундам и sample rate + unit tests.
- [x] Шаг 5: Полный локальный проверочный прогон (`scripts/check.sh` и `check_workflow.py`): 38 Rust тестов, 8 deploy тестов, 12 root тестов PASS.
- [x] Шаг 6: ARM32 сборка (SHA256: `4c3c928a9ea085938d3aaab63b38e27c4937f358f9699b2d445ae8fdf8296785`), деплой через `deploy.sh` и многочасовой тест стабильности на устройстве.

## Проверки, результаты и измерения на устройстве

- `cargo test --bin mini-assistant`: 38/38 PASS.
- `mini-assistant/scripts/check.sh`: 38 Rust тестов + 8 deploy тестов PASS, clippy 0 warnings.
- `scripts/check_workflow.py`: PASS (16 indexed files).
- Длительный тест на устройстве (PID 6470):
  - В предыдущем журнале заявлено **>7.5 часов жизни процесса**. Отсутствие
    сбоев backend и healthcheck за этот интервал отдельно не доказаны.
  - VmRSS: **2 480 кБ (~2.4 МБ)** в наблюдении; peak/отсутствие утечек не измерены.
  - Поток VQE IPC: **313 принятых кадров за 5 секунд** (62.6 кадров/с).
    Процент потерь без ожидаемого счётчика неизвестен.
  - Акустический фон в покое: **RMS 1.6–1.7**, шлюз стабильно закрыт (`gate: GATED`).
  - Home Assistant: обнаружен `climate.midea_xiao_air_conditioner`, настроена белая политика.
