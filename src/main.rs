use anyhow::{Context, Result};
use config::VoiceProvider;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

pub mod alarms;
mod audio;
pub mod buttons;
mod config;
mod gemini;
mod ha;
mod interaction;
mod spotify;
mod wakeword;
pub mod weather;
pub mod wifi;

#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    Run {
        config_path: Option<std::path::PathBuf>,
        dry_run: bool,
        alarms_only: bool,
        conversation_only: bool,
    },
    Check {
        config_path: Option<std::path::PathBuf>,
        conversation_only: bool,
    },
    Version,
    Help,
}

enum VoiceClient {
    Gemini(gemini::GeminiLiveClient),
}

impl VoiceClient {
    fn status_handle(&self) -> Arc<std::sync::atomic::AtomicU8> {
        match self {
            Self::Gemini(client) => client.status_handle(),
        }
    }

    async fn stop(&mut self) {
        match self {
            Self::Gemini(client) => client.stop().await,
        }
    }
}

fn parse_cli_args_from<I, T>(iter: I) -> Result<CliAction>
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    let mut args = iter.into_iter();
    let _prog = args.next();
    let mut config_path = None;
    let mut check = false;
    let mut dry_run = false;
    let mut alarms_only = false;
    let mut conversation_only = false;

    while let Some(arg) = args.next() {
        let s = arg.as_ref();
        match s {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-v" | "--version" => return Ok(CliAction::Version),
            "--check" => check = true,
            "--dry-run" => dry_run = true,
            "--alarms-only" => alarms_only = true,
            "--conversation-only" => conversation_only = true,
            "-c" | "--config" => {
                let val = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--config requires a file path argument"))?;
                config_path = Some(std::path::PathBuf::from(val.as_ref()));
            }
            other if other.starts_with("--config=") => {
                let val = &other["--config=".len()..];
                config_path = Some(std::path::PathBuf::from(val));
            }
            unknown => {
                anyhow::bail!(
                    "Unknown argument: '{unknown}'\n\
                    Usage: mini-assistant [OPTIONS]\n\n\
                    Try 'mini-assistant --help' for more options."
                );
            }
        }
    }

    if check {
        anyhow::ensure!(!alarms_only, "--check conflicts with --alarms-only");
        Ok(CliAction::Check {
            config_path,
            conversation_only,
        })
    } else {
        anyhow::ensure!(!alarms_only || !conversation_only, "Conflicting run modes");
        Ok(CliAction::Run {
            config_path,
            dry_run,
            alarms_only,
            conversation_only,
        })
    }
}

