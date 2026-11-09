# Уточнение текущего голосового контура, 05.11.2026

Ниже сохранено историческое описание. Актуальный кандидат:

`ALSA helper → downmix16k → startup warmup → local personal KWS → speech VAD → UserAudio(turn_id) → manual activityStart/audio/activityEnd → Gemini → locally authorized PCM(epoch,response_id) → actual renderer drain`.

Startup/reconnect/cancel/timeout закрывают turn. Нет модели — acoustic
activation отключена. RMS используется только внутри явно открытой реплики.
Разрешение проверяется до model output/tools и непосредственно перед HA action.
Ответы отменённых запросов не получают ID нового запроса. Mic закрыт на
playback/drain +800мс, то есть это half-duplex; голосовой barge-in не принят.

[Актуальная активация/ограничения](WAKEWORD.md). Офлайн KWS проверен на Mini,
успешный live KWS→модель→слышный ответ пока не принят. Pi не участвовал в этих
voice trials; HA и weather были отключены. Сам runtime — Rust на Mini,
а C audio helpers/vendor ALSA остаются переходной зависимостью.

---

# Текущая архитектура и незакрытые контракты

**Конечная цель владельца:** полностью заменить ПО Яндекса; собственный Rust
runtime/голос/музыка находятся на Mini, Pi обслуживает только Home Assistant.
Ниже описана переходная реализация, не завершённая целевая архитектура.
Обязательные hardware gates: `gpt-mini/docs/TARGET_ARCHITECTURE.md`.
Актуальная сверка исправлений: `gpt-mini/docs/RECHECK_2026-11-02_RU.md`.

На 2026-11-02 текущая ветка — экспериментальный клиент, а не новый Linux
дистрибутив. Штатные DSP/VQE frontend, библиотеки ALSA и аудиодрайвер сохранены.

```text
штатные микрофоны/DSP → yio_audio:9850 → Rust VQE parser
  → InputGate (pre-roll + time-based sizing) → PCM channel → Gemini Live WebSocket
  → model audio (tagged with turn_id) → Rust queue → AudioPlayback (generation kill)
  → mini_pcm_sink (MPCM protocol, 48kHz stereo, peak guard) → vendor ALSA default → динамик
  → tool call → локальная HA policy → REST Home Assistant
```

## Подтверждённое кодом и тестами

Пакеты VQE member159/request и member158/vqe_0 разбираются собственным
клиентом. Формат `vqe_0` проверен на живом устройстве: 16 кГц, 16-бит S16_LE mono,
313 чанков за 5 секунд (62.6 кадров/с, 16 мс на кадр), уровень шума в покое RMS 1.6–1.7.

### Воспроизведение звука (Audio Playback Pipeline)
Штатный бинарник `/system/workdir/bin/aplay` (v1.0.23) имеет несовместимость ABI с vendor
`/system/vendor/quasar/libs/libasound.so.2.0.0`: искажаются `rate` (`187228`) и `start_threshold` (`-148483552`),
из-за чего ALSA не запускает hardware playback (тишина).
Для решения этой проблемы реализован чистый renderer `mini_pcm_sink`:
1. Динамически привязывается к точным версионированным символам ALSA (`ALSA_0.9`, `ALSA_0.9.0rc4`).
2. Принимает поток через stdin по протоколу MPCM (преамбула 16 байт, записи DATA, END, CANCEL).
3. Аппаратный dmixer на Mini требует 48 кГц, stereo (2ch), S16_LE. `AudioPlayback` автоматически
   интерполирует (линейная интерполяция) и дублирует каналы 24 кГц mono -> 48 кГц stereo, а также
   ограничивает амплитуду [-16000, 16000] для защиты `peak_guard` (17000).
4. Завершение реплики (turn complete) отправляет команду END (тип 2), обеспечивая корректный drain.
5. Прерывание (barge-in) немедленно останавливает процесс renderer'а (SIGKILL/CANCEL).

Gemini получает PCM по WebSocket; ответы декодируются из JSON text/binary.
Ожидание `setupComplete` с таймаутом реализовано и проверено на устройстве (517 мс).
При получении `serverContent.interrupted`:
1. `GeminiLiveClient` атомарно инкрементирует `turn_counter` и вызывает `on_interrupted`.
2. `AudioPlayback.interrupt()` инкрементирует поколение (`generation`) и мгновенно убивает активный `aplay` (SIGKILL).
3. Канал динамика типизирован как `(turn_id, Vec<u8>)`. Устаревшие чанки с `turn_id < generation` отбрасываются до постановки в очередь.
4. Проверка поколения после вызова `cmd.spawn()` предотвращает гонку spawn/cancel.

`InputGate` рассчитывает размеры pre-roll буфера (400 мс) и таймаута тишины (2500 мс)
по времени и частоте дискретизации (`bytes_per_ms = (sample_rate * 2) / 1000`),
что обеспечивает инвариантность к размеру входных чанков (256/512/1024 байт).

HA tool call ограничен строгой локальной политикой (`allowed_entities`, типизированные действия).
Проверено на живой сущности `climate.midea_xiao_air_conditioner`.

## Дорожная карта и следующие шаги

- Внедрение оффлайн KWS («Штучка» / «Штучка-Интеллект») по двухступенчатой схеме
  (VAD → KWS на базе pure Rust `tract-onnx` → Cloud Gate), см. [WAKEWORD.md](WAKEWORD.md).
- Обработка обрыва WebSocket и автоматический reconnect с возобновлением контекста.
- Тестирование одновременного воспроизведения речи через ALSA при активном `maind`.
- Подтверждение интерактивного диалога реальным голосом через микрофон Mini.

## Границы зависимостей

В Mini выполняются parsing/routing, сетевой клиент и запуск renderer. В текущей
ветке распознавание/диалог/синтез предоставляет облако Gemini; локальной LLM
или Whistle нет. Raspberry обслуживает Home Assistant и может быть SSH-шлюзом.
Её сетевые роли требуют актуального preflight, не фиксируются по старому IP.

Статический Rust ELF использует glibc helper `mini_pcm_sink` для ALSA и штатный audio stack.
Замена `maind`, переход на OpenAI, Spotify и управление LED/кнопками — отдельные
этапы с собственными критериями приёмки.
