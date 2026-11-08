//! Embedded Captive Portal HTTP Server.
//!
//! Serves the responsive onboarding web page and handles OS-specific
//! captive detection probes (Apple, Android, Windows).

use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{debug, warn};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScannedNetwork {
    pub ssid: String,
    pub signal_strength: i32,
    pub security: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ConnectRequest {
    pub ssid: String,
    pub psk: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ConnectResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait WifiBackend: Send + Sync + 'static {
    fn scan_networks(&self) -> BoxFuture<'static, Result<Vec<ScannedNetwork>>>;
    fn connect_network(&self, req: ConnectRequest) -> BoxFuture<'static, Result<()>>;
    fn enable_soft_ap(&self, _ssid: &str) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { Ok(()) })
    }
    fn disable_soft_ap(&self) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { Ok(()) })
    }
}

const PORTAL_HTML: &str = r#"<!DOCTYPE html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no">
<title>Настройка GPT Mini</title>
<style>
:root { --bg: #0d1117; --card: #161b22; --border: #30363d; --text: #c9d1d9; --accent: #7928ca; --accent-hover: #9e47f5; --success: #2ea043; --err: #f85149; }
* { box-sizing: border-box; margin: 0; padding: 0; }
body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; background: var(--bg); color: var(--text); padding: 20px; display: flex; justify-content: center; align-items: center; min-height: 100vh; }
.card { background: var(--card); border: 1px solid var(--border); border-radius: 16px; padding: 24px; width: 100%; max-width: 400px; box-shadow: 0 8px 24px rgba(0,0,0,0.5); }
h1 { font-size: 20px; font-weight: 600; margin-bottom: 8px; color: #fff; text-align: center; }
p.desc { font-size: 14px; color: #8b949e; text-align: center; margin-bottom: 24px; }
label { display: block; font-size: 13px; font-weight: 500; margin-bottom: 6px; color: #8b949e; }
.input-group { margin-bottom: 18px; }
select, input[type="text"], input[type="password"] { width: 100%; padding: 12px 14px; background: #0d1117; border: 1px solid var(--border); border-radius: 8px; color: #fff; font-size: 15px; outline: none; transition: border-color 0.2s; }
select:focus, input:focus { border-color: var(--accent); }
button.btn { width: 100%; padding: 13px; background: var(--accent); color: #fff; font-size: 16px; font-weight: 600; border: none; border-radius: 8px; cursor: pointer; transition: background 0.2s; margin-top: 8px; }
button.btn:hover { background: var(--accent-hover); }
button.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.status-msg { margin-top: 16px; font-size: 14px; text-align: center; display: none; }
.status-msg.error { color: var(--err); display: block; }
.status-msg.success { color: var(--success); display: block; }
</style>
</head>
<body>
<div class="card">
  <h1>Станция Mini</h1>
  <p class="desc">Подключение к домашнему Wi-Fi</p>
  <form id="wifiForm">
    <div class="input-group">
      <label for="ssid">Сеть Wi-Fi</label>
      <select id="ssid" name="ssid" required>
        <option value="" disabled selected>Поиск сетей...</option>
      </select>
    </div>
    <div class="input-group">
      <label for="psk">Пароль</label>
      <input type="password" id="psk" name="psk" placeholder="Введите пароль">
    </div>
    <button type="submit" id="submitBtn" class="btn">Подключить</button>
    <div id="statusMsg" class="status-msg"></div>
  </form>
</div>
<script>
async function loadNetworks() {
  const sel = document.getElementById('ssid');
  try {
    const res = await fetch('/api/scan');
    const nets = await res.json();
    sel.innerHTML = '<option value="" disabled selected>Выберите сеть</option>';
    nets.forEach(n => {
      const opt = document.createElement('option');
      opt.value = n.ssid;
      opt.textContent = n.ssid + (n.security ? ' (' + n.security + ')' : '');
      sel.appendChild(opt);
    });
  } catch(e) {
    sel.innerHTML = '<option value="" disabled selected>Ошибка сканирования</option>';
  }
}
document.getElementById('wifiForm').addEventListener('submit', async (e) => {
  e.preventDefault();
  const ssid = document.getElementById('ssid').value;
  const psk = document.getElementById('psk').value;
  const btn = document.getElementById('submitBtn');
  const msg = document.getElementById('statusMsg');
  btn.disabled = true;
  btn.textContent = 'Подключение...';
  msg.className = 'status-msg';
  msg.style.display = 'none';
  try {
    const res = await fetch('/api/connect', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ ssid, psk })
    });
    const data = await res.json();
    if (res.ok) {
      msg.textContent = 'Подключаемся к ' + ssid + '. Это окно закроется автоматически.';
      msg.className = 'status-msg success';
    } else {
      msg.textContent = data.message || 'Ошибка подключения';
      msg.className = 'status-msg error';
      btn.disabled = false;
      btn.textContent = 'Подключить';
    }
  } catch (err) {
    msg.textContent = 'Ошибка связи со станцией';
    msg.className = 'status-msg error';
    btn.disabled = false;
    btn.textContent = 'Подключить';
  }
});
window.addEventListener('DOMContentLoaded', loadNetworks);
</script>
</body>
</html>"#;

pub async fn run_portal_server<B: WifiBackend>(
    listener: TcpListener,
    backend: Arc<B>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        tokio::select! {
            biased;
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    debug!("Captive portal received shutdown signal");
                    break;
                }
            }
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, peer)) => {
                        let backend_clone = backend.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, backend_clone).await {
                                debug!(%peer, error = %e, "Client handling closed");
                            }
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "Portal listener accept error");
                    }
                }
            }
        }
    }
    Ok(())
}

