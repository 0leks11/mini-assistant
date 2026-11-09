# Активация голоса: обновление 05.11.2026

Реализованы раздельные KWS и speech VAD. RMS больше не активирует production
диалог. Персональный MFCC/DTW matcher на Rustpotter работает локально в Rust;
это не ASR или LLM. Источник/Apache-2.0/изменения:
[reference-only adaptation](../vendor/rustpotter-ref/README.MINI.md).

- `MINI_WAKEWORD_MODEL` / `assistant.wakeword_model` задаёт приватный `.rpw`.
  Default — `/bkupgrade/data/opt/mini-assistant/keyword.rpw`.
- Отсутствующий/повреждённый шаблон закрывает acoustic activation. Нет fallback
  на громкость. Явный SIGUSR1 открывает ручное окно 8 секунд; SIGUSR2 отменяет
  текущий turn и вывод. Сигналы направляются только в проверенный PID процесса.
- KWS использует исходный mono16k PCM до MINI_MIC_GAIN. После KWS отдельный
  VAD продолжает реплику; pre-roll 2000 мс, default silence 1000 мс, предел
  реплики 30 с, разрешение turn 120 с. Startup/reconnect ничего не активируют.
- Первые 2 секунды PCM после запуска capture отбрасываются: на Mini измерен
  сильный стартовый DC transient. При playback mic закрыт до реального drain
  и ещё 800 мс. Это half-duplex: голосовое прерывание поверх ответа не принято.

Три записи владельца подготовлены через тот же mic/downmix/decimator Mini,
локально и без облака. Начальный trimming ошибочно сохранял фон; исправлено
выделением связного участка вокруг максимума. Приватный шаблон — 10 350 байт,
3 references по 27–33 MFCC frames. Фиксированные scores: median 0.65,
average 0.60; это similarity, не вероятности. Одна отдельная положительная
проба сработала, обычный вопрос и короткий фон — нет. Из 3 исходных training
клипов сработали 2. Это предварительная персональная настройка, а не оценка
false accepts/hour или универсальной точности.

На Mini `keyword-enroll test`: 2 с WAV + 2 с synthetic tail обработаны за
253 мс, RTF 0.063. Измерение короткое, не является CPU/RAM или latency SLA.
Длительный фон/ТВ/похожие слова/разные расстояния пока не приняты.

Офлайн команды (не включают микрофон или сеть):

```sh
cargo run --locked --bin keyword-enroll -- build keyword.rpw sample1.wav sample2.wav sample3.wav
cargo run --locked --bin keyword-enroll -- validate keyword.rpw
cargo run --locked --bin keyword-enroll -- test keyword.rpw held-out.wav
```

Образцы и шаблоны приватны, не коммитятся. CLI не перезаписывает существующий
артефакт. Утилита требует mono16k PCM16, ограничивает размеры/длительность;
loader до allocation проверяет CBOR и semantic shape. Host fixtures не
подтверждают акустическую точность. Облачный провайдер пока Gemini prototype;
выбор конечного провайдера и полная миграция ОС не закрыты.

## Подготовка корпуса: уточнение 05.11

Добавлена offline команда `keyword-enroll build-segments OUTPUT.rpw MANIFEST.json`.
Она читает3–8 относительных WAV-сегментов с start_ms/end_ms, сохраняет тихое
начало без auto-trim. Исходник не более60с, сегмент0.3–3с. Это enrollment
selector, а не полный dataset manifest. Дополнительные поля не принимаются.

`test` теперь даёт offsets конца обработанных кадров, отдельно для audio и
synthetic tail. Последний input frame может содержать padding, указанный в
input_frame_padding_samples; hit на нём может иметь timestamp позже EOF.
Offsets не равны фонетическим границам/задержке без разметки.

Новый poolA записан локально и содержит8подтверждённых обращений четырёх форм.
Текущий baseline на нём не сработал; готовность KWS по-прежнему не принята.
Расширение данных должно сопровождаться calibration/negative tests и проверкой
границ, а не автоматической загрузкой всех фраз в один median-reference.
Практический план находится в root `docs/plans/2026-11-05-wakeword-corpus-and-voices.md`.

---

## Исторический план 02.11 — не описание актуального runtime

# Wake word «Штучка»: исследовательский план

Обновлено: 02.11.2026. Статус — **KWS ещё не реализован**. Этот документ
задаёт проверяемые гипотезы; он не является benchmark или обещанием приватности.
Общая очередь исправлений: `gpt-mini/docs/GENERAL_PLAN.md`, R1–R8.

## Текущий путь

