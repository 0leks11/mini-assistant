//! Wi-Fi Setup Coordinator.
//!
//! Bridges the physical button event (Mute long-press >= 5s) to the Captive Portal
//! and DNS responder lifecycle, managing transitions between Client mode and SoftAP mode.

use super::dns::run_dns_server;
use super::portal::{run_portal_server, BoxFuture, ConnectRequest, ScannedNetwork, WifiBackend};
use crate::buttons::MuteButtonEvent;
use anyhow::{bail, Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{watch, RwLock};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupState {
    Idle,
    SetupActive,
    Connecting,
    Connected,
    Failed,
}

pub struct WifiSetupCoordinator<B: WifiBackend> {
    backend: Arc<B>,
    state: Arc<RwLock<SetupState>>,
    shutdown_tx: Arc<RwLock<Option<watch::Sender<bool>>>>,
}

impl<B: WifiBackend> WifiSetupCoordinator<B> {
    pub fn new(backend: Arc<B>) -> Self {
        Self {
            backend,
            state: Arc::new(RwLock::new(SetupState::Idle)),
            shutdown_tx: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn current_state(&self) -> SetupState {
        *self.state.read().await
    }

    /// Handles a button event. LongPress initiates setup mode.
    pub async fn handle_button_event(
        &self,
        event: MuteButtonEvent,
        dns_bind: SocketAddr,
        portal_listener: TcpListener,
    ) -> Result<bool> {
        match event {
            MuteButtonEvent::LongPress { .. } => {
                info!("Long press detected on Mute button: entering Wi-Fi setup mode");
                self.start_setup_mode(dns_bind, portal_listener).await?;
                Ok(true)
            }
            MuteButtonEvent::ShortPress { .. } => {
                // Short click: normal mute/unmute toggle, ignore for Wi-Fi setup
                Ok(false)
            }
        }
    }

    /// Handles a button event by binding the portal TCP listener dynamically when needed.
    pub async fn handle_button_event_with_addr(
        &self,
        event: MuteButtonEvent,
        dns_bind: SocketAddr,
        portal_bind: SocketAddr,
    ) -> Result<bool> {
        match event {
            MuteButtonEvent::LongPress { .. } => {
                info!("Long press detected on Mute button: entering Wi-Fi setup mode");
                let portal_listener = TcpListener::bind(portal_bind)
                    .await
                    .with_context(|| format!("Failed to bind captive portal on {portal_bind}"))?;
                self.start_setup_mode(dns_bind, portal_listener).await?;
                Ok(true)
            }
            MuteButtonEvent::ShortPress { .. } => Ok(false),
        }
    }

    /// Starts the Captive DNS and Portal HTTP servers.
    pub async fn start_setup_mode(
        &self,
        dns_bind: SocketAddr,
        portal_listener: TcpListener,
    ) -> Result<()> {
        let mut state = self.state.write().await;
        if *state == SetupState::SetupActive {
            return Ok(()); // Already running
        }

        let (tx, rx) = watch::channel(false);
        *self.shutdown_tx.write().await = Some(tx);
        *state = SetupState::SetupActive;

        let portal_rx = rx.clone();
        let dns_rx = rx.clone();
        let dhcp_rx = rx;

        // Wrap backend with state tracker
        let tracked_backend = Arc::new(CoordinatorBackend {
            inner: self.backend.clone(),
            state: self.state.clone(),
        });

        // 1. Enable SoftAP on backend
        if let Err(e) = self.backend.enable_soft_ap("Station-Mini-Setup").await {
            warn!(error = %e, "Failed to enable SoftAP on Wi-Fi backend");
        }

        // 2. Spawn Portal Server
        tokio::spawn(async move {
            if let Err(e) = run_portal_server(portal_listener, tracked_backend, portal_rx).await {
                warn!(error = %e, "Captive portal server exited with error");
            }
        });

        // 3. Spawn DNS Server
        let target_ip = [192, 168, 4, 1];
        tokio::spawn(async move {
            if let Err(e) = run_dns_server(dns_bind, target_ip, dns_rx).await {
                warn!(error = %e, "Captive DNS server exited with error");
            }
        });

        // 4. Spawn DHCP Server
        let dhcp_bind = SocketAddr::from(([0, 0, 0, 0], super::dhcp::DHCP_SERVER_PORT));
        let assigned_ip = [192, 168, 4, 2];
        tokio::spawn(async move {
            if let Err(e) =
                super::dhcp::run_dhcp_server(dhcp_bind, target_ip, assigned_ip, dhcp_rx).await
            {
                debug!(
                    error = %e,
                    "Captive DHCP server exited or could not bind privileged port"
                );
            }
        });

        info!(portal = %dns_bind, "Wi-Fi SoftAP Captive Portal started successfully");
        Ok(())
    }

    /// Stops the setup servers and returns to Idle state.
    pub async fn stop_setup_mode(&self) -> Result<()> {
        let mut tx_guard = self.shutdown_tx.write().await;
        if let Some(tx) = tx_guard.take() {
            let _ = tx.send(true);
        }
        let _ = self.backend.disable_soft_ap().await;
        let mut state = self.state.write().await;
        *state = SetupState::Idle;
        info!("Wi-Fi setup mode stopped");
        Ok(())
    }
}

struct CoordinatorBackend<B: WifiBackend> {
    inner: Arc<B>,
    state: Arc<RwLock<SetupState>>,
}

impl<B: WifiBackend> WifiBackend for CoordinatorBackend<B> {
    fn scan_networks(&self) -> BoxFuture<'static, Result<Vec<ScannedNetwork>>> {
        self.inner.scan_networks()
    }

    fn connect_network(&self, req: ConnectRequest) -> BoxFuture<'static, Result<()>> {
        let inner = self.inner.clone();
        let state = self.state.clone();
        Box::pin(async move {
            {
                let mut s = state.write().await;
                *s = SetupState::Connecting;
            }
            match inner.connect_network(req).await {
                Ok(()) => {
                    let mut s = state.write().await;
                    *s = SetupState::Connected;
                    let _ = inner.disable_soft_ap().await;
                    Ok(())
                }
                Err(e) => {
                    let mut s = state.write().await;
                    *s = SetupState::Failed;
                    bail!(e)
                }
            }
        })
    }

    fn enable_soft_ap(&self, ssid: &str) -> BoxFuture<'static, Result<()>> {
        self.inner.enable_soft_ap(ssid)
    }

    fn disable_soft_ap(&self) -> BoxFuture<'static, Result<()>> {
        self.inner.disable_soft_ap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockBackend {
        connected: AtomicUsize,
        should_fail: bool,
    }

    impl WifiBackend for MockBackend {
        fn scan_networks(&self) -> BoxFuture<'static, Result<Vec<ScannedNetwork>>> {
            Box::pin(async move { Ok(vec![]) })
        }

        fn connect_network(&self, _req: ConnectRequest) -> BoxFuture<'static, Result<()>> {
            self.connected.fetch_add(1, Ordering::SeqCst);
            let fail = self.should_fail;
            Box::pin(async move {
                if fail {
                    bail!("Connection timeout");
                }
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn test_button_event_routing_and_state_transitions() {
        let backend = Arc::new(MockBackend {
            connected: AtomicUsize::new(0),
            should_fail: false,
        });
        let coordinator = WifiSetupCoordinator::new(backend);

        assert_eq!(coordinator.current_state().await, SetupState::Idle);

        // 1. Short click -> returns false, stays Idle
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dns_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let handled = coordinator
            .handle_button_event(
                MuteButtonEvent::ShortPress {
                    duration: std::time::Duration::from_millis(200),
                },
                dns_addr,
                listener,
            )
            .await
            .unwrap();
        assert!(!handled);
        assert_eq!(coordinator.current_state().await, SetupState::Idle);

        // 2. LongPress -> returns true, transitions to SetupActive
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let handled = coordinator
            .handle_button_event(
                MuteButtonEvent::LongPress {
                    duration: std::time::Duration::from_secs(5),
                },
                dns_addr,
                listener,
            )
            .await
            .unwrap();
        assert!(handled);
        assert_eq!(coordinator.current_state().await, SetupState::SetupActive);

        // 3. Stop -> transitions back to Idle
        coordinator.stop_setup_mode().await.unwrap();
        assert_eq!(coordinator.current_state().await, SetupState::Idle);
    }
}