async fn handle_client<B: WifiBackend>(mut stream: TcpStream, backend: Arc<B>) -> Result<()> {
    let mut buf = [0u8; 4096];
    let n = match tokio::time::timeout(std::time::Duration::from_secs(10), stream.read(&mut buf))
        .await
    {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Ok(()), // Client idle timeout, close connection cleanly
    };
    if n == 0 {
        return Ok(());
    }

    let req_str = String::from_utf8_lossy(&buf[..n]);
    let mut lines = req_str.lines();
    let first_line = lines.next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    let (status, content_type, body) = match (method, path) {
        ("GET", "/hotspot-detect.html") | ("GET", "/") => (
            "200 OK",
            "text/html; charset=utf-8",
            PORTAL_HTML.as_bytes().to_vec(),
        ),
        ("GET", "/generate_204")
        | ("GET", "/gen_204")
        | ("GET", "/connecttest.txt")
        | ("GET", "/ncsi.txt") => {
            // Android / Windows probe redirects to captive root
            let redirect_headers = "302 Found\r\nLocation: http://192.168.4.1/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream
                .write_all(format!("HTTP/1.1 {redirect_headers}").as_bytes())
                .await?;
            return Ok(());
        }
        ("GET", "/api/scan") => match backend.scan_networks().await {
            Ok(networks) => {
                let json = serde_json::to_vec(&networks)?;
                ("200 OK", "application/json", json)
            }
            Err(e) => {
                let err_json = serde_json::to_vec(&serde_json::json!({ "error": e.to_string() }))?;
                ("500 Internal Server Error", "application/json", err_json)
            }
        },
        ("POST", "/api/connect") => {
            // Locate body after \r\n\r\n
            if let Some(pos) = req_str.find("\r\n\r\n") {
                let body_str = &req_str[pos + 4..];
                match serde_json::from_str::<ConnectRequest>(body_str) {
                    Ok(req) => {
                        if req.ssid.trim().is_empty() {
                            let resp = serde_json::to_vec(&ConnectResponse {
                                status: "error".into(),
                                message: Some("SSID cannot be empty".into()),
                            })?;
                            ("400 Bad Request", "application/json", resp)
                        } else {
                            match backend.connect_network(req).await {
                                Ok(()) => {
                                    let resp = serde_json::to_vec(&ConnectResponse {
                                        status: "connecting".into(),
                                        message: None,
                                    })?;
                                    ("200 OK", "application/json", resp)
                                }
                                Err(e) => {
                                    let resp = serde_json::to_vec(&ConnectResponse {
                                        status: "error".into(),
                                        message: Some(e.to_string()),
                                    })?;
                                    ("500 Internal Server Error", "application/json", resp)
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let resp = serde_json::to_vec(&ConnectResponse {
                            status: "error".into(),
                            message: Some(format!("Invalid JSON payload: {e}")),
                        })?;
                        ("400 Bad Request", "application/json", resp)
                    }
                }
            } else {
                ("400 Bad Request", "text/plain", b"Missing body".to_vec())
            }
        }
        ("GET", _) => {
            // Default fallback for any other probe domain: serve captive portal HTML
            (
                "200 OK",
                "text/html; charset=utf-8",
                PORTAL_HTML.as_bytes().to_vec(),
            )
        }
        _ => (
            "405 Method Not Allowed",
            "text/plain",
            b"Method Not Allowed".to_vec(),
        ),
    };

    let response_headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );

    stream.write_all(response_headers.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockWifiBackend {
        scan_results: Vec<ScannedNetwork>,
        connect_invocations: AtomicUsize,
        should_fail_connect: bool,
    }

    impl WifiBackend for MockWifiBackend {
        fn scan_networks(&self) -> BoxFuture<'static, Result<Vec<ScannedNetwork>>> {
            let res = self.scan_results.clone();
            Box::pin(async move { Ok(res) })
        }

        fn connect_network(&self, req: ConnectRequest) -> BoxFuture<'static, Result<()>> {
            self.connect_invocations.fetch_add(1, Ordering::SeqCst);
            let fail = self.should_fail_connect;
            Box::pin(async move {
                if fail {
                    anyhow::bail!("Simulated Wi-Fi auth failure");
                }
                if req.psk == "badpass" {
                    anyhow::bail!("Password rejected");
                }
                Ok(())
            })
        }
    }

    async fn send_http_request(addr: SocketAddr, req: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.expect("Connect to portal");
        stream
            .write_all(req.as_bytes())
            .await
            .expect("Write request");
        let mut resp = String::new();
        stream
            .read_to_string(&mut resp)
            .await
            .expect("Read response");
        resp
    }

    #[tokio::test]
    async fn test_apple_captive_probe_returns_portal_html() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let backend = Arc::new(MockWifiBackend {
            scan_results: vec![],
            connect_invocations: AtomicUsize::new(0),
            should_fail_connect: false,
        });

        let server = tokio::spawn(run_portal_server(listener, backend, shutdown_rx));

        let resp = send_http_request(
            addr,
            "GET /hotspot-detect.html HTTP/1.1\r\nHost: captive.apple.com\r\n\r\n",
        )
        .await;

        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.contains("Content-Type: text/html; charset=utf-8"));
        assert!(resp.contains("Станция Mini"));

        let _ = shutdown_tx.send(true);
        let _ = server.await;
    }

    #[tokio::test]
    async fn test_android_probe_returns_302_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let backend = Arc::new(MockWifiBackend {
            scan_results: vec![],
            connect_invocations: AtomicUsize::new(0),
            should_fail_connect: false,
        });

        let server = tokio::spawn(run_portal_server(listener, backend, shutdown_rx));

        let resp = send_http_request(
            addr,
            "GET /generate_204 HTTP/1.1\r\nHost: connectivitycheck.gstatic.com\r\n\r\n",
        )
        .await;

        assert!(resp.starts_with("HTTP/1.1 302 Found"));
        assert!(resp.contains("Location: http://192.168.4.1/"));

        let _ = shutdown_tx.send(true);
        let _ = server.await;
    }

    #[tokio::test]
    async fn test_api_scan_and_connect_flow() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let backend = Arc::new(MockWifiBackend {
            scan_results: vec![ScannedNetwork {
                ssid: "Home-WiFi-5G".into(),
                signal_strength: -55,
                security: "WPA2".into(),
            }],
            connect_invocations: AtomicUsize::new(0),
            should_fail_connect: false,
        });

        let server = tokio::spawn(run_portal_server(listener, backend.clone(), shutdown_rx));

        // 1. Scan
        let scan_resp =
            send_http_request(addr, "GET /api/scan HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n").await;
        assert!(scan_resp.starts_with("HTTP/1.1 200 OK"));
        assert!(scan_resp.contains("Home-WiFi-5G"));

        // 2. Connect with empty SSID rejected
        let bad_connect = send_http_request(
            addr,
            "POST /api/connect HTTP/1.1\r\nHost: 192.168.4.1\r\nContent-Length: 26\r\n\r\n{\"ssid\":\"\",\"psk\":\"pass123\"}",
        )
        .await;
        assert!(bad_connect.starts_with("HTTP/1.1 400 Bad Request"));

        // 3. Connect valid
        let good_connect = send_http_request(
            addr,
            "POST /api/connect HTTP/1.1\r\nHost: 192.168.4.1\r\nContent-Length: 42\r\n\r\n{\"ssid\":\"Home-WiFi-5G\",\"psk\":\"pass123\"}",
        )
        .await;
        assert!(good_connect.starts_with("HTTP/1.1 200 OK"));
        assert!(good_connect.contains("\"status\":\"connecting\""));
        assert_eq!(backend.connect_invocations.load(Ordering::SeqCst), 1);

        let _ = shutdown_tx.send(true);
        let _ = server.await;
    }
}