`VQE PCM → InputGate(SimpleVadEngine) → Gemini Live`.

- В main.rs defaults: VAD threshold 30.0, mic gain 20.0x. Их можно переопределить
  окружением; значения ниже — не свидетельство реально загруженного .env.
- RMS измеряется до программного усиления. Значения 200.0 и 1.6–1.7 из
  предыдущего текста относились к ранней калибровке, а не текущему default.
- Pre-roll рассчитывается на 400 мс, closing silence — на 2500 мс при 16 kHz.
  Это исправляет прежнюю зависимость времени от размера входного chunk.
- VAD реагирует на энергию любой речи/музыки. Список wake_words только хранится
  в config и логируется: ключевое слово не проверяется.
- Пока playback.is_playing, вход отбрасывается. Пользовательское прерывание
  голосом поверх ответа этим путём тоже не обнаруживается.

## Требования к новой интеграции

Целевые фразы: «Штучка», «Штучка-Интеллект». Нужны раздельные роли:

1. **ActivationDetector** локально определяет ключевую фразу, до этого cloud gate закрыт.
2. **SpeechActivity** продолжает реплику по VAD после one-shot активации.
3. **InputGate** управляет pre-roll, конечными состояниями, timeout, mute и reset.
4. **Session supervisor** принимает только разрешённый аудиопоток и закрывает его
   при ошибках backend. Нужны отдельные состояния READY/LISTENING/SPEAKING.

Подстановка one-shot KWS в прежний единый WakeWordEngine недостаточна:
сейчас один engine определяет и активацию, и продолжение речи. Следующие слова
могут не содержать wake word и ошибочно закрыть gate.

Опциональный VAD prefilter должен сохранять нужный контекст признаков. Простой
пропуск тихих кадров может ухудшить KWS. Вычисление VAD/буферов продолжает
потреблять CPU даже когда inference не вызывается; «0% CPU» не обещаем.

## Ограничения Mini и выбор кандидата

Подтверждены: A113X/Cortex-A53; userspace ELF32 ARM/armhf, vendor glibc 2.23,
около 236 MiB общей RAM. Rust-клиент использует musl target. Это не доказывает
статическую линковку будущих native dependencies. Доступная память, CPU и
температура зависят от одновременно работающих vendor/frontend служб.

Пока **не выбраны** модель и runtime. Исследовательские ветки:

| Кандидат | Что нужно доказать до выбора |
|---|---|
| tract + ONNX KWS / openWakeWord pipeline | Поддержка ops/features, весь embedding/frontend, русский датасет, ARM32 build, реальная память и скорость |
| Vosk с ограниченной грамматикой | Русский словарь/качество на двух фразах, память decoder, ABI и native dependencies |
| Sherpa-ONNX KWS | Доступность подходящей русской KWS-модели/tokenizer, ARM32 runtime, RSS/CPU и качество |

Ранее приведённые 5–8 MiB RAM / 3–7% CPU для tract и 25 MiB для Vosk
**не были измерены на Mini и отозваны как результат**. Язык реализации не
гарантирует безопасность всей цепочки, отсутствие native dependencies или
подходящий inference budget. Disk size модели не равен RSS процесса.

Бюджет RAM/CPU — входные ограничения, которые сначала согласуют по preflight
стенда. До измерений нельзя считать один вариант лучшим или совместимым.

## Конкретные опыты и критерии приёмки

1. Зафиксировать snapshot, модель/лицензию/hash, toolchain, feature pipeline,
   ABI/NEEDED, firmware и наличие служб. Секреты/записи речи в Git не добавлять.
2. Offline corpus: целевые слова с вариантами произношения, обычные вопросы,
   похожие слова, музыка и шум; отдельные train/validation/test наборы.
3. Измерить false accepts на час фонового звука и false rejects целевых фраз,
   задержку от конца слова. Порог выбирается по этим данным, не по тихой комнате.
4. Contract tests gate: до trigger → 0 cloud frames; после trigger вопрос
   продолжается без нового слова; тишина/mute/timeout закрывают gate.
5. Перед аппаратным опытом закрыть R1 logging и согласовать сценарий.
   Затем measured peak RSS, CPU/core, inference latency, temperature и
   одновременную работу audio frontend на Mini с ограничением времени.
6. Проверить reset между репликами, разные chunk boundaries, длинную речь,
   interrupted playback и смену/потерю backend. Cleanup должен быть явным.

Ложные активации вероятностны. Приватность — свойство всей цепочки
KWS/gate/logging/transport, которое проверяется отрицательными тестами;
«100% приватность» и «абсолютная надёжность» не являются результатом проекта.