async fn run_preflight_check(
    config_path: Option<&std::path::Path>,
    conversation_only: bool,
) -> Result<()> {
    eprintln!("🔍 Running mini-assistant preflight diagnostics...");

    // 1. Config loading
    let config = config::Config::load_from(config_path)
        .context("Preflight FAIL: Configuration load error")?;
    eprintln!("✅ [OK] Config loaded successfully");
    eprintln!("       Assistant Name: {}", config.assistant.name);
    let voice = config.voice_settings()?;
    let provider = voice.provider;
    eprintln!(
        "       Voice provider: {} ({})",
        provider.name(),
        voice.model
    );
    eprintln!("       Audio Backend:  {}", config.audio.capture_backend);

    let api_key = voice
        .api_key
        .with_context(|| format!("Preflight FAIL: {} is not configured", provider.key_env()))?;

    // 3. Home Assistant authentication check (read-only verification)
    if let Some(ha_cfg) = config
        .ha
        .as_ref()
        .filter(|_| !conversation_only && provider == VoiceProvider::Gemini)
    {
        if let Some(ref token) = ha_cfg.token {
            if !token.trim().is_empty() && !ha_cfg.allowed_entities.is_empty() {
                eprintln!("⏳ Testing Home Assistant connection to {}...", ha_cfg.url);
                let client = ha::HomeAssistantClient::new(
                    ha_cfg.url.clone(),
                    token,
                    &ha_cfg.allowed_entities,
                )
                .context("Preflight FAIL: Invalid Home Assistant client configuration")?;

                let auth_msg = client
                    .verify_auth()
                    .await
                    .context("Preflight FAIL: Home Assistant authorization rejected")?;
                eprintln!("✅ [OK] Home Assistant token authorized: \"{auth_msg}\"");

                let states = client
                    .get_states()
                    .await
                    .context("Preflight FAIL: Failed to query Home Assistant allowed entities")?;
                eprintln!(
                    "✅ [OK] Home Assistant allowed entities accessible: {} entities found",
                    states.len()
                );
                for entity in &states {
                    eprintln!("       - [{}] state: {}", entity.entity_id, entity.state);
                }
            } else {
                eprintln!("ℹ️ [SKIP] Home Assistant configured without token or allowed entities");
            }
        } else {
            eprintln!("ℹ️ [SKIP] Home Assistant configured without token");
        }
    } else {
        eprintln!("ℹ️ [SKIP] Home Assistant is not configured");
    }

    // Verify the selected provider without opening either audio device.
    let system_instruction = "Preflight check session".to_owned();
    match provider {
        VoiceProvider::Gemini => {
            gemini::GeminiLiveClient::verify_handshake(&gemini::LiveClientConfig {
                api_key,
                model: voice.model.to_owned(),
                voice_name: voice.voice.to_owned(),
                system_instruction,
                has_ha_tools: false,
                has_weather_tools: false,
                has_alarm_tools: false,
                has_spotify_tools: false,
                has_stop_dialogue: false,
                endpoint: None,
                cancellation_epoch: None,
                authorization: None,
                suppress_followup: None,
            })
            .await
        }
    }
    .with_context(|| format!("Preflight FAIL: {} setup handshake failed", provider.name()))?;
    eprintln!("✅ [OK] {} setup handshake completed", provider.name());

    eprintln!("\n🎉 All preflight diagnostic checks PASSED.");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // 0. Parse CLI arguments before initializing logger or background services
    let action = parse_cli_args_from(std::env::args())?;
    match action {
        CliAction::Help => {
            println!(
                "mini-assistant v0.1.0 — Voice assistant for GPT Mini (ARM32 musl)\n\n\
                Usage: mini-assistant [OPTIONS]\n\n\
                Options:\n  \
                  -c, --config <PATH>  Path to config.toml\n  \
                  --check              Verify provider connection without audio and exit\n  \
                  --dry-run            Mock audio capture/playback (provider network still used)\n  \
                  --conversation-only Voice test with no HA, weather or alarm services\n  \
                  --alarms-only        Run configured local alarms without voice services\n  \
                  --spotify-connect <PATH> Run a standalone Spotify Connect receiver\n  \
                  -v, --version        Print version information and exit\n  \
                  -h, --help           Print help information and exit"
            );
            return Ok(());
        }
        CliAction::Version => {
            println!("mini-assistant v0.1.0");
            return Ok(());
        }
        CliAction::Check {
            config_path,
            conversation_only,
        } => {
            if dotenvy::dotenv().is_err() {
                let fallback = std::path::Path::new("/data/opt/mini-assistant/.env");
                if fallback.exists() {
                    let _ = dotenvy::from_path(fallback);
                }
            }
            return run_preflight_check(config_path.as_deref(), conversation_only).await;
        }
        CliAction::Run {
            config_path,
            dry_run,
            alarms_only,
            conversation_only,
        } => {
            // 1. Load environment variables (.env)
            if dotenvy::dotenv().is_err() {
                let fallback = std::path::Path::new("/data/opt/mini-assistant/.env");
                if fallback.exists() {
                    let _ = dotenvy::from_path(fallback);
                }
            }

            // 2. Initialize tracing logger (unbuffered stderr for instant logging)
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::from_default_env()
                        .add_directive("mini_assistant=debug".parse()?),
                )
                .init();

            info!("=== mini-assistant v0.1.0 starting on GPT Mini ===");
            if dry_run {
                info!("Dry-run mode active: hardware audio capture/playback disabled");
            }

            // 3. Load config
            let config = config::Config::load_from(config_path.as_deref())
                .context("Failed to load configuration")?;
            info!("Assistant Name: {}", config.assistant.name);
            info!("Audio Playback: {}", config.audio.playback_device);

            let alarm_cfg = config.alarms.clone().unwrap_or_default();
            let alarms_enabled = alarm_cfg.enabled && !conversation_only;
            let alarm_tz = alarm_cfg.timezone.clone();
            let authorization = Arc::new(interaction::TurnAuthorization::new(Duration::from_secs(
                120,
            )));
            let alarm_clock = Arc::new(alarms::SystemClock);
            let alarm_manager = if alarms_enabled {
                anyhow::ensure!(
                    alarms::types::try_parse_timezone_offset(&alarm_tz).is_some(),
                    "Unsupported alarm timezone"
                );
                let dir = if dry_run {
                    std::env::temp_dir().join(format!("mini-alarms-dryrun-{}", std::process::id()))
                } else {
                    alarm_cfg.data_dir.clone()
                };
                let store = Arc::new(alarms::AlarmStore::new(dir)?);
                Some(Arc::new(alarms::AlarmManager::new(
                    alarm_clock.clone(),
                    store,
                )?))
            } else {
                None
            };
            anyhow::ensure!(
                !alarms_only || alarm_manager.is_some(),
                "--alarms-only requires [alarms] enabled = true"
            );
            let local_playback = alarm_manager
                .as_ref()
                .map(|_| start_audio_output(&config.audio, dry_run))
                .transpose()?;
            let alarm_runner = alarm_manager
                .as_ref()
                .zip(local_playback.as_ref())
                .map(|(manager, playback)| {
                    AlarmRunner::start(
                        manager.clone(),
                        playback.clone(),
                        authorization.clone(),
                        &alarm_cfg,
                    )
                })
                .transpose()?;
            if alarms_only {
                return wait_for_alarm_shutdown().await;
            }

            let voice = config.voice_settings()?;
            let provider = voice.provider;
            let api_key = match voice.api_key {
                Some(key) => key,
                None => {
                    if alarm_runner.is_some() {
                        warn!("No provider key configured; continuing with local alarms");
                        return wait_for_alarm_shutdown().await;
                    }
                    warn!("{} is not configured", provider.key_env());
                    info!("Waiting for signal (Ctrl-C to stop)...");
                    tokio::signal::ctrl_c().await?;
                    return Ok(());
                }
            };
            info!(
                "Voice provider: {} (voice: {})",
                provider.name(),
                voice.voice
            );
            let tools_allowed = !conversation_only && provider == VoiceProvider::Gemini;
            let alarm_tools_enabled = alarms_enabled && tools_allowed;

            // 4. Initialize Home Assistant client if configured
            let (ha_client, ha_tools_enabled, device_summary) =
                match config.ha.as_ref().filter(|_| tools_allowed) {
                    Some(ha_cfg)
                        if ha_cfg.token.is_some() && !ha_cfg.allowed_entities.is_empty() =>
                    {
                        let token = ha_cfg.token.as_ref().unwrap();
                        match ha::HomeAssistantClient::new(
                            ha_cfg.url.clone(),
                            token,
                            &ha_cfg.allowed_entities,
                        ) {
                            Ok(client) => {
                                info!("Connecting to Home Assistant at {}...", ha_cfg.url);
                                let summary = match client.get_device_summary().await {
                                    Ok(s) => {
                                        info!("Loaded explicitly allowed Home Assistant roster");
                                        s
                                    }
                                    Err(e) => {
                                        warn!("Failed to fetch devices from Home Assistant: {}", e);
                                        "Устройства умного дома временно недоступны.".to_string()
                                    }
                                };
                                (Some(Arc::new(client)), true, summary)
                            }
                            Err(e) => {
                                warn!("Invalid Home Assistant configuration: {}", e);
                                (None, false, String::new())
                            }
                        }
                    }
                    _ => {
                        info!(
                        "HA tools disabled: URL, token and explicit allowed_entities are required."
                    );
                        (None, false, String::new())
                    }
                };

            // 4b. Initialize Weather service if enabled
            let weather_tools_enabled =
                tools_allowed && config.weather.as_ref().is_none_or(|w| w.enabled);

            let weather_service = if weather_tools_enabled {
                info!("Weather service initialized (provider: Open-Meteo, locations: Batumi, Tbilisi, Tsikhisdziri)");
                Some(Arc::new(weather::WeatherService::default()))
            } else {
                info!("Weather service disabled in configuration");
                None
            };

            // 4c. Initialize Spotify client and local Web Server
            let spotify_cfg = config.spotify.clone().unwrap_or_default();
            let spotify_client_holder =
                Arc::new(tokio::sync::RwLock::new(None::<spotify::SpotifyApiClient>));

            let spotify_client_id = spotify_cfg
                .client_id
                .clone()
                .or_else(|| std::env::var("SPOTIFY_CLIENT_ID").ok())
                .unwrap_or_default();

            let spotify_refresh_token = spotify_cfg
                .refresh_token
                .clone()
                .or_else(|| std::env::var("SPOTIFY_REFRESH_TOKEN").ok())
                .or_else(|| spotify::load_persisted_token(&spotify_cfg.token_file));

            if let Some(rt) = spotify_refresh_token {
                if !spotify_client_id.trim().is_empty() {
                    info!("Initializing Spotify API client from configured/persisted credentials");
                    let client = spotify::SpotifyApiClient::new(spotify_client_id.clone(), rt);
                    *spotify_client_holder.write().await = Some(client);
                }
            }

            let spotify_tools_enabled = tools_allowed && spotify_cfg.enabled;

            // Start local web management server (e.g. port 8080)
            if spotify_cfg.enabled {
                let srv_client_id = spotify_client_id.clone();
                let srv_redirect_uri = format!(
                    "http://192.168.1.110:{}/spotify/callback",
                    spotify_cfg.web_port
                );
                let web_server = Arc::new(
                    spotify::SpotifyWebServer::new(
                        srv_client_id,
                        srv_redirect_uri,
                        spotify_client_holder.clone(),
                    )
                    .with_token_path(spotify_cfg.token_file.clone()),
                );
                let web_port = spotify_cfg.web_port;
                tokio::spawn(async move {
                    let bind_addr = std::net::SocketAddr::from(([0, 0, 0, 0], web_port));
                    match tokio::net::TcpListener::bind(bind_addr).await {
                        Ok(listener) => {
                            let (_tx, rx) = tokio::sync::watch::channel(false);
                            if let Err(e) = web_server.run(listener, rx).await {
                                warn!(error = %e, "Spotify web server stopped with error");
                            }
                        }
                        Err(e) => {
                            warn!(port = web_port, error = %e, "Could not bind Spotify web server");
                        }
                    }
                });
            }

            // 4d. Initialize Spotify Soloist Controller
            let soloist_client = Arc::new(spotify::SoloistClient::new(
                spotify_cfg.soloist_ctl.clone(),
                spotify_cfg.ws_url.clone(),
            ));
            let voice_active = Arc::new(std::sync::atomic::AtomicBool::new(false));

            // 4e. Initialize Wi-Fi Setup and Mute Button Watcher
            let wifi_cfg = config.wifi.clone().unwrap_or_default();
            if wifi_cfg.enabled {
                let wifi_coordinator =
                    Arc::new(wifi::coordinator::WifiSetupCoordinator::new(Arc::new(
                        wifi::wpa::WpaSupplicantBackend::new(wifi_cfg.interface.clone()),
                    )));
                let coord = wifi_coordinator.clone();
                let dns_bind = std::net::SocketAddr::from(([0, 0, 0, 0], wifi_cfg.dns_port));
                let portal_bind = std::net::SocketAddr::from(([0, 0, 0, 0], wifi_cfg.portal_port));

                let (tx_btn, mut rx_btn) = tokio::sync::mpsc::channel(16);
                let button_device = buttons::find_adc_keypad_device().await;

                if button_device.exists() {
                    let dev = button_device.clone();
                    tokio::spawn(async move {
                        if let Err(e) = buttons::run_mute_button_watcher(
                            &dev,
                            alarms::clock::SystemClock,
                            tx_btn,
                        )
                        .await
                        {
                            warn!(path = %dev.display(), error = %e, "Mute button watcher stopped");
                        }
                    });

                    tokio::spawn(async move {
                        while let Some(event) = rx_btn.recv().await {
                            if let Err(e) = coord
                                .handle_button_event_with_addr(event, dns_bind, portal_bind)
                                .await
                            {
                                warn!(error = %e, "Failed to handle mute button event");
                            }
                        }
                    });
                } else {
                    debug!(
                        path = %button_device.display(),
                        "Mute button hardware input device not found (non-Linux or virtual mock environment)"
                    );
                }
            }

            // 5. Build dynamic system instruction with HA device list and weather capabilities
            let mut system_instruction = format!(
        "Ты — персональный голосовой ассистент устройства Мини по имени «{name}» (также откликаешься на «Штучка» или «Штучка-Интеллект»).\n\n\
        ХАРАКТЕР И ГОЛОС:\n\
        Твой характер — предельно спокойный, невозмутимый, расслабленный, лаконичный и точный в фактах. \
        Говори размеренно, неторопливо, без лишней суеты, без восклицаний и наигранного энтузиазма. \
        Никогда не используй markdown-разметку, списки или спецсимволы в голосовых ответах — только чистая естественная речь.\n\n\
        ОТВЕТЫ НА РАЗГОВОРНЫЕ ВОПРОСЫ И ОБРАЩЕНИЯ:\n\
        Когда пользователь обращается к тебе, здоровается («привет», «здравствуй»), произносит твоё имя («Штучка»), спрашивает «как дела», шутит или задаёт вопросы — \
        ОБЯЗАТЕЛЬНО отвечай голосом коротко (1-2 фразы), спокойно и слегка лениво. \
        В ТАКИХ РАЗГОВОРНЫХ РЕПЛИКАХ КАТЕГОРИЧЕСКИ ЗАПРЕЩЕНО ВЫЗЫВАТЬ КАКИЕ-ЛИБО ИНСТРУМЕНТЫ (TOOLS)!\n\n\
        ОСТАНОВКА РЕЧИ И ПРЕРЫВАНИЕ:\n\
        Если пользователь говорит «хватит», «стоп», «замолчи», «тишина», «отмена» или просит помолчать — \
        ОБЯЗАТЕЛЬНО немедленно замолчи! Ответь максимум одним словом: «Молчу» (или не издавай звуков вообще). \
        КАТЕГОРИЧЕСКИ ЗАПРЕЩЕНО в ответ на «стоп» начинать новые длинные монологи или вызывать какие-либо инструменты!\n\n\
        УПРАВЛЕНИЕ УМНЫМ ДОМОМ (Home Assistant):\n\
        Вызывай функцию 'control_home_device' ТОЛЬКО И ИСКЛЮЧИТЕЛЬНО тогда, когда пользователь ЯВНО И ОДНОЗНАЧНО попросил выполнить действие с устройством в своей текущей фразе!\n\
        Даже если в списке устройств указано состояние 'unavailable', при любой явной команде пользователя («включи кондиционер», «выключи кондиционер») ТЫ ОБЯЗАНА ВЫЗВАТЬ 'control_home_device', так как статус устройства проверяется в реальном времени!\n\
        СТРОГИЕ ПРАВИЛА ВЫЗОВА ИНСТРУМЕНТОВ:\n\
        1. Включение: при явных фразах «включи кондиционер», «запусти сплит», «вруби кондей» вызывай: domain='climate', service='turn_on', entity_id='climate.midea_xiao_air_conditioner'.\n\
        2. Выключение: при явных фразах «выключи кондиционер», «отключи кондей», «выруби сплит» вызывай: domain='climate', service='turn_off', entity_id='climate.midea_xiao_air_conditioner'. НИКОГДА не вызывай 'turn_on', если просят выключить!\n\
        3. Температура: при фразах «поставь 22 градуса» вызывай: domain='climate', service='set_temperature', entity_id='climate.midea_xiao_air_conditioner', parameters={{'temperature': 22}}.\n\
        4. Ответ после выполнения: после успешного вызова инструмента подтверди выполнение предельно коротко голосом (1-2 слова): «Включила», «Выключила», «Поставила 22 градуса» или «Готово». Если инструмент вернул 'device_unavailable' или ошибку — обязательно сообщи голосом: «Кондиционер сейчас недоступен».\n\
        5. СТРОЖАЙШИЙ ЗАПРЕТ ЛОЖНЫХ СРАБАТЫВАНИЙ: если пользователь просто позвал («Штучка»), спросил «как дела», если слышен щелчок, шум выключения прибора, вздох или неразборчивый звук — КАТЕГОРИЧЕСКИ ЗАПРЕЩЕНО вызывать 'control_home_device'! Не включай ничего сама по себе!",
        name = config.assistant.name
    );

            system_instruction.push_str("\nПри запуске, в тишине, после окончания ответа и без явного обращения молчи. Не продолжай примеры, не рассказывай погоду и не предлагай действия сама. Не инициируй диалог.\n");
            if ha_tools_enabled {
                system_instruction.push_str("\n\nДоступные устройства в доме:\n");
                system_instruction.push_str(&device_summary);
            }

            if weather_tools_enabled {
                system_instruction.push_str(
                    "\n\nПРОГНОЗ ПОГОДЫ (инструмент 'get_weather'):\n\
                    Инструмент 'get_weather' поддерживает ЛЮБЫЕ города и населенные пункты мира (Батуми, Тбилиси, Москва, Париж, Кутаиси и любые другие).\n\
                    1. Когда пользователь спрашивает о погоде (в любом городе, о температуре, дожде, ветре, влажности) — ОБЯЗАТЕЛЬНО вызови инструмент 'get_weather'.\n\
                    В параметр 'location_id' передавай точное название города, названного пользователем (например 'Москва', 'Кутаиси', 'Батуми', 'Париж'). Если город не назван, используй 'Батуми'.\n\
                    2. ОБЯЗАТЕЛЬНЫЕ ПРАВИЛА ЧИСЛОВОЙ ТОЧНОСТИ В ОТВЕТЕ:\n\
                    - Если пользователь спросил про ветер или силу ветра — ОБЯЗАТЕЛЬНО назови точную скорость ветра в метрах в секунду (м/с) из поля wind_speed_ms (например: «ветер 2 метра в секунду») и направление. КАТЕГОРИЧЕСКИ ЗАПРЕЩЕНО заменять цифры качественными оценками вроде «не сильный», «слабый», «нормальный»!\n\
                    - Если спросили про температуру — называй точные градусы («плюс 17 градусов»).\n\
                    - Если спросили про дождь или влажность — называй точные осадки или проценты.\n\
                    - Отвечай конкретно и по существу на заданный вопрос (не говори «всё хорошо» вместо запрошенных данных).\n\
                    КАТЕГОРИЧЕСКИ ЗАПРЕЩЕНО вызывать функцию 'control_home_device' при вопросах о погоде!\n"
                );
            }

            if alarm_tools_enabled {
                system_instruction.push_str(&format!(
                    "\n\nЛОКАЛЬНЫЕ БУДИЛЬНИКИ (инструменты 'create_alarm', 'list_alarms', 'delete_alarm', 'snooze_alarm', 'stop_alarm'):\n\
                    Все будильники работают локально на устройстве и не зависят от интернета или внешних серверов. Часовой пояс устройства: {tz}.\n\
                    1. Постановка будильника:\n\
                       При фразах «поставь будильник на 7 утра», «разбуди меня в восемь тридцать», «заведи будильник на 7 утра по будням», «поставь будильник на завтра на девять» — ОБЯЗАТЕЛЬНО вызови инструмент 'create_alarm'.\n\
                       - В поле 'time' передавай точное время в 24-часовом формате 'HH:MM' (например '07:00', '08:30', '19:00').\n\
                       - Если пользователь сказал «по будням», передавай days_of_week=['weekdays'].\n\
                       - Если «по выходным» — days_of_week=['weekends'].\n\
                       - Если «каждый день» / «ежедневно» — days_of_week=['daily'].\n\
                       - Для разового будильника с явной датой передавай date='today', date='tomorrow' или точную дату YYYY-MM-DD. День недели в разовой просьбе не превращай в повтор. Уточни точное время или дату при неоднозначности.\n\
                       - Подтверждай установку только после успешного результата инструмента. Озвучь сохранённые время и дату или расписание из результата.\n\
                    2. Список будильников:\n\
                       При вопросах «какие у меня будильники?», «что по будильникам?», «есть ли будильник на завтра?» — ОБЯЗАТЕЛЬНО вызови инструмент 'list_alarms'.\n\
                       - Озвучь голосом список активных будильников (их время и дни). Если будильников нет, скажи: «У вас нет активных будильников».\n\
                    3. Удаление и отмена будильника:\n\
                       При фразах «удали будильник на 7 утра», «отмени будильник» — вызови инструмент 'delete_alarm', указав время, имя или ID. При нескольких совпадениях попроси выбрать запись.\n\
                    4. Отложить или остановить звонок:\n\
                       При фразах «отложи будильник на 5 минут», «еще 5 минут» — вызови 'snooze_alarm'.\n\
                       'stop_alarm' и 'snooze_alarm' действуют только на текущий звонок. Пока звучит сигнал, на этом этапе для остановки доступна локальная CLI.\n\
                    КАТЕГОРИЧЕСКИ ЗАПРЕЩЕНО вызывать 'control_home_device' или 'get_weather' при командах о будильниках!\n",
                    tz = alarm_tz
                ));
            }

            if spotify_tools_enabled {
                system_instruction.push_str(
                    "\n\nУПРАВЛЕНИЕ МУЗЫКОЙ В SPOTIFY (инструменты 'spotify_play', 'spotify_pause', 'spotify_next', 'spotify_previous', 'spotify_volume'):\n\
                    1. При фразах «поставь на паузу», «останови музыку», «стоп музыка», «пауза» — ОБЯЗАТЕЛЬНО вызови инструмент 'spotify_pause'.\n\
                    2. При фразах «продолжи музыку», «включи музыку», «играй» — вызови 'spotify_play'. Если назван конкретный трек, исполнитель или плейлист («включи рок», «включи Queen») — передай этот текст в параметр 'query'.\n\
                    3. При фразах «следующий трек», «дальше», «переключи песню» — вызови 'spotify_next'.\n\
                    4. При фразах «предыдущий трек», «назад» — вызови 'spotify_previous'.\n\
                    5. При фразах «громкость музыки на 50», «сделай потише/погромче» — вызови 'spotify_volume' с числовым значением volume_percent (0-100).\n\
                    После успешного выполнения подтверждай голосом предельно лаконично (1-2 слова): «Пауза», «Включила», «Следующий».\n"
                );
            }

            system_instruction.push_str(
                "\n\nУПРАВЛЕНИЕ ДИАЛОГОМ И ОСТАНОВКОЙ:\n\
                1. При любых фразах завершения диалога или остановки («хватит», «все», «всё», «стоп», «молчи», «достаточно», «замолчи», «отбой», «пока», «до свидания», «хватит болтать») — ТЫ ОБЯЗАНА СРАЗУ ВЫЗВАТЬ ИНСТРУМЕНТ 'stop_dialogue' и ответить предельно коротко (1 слово): «Молчу» (или «Хорошо»).\n\
                2. После вызова 'stop_dialogue' диалог немедленно завершается.\n\
                3. В диалоге будь естественным и лаконичным собеседником. Если пользователь задает уточняющий вопрос — отвечай в контексте предыдущего разговора.\n"
            );

            // 6. Setup Tool Calls handler for Home Assistant, Weather, and Alarms
            if !tools_allowed {
                system_instruction = format!(
                    "Ты — голосовой собеседник «{}». Отвечай кратко и естественно на языке пользователя. \
                    В этом режиме доступны только разговорные ответы. Не заявляй о выполнении внешних действий.",
                    config.assistant.name
                );
            }

            let any_tools_enabled = ha_tools_enabled
                || weather_tools_enabled
                || alarm_tools_enabled
                || spotify_tools_enabled;
            let (tool_call_tx, mut tool_call_rx) = mpsc::channel::<gemini::ToolCallRequest>(20);
            let suppress_followup = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let suppress_followup_for_mic = suppress_followup.clone();
            let suppress_followup_for_tools = suppress_followup.clone();

            if any_tools_enabled {
                let ha_opt = ha_client.clone();
                let weather_opt = weather_service.clone();
                let tool_authorization = authorization.clone();
                let alarm_mgr_opt = alarm_manager.clone();
                let alarm_clock_opt = alarm_clock.clone();
                let alarm_tz_tools = alarm_tz.clone();
                let spotify_client_opt = spotify_client_holder.clone();
                let soloist_for_tools = soloist_client.clone();

                tokio::spawn(async move {
                    while let Some(req) = tool_call_rx.recv().await {
                        if !tool_authorization.is_current(req.turn_id) {
                            continue;
                        }

                        if req.function_name == "stop_dialogue" {
                            info!("stop_dialogue tool executed: dialogue stopped by user command");
                            suppress_followup_for_tools
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                            let _ = req.reply_tx.send(json!({"status": "dialogue_stopped"}));
                            continue;
                        }

                        // Try alarm tools first
                        if let Some(ref mgr) = alarm_mgr_opt {
                            if let Some(alarm_out) = crate::alarms::handle_alarm_tool_call(
                                mgr,
                                alarm_clock_opt.as_ref(),
                                &req,
                                &alarm_tz_tools,
                            ) {
                                let _ = req.reply_tx.send(alarm_out);
                                continue;
                            }
                        }

                        // Try Spotify tools
                        if let Some(sp_out) = crate::spotify::handle_spotify_tool_call(
                            &soloist_for_tools,
                            &spotify_client_opt,
                            &req,
                        )
                        .await
                        {
                            let _ = req.reply_tx.send(sp_out);
                            continue;
                        }

                        let output = match req.function_name.as_str() {
                            "get_weather" => {
                                if let Some(ref weather_svc) = weather_opt {
                                    let location_id = req
                                        .args
                                        .get("location_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("batumi");
                                    let period = req
                                        .args
                                        .get("period")
                                        .and_then(|v| v.as_str())
                                        .and_then(|s| serde_json::from_value(json!(s)).ok())
                                        .unwrap_or_default();
                                    let granularity = req
                                        .args
                                        .get("granularity")
                                        .and_then(|v| v.as_str())
                                        .and_then(|s| serde_json::from_value(json!(s)).ok())
                                        .unwrap_or_default();

                                    let weather_req = weather::GetWeatherRequest {
                                        location_id: location_id.to_string(),
                                        period,
                                        granularity,
                                        metrics: vec![],
                                        provider: None,
                                    };

                                    match weather_svc.get_weather(weather_req).await {
                                        Ok(resp) => {
                                            debug!(
                                                "Weather tool completed (private result omitted)"
                                            );
                                            serde_json::to_value(&resp).unwrap_or_else(|_| {
                                                json!({ "status": "ok", "summary": resp.summary })
                                            })
                                        }
                                        Err(e) => {
                                            warn!("Weather tool failed (private details omitted)");
                                            json!({ "status": "error", "message": e.to_string() })
                                        }
                                    }
                                } else {
                                    json!({
                                        "status": "error",
                                        "message": "Погодный сервис отключен."
                                    })
                                }
                            }
                            "control_home_device" => {
                                if let Some(ref ha) = ha_opt {
                                    let domain = req
                                        .args
                                        .get("domain")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let service = req
                                        .args
                                        .get("service")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let entity_id = req
                                        .args
                                        .get("entity_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let mut params = req
                                        .args
                                        .get("parameters")
                                        .cloned()
                                        .unwrap_or_else(|| json!({}));

                                    if let Some(map) = params.as_object_mut() {
                                        map.insert("entity_id".to_string(), json!(entity_id));

                                        // Check device current state first
                                        let current_state =
                                            ha.get_entity_state(entity_id).await.ok().flatten();
                                        if let Some(ref entity) = current_state {
                                            if entity.state == "unavailable"
                                                || entity.state == "unknown"
                                            {
                                                warn!(
                                                    "Target HA entity {} is in state '{}' (offline/unavailable)",
                                                    entity_id, entity.state
                                                );
                                                json!({
                                                    "status": "device_unavailable",
                                                    "entity_id": entity_id,
                                                    "friendly_name": entity.friendly_name(),
                                                    "state": entity.state,
                                                    "message": format!(
                                                        "Устройство '{}' сейчас недоступно в Home Assistant (offline). Сообщи об этом пользователю голосом.",
                                                        entity.friendly_name()
                                                    )
                                                })
                                            } else {
                                                match ha.call_service(domain, service, params).await
                                                {
                                                    Ok(res) => json!({
                                                        "status": "success",
                                                        "result": res,
                                                        "message": "Команда выполнена. Подтверди голосом предельно кратко (1-2 слова)."
                                                    }),
                                                    Err(e) => {
                                                        json!({ "status": "error", "message": e.to_string() })
                                                    }
                                                }
                                            }
                                        } else {
                                            match ha.call_service(domain, service, params).await {
                                                Ok(res) => json!({
                                                    "status": "success",
                                                    "result": res,
                                                    "message": "Команда выполнена. Подтверди голосом предельно кратко (1-2 слова)."
                                                }),
                                                Err(e) => {
                                                    json!({ "status": "error", "message": e.to_string() })
                                                }
                                            }
                                        }
                                    } else {
                                        json!({ "status": "error", "message": "HA parameters must be an object" })
                                    }
                                } else {
                                    json!({
                                        "status": "error",
                                        "message": "Home Assistant не настроен."
                                    })
                                }
                            }
                            "get_device_history" => {
                                if let Some(ref ha) = ha_opt {
                                    let entity_id = req
                                        .args
                                        .get("entity_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let hours = match req.args.get("hours_ago") {
                                        None => 24,
                                        Some(value) => value
                                            .as_u64()
                                            .and_then(|n| u32::try_from(n).ok())
                                            .unwrap_or(0),
                                    };

                                    match ha.get_entity_history(entity_id, hours).await {
                                        Ok(history) => {
                                            json!({ "status": "success", "history": history })
                                        }
                                        Err(e) => {
                                            json!({ "status": "error", "message": e.to_string() })
                                        }
                                    }
                                } else {
                                    json!({
                                        "status": "error",
                                        "message": "Home Assistant не настроен."
                                    })
                                }
                            }
                            other => {
                                json!({ "status": "unknown_function", "name": other })
                            }
                        };

                        let _ = req.reply_tx.send(output);
                    }
                });
            }

            // 7. Initialize ALSA Playback (24000 Hz, 16-bit mono from Gemini Live)
            let playback = match local_playback {
                Some(playback) => playback,
                None => start_audio_output(&config.audio, dry_run)?,
            };

            let playback_for_interrupt = playback.clone();
            let on_interrupted = Arc::new(move || {
                playback_for_interrupt.interrupt();
            });

            // 8. Channels for audio routing:
            // Raw mic from yio_audio IPC
            let (tx_raw_mic, mut rx_raw_mic) = mpsc::channel::<Vec<u8>>(100);
            // Gated mic PCM (16 kHz) goes to the selected provider after activation.
            let (tx_cloud_mic, rx_cloud_mic) = mpsc::channel::<gemini::UserAudio>(100);
            // Returned PCM (24 kHz) carries the cancellation epoch and response ID.
            let (tx_speaker, mut rx_speaker) = mpsc::channel::<(u64, u64, Vec<u8>)>(100);

            // Local keyword grants a turn; energy only detects speech within it.
            let vad_threshold = std::env::var("MINI_VAD_THRESHOLD")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .filter(|v| v.is_finite() && (1.0..=1000.0).contains(v))
                .unwrap_or(18.0);
            let silence_timeout_ms = std::env::var("MINI_SILENCE_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|v| (200..=10000).contains(v))
                .unwrap_or(2500);
            let initial_silence_timeout_ms = std::env::var("MINI_INITIAL_SILENCE_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|v| (1000..=15000).contains(v))
                .unwrap_or(6500);
            let mic_gain = std::env::var("MINI_MIC_GAIN")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .filter(|v| v.is_finite() && (0.1..=20.0).contains(v))
                .unwrap_or(1.0);
            let model_path = std::env::var_os("MINI_WAKEWORD_MODEL")
                .map(std::path::PathBuf::from)
                .or(config.assistant.wakeword_model.clone())
                .unwrap_or_else(|| "/bkupgrade/data/opt/mini-assistant/keyword.rpw".into());
            let activation: Box<dyn wakeword::WakeWordEngine> =
                match wakeword::keyword::KeywordEngine::load(&model_path) {
                    Ok(engine) => {
                        info!("Local personal keyword detector loaded");
                        Box::new(engine)
                    }
                    Err(_) => {
                        warn!("No valid local keyword enrollment: acoustic activation DISABLED; explicit SIGUSR1 control only");
                        Box::new(wakeword::keyword::DisabledKeyword)
                    }
                };
            let mut gate = wakeword::InputGate::with_activation_and_initial_timeout(
                activation,
                vad_threshold,
                2000,
                initial_silence_timeout_ms,
                silence_timeout_ms,
                16000,
            );
            let manual_arm = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let cancel_input = Arc::new(std::sync::atomic::AtomicBool::new(false));
            #[cfg(unix)]
            {
                let mut activate =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
                let mut cancel =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2())?;
                let arm = manual_arm.clone();
                let reset = cancel_input.clone();
                let auth = authorization.clone();
                let speaker = playback.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = activate.recv() => { arm.store(true, std::sync::atomic::Ordering::SeqCst); }
                            _ = cancel.recv() => {
                                auth.cancel(); speaker.interrupt();
                                arm.store(false, std::sync::atomic::Ordering::SeqCst);
                                reset.store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                        }
                    }
                });
            }
            let (tx_session_status, rx_session_status) = tokio::sync::oneshot::channel();
            let mic_authorization = authorization.clone();
            let playback_for_mic = playback.clone();
            let alarm_for_mic = alarm_runner.as_ref().map(|runner| runner.player.clone());
            let soloist_for_mic = soloist_client.clone();
            let voice_active_for_mic = voice_active.clone();
            tokio::spawn(async move {
                use std::sync::atomic::Ordering;
                use std::time::{Duration, Instant};
                let Ok(session_status): Result<Arc<std::sync::atomic::AtomicU8>, _> =
                    rx_session_status.await
                else {
                    return;
                };
                // Controls issued during setup must not arm a future connection.
                manual_arm.store(false, Ordering::SeqCst);
                let mut active_turn = None;
                let mut arm_until = Instant::now();
                let mut followup_until = Instant::now();
                let followup_timeout_secs = std::env::var("MINI_FOLLOWUP_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .filter(|v| (1..=60).contains(v))
                    .unwrap_or(10);
                let mut echo_until = Instant::now() + Duration::from_secs(1);
                let mut utterance_started = Instant::now();
                let mut last_turn_seen: Option<u64> = None;
                let mut led_is_on = false;
                // Hardware capture has a measured startup DC transient. Discard two
                // seconds by PCM duration, independently of network setup latency.
                let mut warmup_bytes: usize = 16000 * 2 * 2;
                // Opt-in structural diagnostics: no audio, words or tool arguments.
                let diagnostics = std::env::var("MINI_DIAGNOSTICS").is_ok_and(|v| v == "1");
                let mut diagnostic_since = Instant::now();
                let mut diagnostic_chunks = 0u64;
                let mut diagnostic_peak = 0.0_f64;
                while let Some(chunk) = rx_raw_mic.recv().await {
                    let now = Instant::now();
                    if diagnostics {
                        diagnostic_chunks += 1;
                        let samples = chunk.len() / 2;
                        if samples > 0 {
                            let energy = chunk
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .map(|s| f64::from(i16::from_le_bytes(*s)).powi(2))
                                .sum::<f64>();
                            diagnostic_peak = diagnostic_peak.max((energy / samples as f64).sqrt());
                        }
                        if now.duration_since(diagnostic_since) >= Duration::from_secs(5) {
                            debug!(
                                chunks = diagnostic_chunks,
                                peak_rms = diagnostic_peak,
                                session_status = session_status.load(Ordering::SeqCst),
                                playback = playback_for_mic.is_playing(),
                                gate = gate.is_active(),
                                warmup_bytes,
                                "Local microphone health (content omitted)"
                            );
                            diagnostic_since = now;
                            diagnostic_chunks = 0;
                            diagnostic_peak = 0.0;
                        }
                    }
                    let session_state = gemini::live_client::SessionStatus::from(
                        session_status.load(Ordering::SeqCst),
                    );
                    let is_ready = session_state == gemini::live_client::SessionStatus::Ready;
                    if session_state == gemini::live_client::SessionStatus::Stopped {
                        gate.reset_gate();
                        active_turn = None;
                        arm_until = now;
                        followup_until = now;
                        last_turn_seen = None;
                        manual_arm.store(false, Ordering::SeqCst);
                        continue;
                    }
                    if cancel_input.swap(false, Ordering::SeqCst) {
                        arm_until = now;
                        followup_until = now;
                        last_turn_seen = None;
                        gate.set_manual_armed(false);
                        gate.reset_gate();
                        active_turn = None;
                    }
                    if manual_arm.swap(false, Ordering::SeqCst) {
                        mic_authorization.cancel();
                        playback_for_mic.interrupt();
                        let soloist = soloist_for_mic.clone();
                        tokio::spawn(async move {
                            if let Err(e) = soloist.pause().await {
                                debug!("Soloist manual arm pause error: {e}");
                            }
                        });
                        gate.reset_gate();
                        active_turn = None;
                        arm_until = now + Duration::from_secs(8);
                        followup_until = now;
                    }
                    if warmup_bytes > 0 {
                        warmup_bytes = warmup_bytes.saturating_sub(chunk.len());
                        gate.reset_gate();
                        continue;
                    }

                    // Speaker state and follow-up window tracking
                    let is_speaker_playing = playback_for_mic.is_playing()
                        || alarm_for_mic
                            .as_ref()
                            .is_some_and(|alarm| alarm.is_ringing());

                    let current_auth_turn = mic_authorization.current_turn();
                    if let Some(turn_id) = current_auth_turn {
                        last_turn_seen = Some(turn_id);
                    } else if let Some(finished_id) = last_turn_seen.take() {
                        if !is_speaker_playing {
                            if suppress_followup_for_mic.swap(false, Ordering::SeqCst) {
                                info!(
                                    finished_turn = finished_id,
                                    "Dialogue ended explicitly: follow-up suppressed"
                                );
                                followup_until = now;
                                gate.set_manual_armed(false);
                            } else {
                                followup_until = now + Duration::from_secs(followup_timeout_secs);
                                info!(
                                    finished_turn = finished_id,
                                    timeout_secs = followup_timeout_secs,
                                    "Conversational follow-up window opened: listening for continuation without wakeword"
                                );
                            }
                        } else {
                            // Hold finished_id until speaker audio finishes playing
                            last_turn_seen = Some(finished_id);
                        }
                    }

                    if is_speaker_playing {
                        echo_until = now + Duration::from_millis(800);
                    }
                    let speaker_active = is_speaker_playing || now < echo_until;

                    if speaker_active {
                        // During speaker output or echo decay:
                        // Never allow VAD / energy barge-in (prevents self-interruption on echo).
                        // ONLY allow the keyword engine (Rustpotter «Штучка») to detect acoustic barge-in!
                        gate.set_manual_armed(false);
                    } else {
                        // When speaker is silent:
                        // Manual arming enables VAD speech detection (if manual_arm was triggered or in follow-up window)
                        let in_followup = now < followup_until;
                        let is_manually_armed = now < arm_until || in_followup;
                        gate.set_manual_armed(is_manually_armed);
                    }

                    // Unified visual LED state: active during input, thinking, playback, or follow-up window
                    let is_turn_in_progress =
                        current_auth_turn.is_some() || active_turn.is_some() || is_speaker_playing;
                    let should_led_be_on = is_turn_in_progress || now < followup_until;
                    voice_active_for_mic.store(should_led_be_on, Ordering::SeqCst);
                    if should_led_be_on != led_is_on {
                        led_is_on = should_led_be_on;
                        set_led_active(led_is_on);
                    }

                    let was_active = gate.is_active();
                    let frames = gate.process_frame(chunk);
                    let mut now_active = gate.is_active();
                    if !was_active && now_active {
                        mic_authorization.cancel();
                        playback_for_mic.interrupt();
                        let soloist = soloist_for_mic.clone();
                        tokio::spawn(async move {
                            if let Err(e) = soloist.pause().await {
                                debug!("Soloist barge-in pause skipped or error: {e}");
                            }
                        });
                        let id = mic_authorization.begin_explicit_turn();
                        active_turn = Some(id);
                        last_turn_seen = Some(id);
                        let expected_epoch = playback_for_mic.cancellation_epoch();
                        let lease = mic_authorization.clone();
                        let speaker = playback_for_mic.clone();
                        tokio::spawn(async move {
                            while lease.is_current(id) {
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                            speaker.interrupt_if_epoch(expected_epoch);
                        });
                        utterance_started = now;
                        arm_until = now;
                        followup_until = now;
                        info!("Explicit local activation: user turn opened");
                    }
                    if now_active && now.duration_since(utterance_started) > Duration::from_secs(60)
                    {
                        gate.reset_gate();
                        now_active = false;
                    }
                    if let Some(id) = active_turn {
                        if is_ready {
                            for frame in frames {
                                if !mic_authorization.is_current(id) {
                                    break;
                                }
                                if tx_cloud_mic
                                    .send(gemini::UserAudio {
                                        turn_id: id,
                                        pcm: frame,
                                    })
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        if !now_active {
                            if is_ready {
                                let _ = tx_cloud_mic
                                    .send(gemini::UserAudio {
                                        turn_id: id,
                                        pcm: Vec::new(),
                                    })
                                    .await;
                            }
                            active_turn = None;
                        }
                    }
                }
            });

            // Startup DNS wait has an overall deadline; it never grants a turn.
            if !dry_run {
                let ready = wait_until_ready(std::time::Duration::from_secs(30), || async {
                    tokio::net::lookup_host(provider.host())
                        .await
                        .is_ok_and(|mut addresses| addresses.next().is_some())
                })
                .await;
                if !ready {
                    warn!("DNS readiness deadline reached; connection may fail and retry");
                }
            }

            info!("Connecting to {}", provider.name());
            let model = voice.model.to_owned();
            let voice_name = voice.voice.to_owned();
            let cancellation_epoch = playback.cancellation_epoch_handle();
            let connection = match provider {
                VoiceProvider::Gemini => gemini::GeminiLiveClient::connect(
                    gemini::LiveClientConfig {
                        api_key,
                        model,
                        voice_name,
                        system_instruction,
                        has_ha_tools: ha_tools_enabled,
                        has_weather_tools: weather_tools_enabled,
                        has_alarm_tools: alarm_tools_enabled,
                        has_spotify_tools: spotify_tools_enabled,
                        has_stop_dialogue: any_tools_enabled,
                        endpoint: None,
                        cancellation_epoch: Some(cancellation_epoch),
                        authorization: Some(authorization.clone()),
                        suppress_followup: Some(suppress_followup.clone()),
                    },
                    rx_cloud_mic,
                    tx_speaker,
                    any_tools_enabled.then_some(tool_call_tx),
                    on_interrupted,
                )
                .await
                .map(VoiceClient::Gemini),
            };
            let mut voice_client = match connection {
                Ok(client) => client,
                Err(_) if alarm_runner.is_some() => {
                    warn!(
                        "{} startup failed; continuing with local alarms",
                        provider.name()
                    );
                    return wait_for_alarm_shutdown().await;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to start {}", provider.name()))
                }
            };
            let _ = tx_session_status.send(voice_client.status_handle());

            // 12. Start Audio Capture (direct ALSA hw:0,2 8ch PDM)
            let mut alsa_capture: Option<audio::AlsaCapture> = None;

            let capture_result = async {
                if dry_run {
                    info!("Dry-run active: skipping hardware microphone capture");
                } else {
                    info!(
                        "Initializing microphone capture (backend: {})...",
                        config.audio.capture_backend
                    );

                    let alsa_cfg = audio::AlsaCaptureConfig {
                        device: config.audio.capture_device.clone(),
                        channel_mode: audio::ChannelMode::Avg4,
                        bit_shift: 16,
                        gain: mic_gain,
                        custom_arecord_path: None,
                    };
                    match audio::AlsaCapture::start(alsa_cfg, tx_raw_mic) {
                        Ok(c) => {
                            info!(
                                "Started direct ALSA microphone capture from {} (8ch S32_LE 48kHz)",
                                config.audio.capture_device
                            );
                            alsa_capture = Some(c);
                        }
                        Err(e) => {
                            return Err(e).context("ALSA capture unavailable");
                        }
                    }
                }

                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(error) = capture_result {
                voice_client.stop().await;
                if alarm_runner.is_some() {
                    warn!("Voice capture unavailable; continuing with local alarms");
                    return wait_for_alarm_shutdown().await;
                }
                return Err(error);
            }

            if !dry_run {
                turn_off_boot_led();
            }

            // Accepted model packets retain their cancellation epoch through actual drain.
            let playback_sink = playback.clone();
            let output_authorization = authorization.clone();
            tokio::spawn(async move {
                while let Some((epoch, resp_id, pcm)) = rx_speaker.recv().await {
                    let Some(id) = output_authorization.current_turn() else {
                        continue;
                    };
                    if epoch != playback_sink.cancellation_epoch() {
                        continue;
                    }
                    let end = pcm.is_empty();
                    if let Err(e) = playback_sink.play(epoch, resp_id, pcm).await {
                        error!("Playback error: {}", e);
                        output_authorization.cancel();
                        playback_sink.interrupt();
                    }
                    if end {
                        let auth = output_authorization.clone();
                        let speaker = playback_sink.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                            while auth.is_current(id) && speaker.is_playing() {
                                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                            auth.finish(id);
                        });
                    }
                }
            });

            info!(
                "Voice pipeline ready (ha_tools: {}, weather_tools: {}, alarm_tools: {}, spotify_tools: {})",
                ha_tools_enabled, weather_tools_enabled, alarm_tools_enabled, spotify_tools_enabled
            );
            info!("Press Ctrl+C to terminate cleanly.");

            wait_for_shutdown().await?;
            info!("Shutting down mini-assistant...");

            voice_client.stop().await;

            if let Some(mut cap) = alsa_capture {
                cap.stop();
            }

            info!("Bye!");
            Ok(())
        }
    }
}

fn start_audio_output(
    config: &config::AudioConfig,
    dry_run: bool,
) -> Result<Arc<audio::AudioPlayback>> {
    let output = if dry_run {
        audio::AudioPlayback::start_with_backend(
            config.playback_device.clone(),
            24000,
            audio::PlaybackBackend::Mock,
        )
    } else {
        audio::AudioPlayback::start(config.playback_device.clone(), 24000)
    };
    Ok(Arc::new(
        output.context("Failed to initialize audio playback")?,
    ))
}

struct AlarmRunner {
    player: Arc<alarms::AlarmPlayer>,
    task: tokio::task::JoinHandle<()>,
}

impl AlarmRunner {
    fn start(
        manager: Arc<alarms::AlarmManager<alarms::SystemClock>>,
        playback: Arc<audio::AudioPlayback>,
        authorization: Arc<interaction::TurnAuthorization>,
        config: &config::AlarmConfig,
    ) -> Result<Self> {
        manager.try_claim_scheduler()?;
        let player = Arc::new(alarms::AlarmPlayer::new(playback));
        let output = player.clone();
        let max_duration = Duration::from_secs(config.max_duration_secs);
        let ramp_duration = Duration::from_secs(config.volume_ramp_secs);
        let melody = config.melody_path.clone();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut active = None::<(alarms::AlarmId, u64)>;
            let mut storage_unavailable = false;
            loop {
                interval.tick().await;
                let outcome = (|| -> Result<()> {
                    if let Some((id, epoch)) = &active {
                        if manager.try_active_ringing_id()?.as_ref() != Some(id) {
                            if output.stop_if_epoch(*epoch) {
                                info!("Local alarm playback stopped");
                            }
                            active = None;
                        } else if !output.is_ringing() {
                            manager.stop(Some(id))?;
                            active = None;
                        }
                    }
                    for event in manager.try_tick()? {
                        if let alarms::AlarmEvent::Triggered { id, .. } = event {
                            if manager.try_active_ringing_id()?.as_ref() == Some(&id) {
                                authorization.cancel();
                                let epoch =
                                    output.start(melody.clone(), max_duration, ramp_duration);
                                active = Some((id, epoch));
                                info!("Local alarm playback started");
                            }
                        }
                    }
                    Ok(())
                })();
                if outcome.is_err() && !storage_unavailable {
                    warn!("Alarm scheduler could not access saved state");
                }
                storage_unavailable = outcome.is_err();
            }
        });
        Ok(Self { player, task })
    }
}

impl Drop for AlarmRunner {
    fn drop(&mut self) {
        self.task.abort();
        self.player.stop();
    }
}

async fn wait_for_alarm_shutdown() -> Result<()> {
    info!("Local alarm scheduler ready");
    wait_for_shutdown().await
}

async fn wait_for_shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn wait_until_ready<F, Fut>(budget: std::time::Duration, mut probe: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(budget, async {
        loop {
            if probe().await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    })
    .await
    .is_ok()
}

/// Turn off the solid light blue boot LED (SI-EN SN3236 at I2C address 0x3c)
fn turn_off_boot_led() {
    let res = std::process::Command::new("i2cset")
        .args(["-y", "0", "0x3c", "0x00", "0x00"])
        .status();
    match res {
        Ok(status) if status.success() => {
            info!("Hardware boot LED (sn3236 @ 0x3c) turned off successfully");
        }
        Ok(status) => {
            debug!("i2cset exited with non-zero status: {:?}", status);
        }
        Err(e) => {
            debug!("Could not invoke i2cset (normal on non-device host): {}", e);
        }
    }
}

/// Set hardware LED activity indicator (sn3236 @ 0x3c)
fn set_led_active(active: bool) {
    let val = if active { "0x01" } else { "0x00" };
    let _ = std::process::Command::new("i2cset")
        .args(["-y", "0", "0x3c", "0x00", val])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn readiness_has_deadline_even_if_probe_never_returns() {
        let start = std::time::Instant::now();
        assert!(
            !wait_until_ready(std::time::Duration::from_millis(10), std::future::pending).await
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn test_cli_parse_help() {
        assert_eq!(
            parse_cli_args_from(["mini-assistant", "-h"]).unwrap(),
            CliAction::Help
        );
        assert_eq!(
            parse_cli_args_from(["mini-assistant", "--help"]).unwrap(),
            CliAction::Help
        );
    }

    #[test]
    fn test_cli_parse_version() {
        assert_eq!(
            parse_cli_args_from(["mini-assistant", "-v"]).unwrap(),
            CliAction::Version
        );
        assert_eq!(
            parse_cli_args_from(["mini-assistant", "--version"]).unwrap(),
            CliAction::Version
        );
    }

    #[test]
    fn test_cli_parse_check() {
        assert_eq!(
            parse_cli_args_from(["mini-assistant", "--check"]).unwrap(),
            CliAction::Check {
                config_path: None,
                conversation_only: false
            }
        );
        assert_eq!(
            parse_cli_args_from(["mini-assistant", "--check", "-c", "custom.toml"]).unwrap(),
            CliAction::Check {
                config_path: Some(std::path::PathBuf::from("custom.toml")),
                conversation_only: false,
            }
        );
    }

    #[test]
    fn test_cli_parse_run_options() {
        assert_eq!(
            parse_cli_args_from(["mini-assistant"]).unwrap(),
            CliAction::Run {
                config_path: None,
                dry_run: false,
                alarms_only: false,
                conversation_only: false,
            }
        );
        assert_eq!(
            parse_cli_args_from(["mini-assistant", "--dry-run", "-c", "my_config.toml"]).unwrap(),
            CliAction::Run {
                config_path: Some(std::path::PathBuf::from("my_config.toml")),
                dry_run: true,
                alarms_only: false,
                conversation_only: false,
            }
        );
        assert_eq!(
            parse_cli_args_from(["mini-assistant", "--config=other.toml"]).unwrap(),
            CliAction::Run {
                config_path: Some(std::path::PathBuf::from("other.toml")),
                dry_run: false,
                alarms_only: false,
                conversation_only: false,
            }
        );
    }

    #[test]
    fn test_cli_parse_unknown_rejected() {
        assert!(parse_cli_args_from(["mini-assistant", "--unknown-flag"]).is_err());
        assert!(parse_cli_args_from(["mini-assistant", "-c"]).is_err());
    }

    #[test]
    fn local_alarms_cli_is_explicit_and_separate_from_preflight() {
        assert_eq!(
            parse_cli_args_from(["mini-assistant", "--alarms-only", "--dry-run"]).unwrap(),
            CliAction::Run {
                config_path: None,
                dry_run: true,
                alarms_only: true,
                conversation_only: false,
            }
        );
        assert!(parse_cli_args_from(["mini-assistant", "--check", "--alarms-only"]).is_err());
    }

    #[test]
    fn conversation_check_is_explicit_mode() {
        assert_eq!(
            parse_cli_args_from(["mini-assistant", "--check", "--conversation-only"]).unwrap(),
            CliAction::Check {
                config_path: None,
                conversation_only: true
            }
        );
    }
}
