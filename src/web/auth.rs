//! Access control, calibration identities, and session leases for the acoustic API.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    AcousticsRead,
    AcousticsWrite,
    Profiles,
    Samples,
    Jobs,
}

impl Scope {
    pub fn all() -> HashSet<Self> {
        [
            Scope::AcousticsRead,
            Scope::AcousticsWrite,
            Scope::Profiles,
            Scope::Samples,
            Scope::Jobs,
        ]
        .into_iter()
        .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationLease {
    pub lease_id: String,
    pub baseline_revision: u64,
    pub max_duration_secs: u64,
    pub max_volume_pct: u8,
    pub max_runs: u32,
    pub runs_count: u32,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct CalibrationToken {
    pub token: String,
    pub identity_id: String,
    pub created_at: Instant,
    pub expires_at: Instant,
    pub scopes: HashSet<Scope>,
    pub revoked: bool,
    pub lease: CalibrationLease,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RequestIdentity {
    Owner,
    Calibration {
        identity_id: String,
        lease_id: String,
        scopes: HashSet<Scope>,
        token: String,
    },
    Anonymous,
}

impl RequestIdentity {
    pub fn is_owner(&self) -> bool {
        matches!(self, RequestIdentity::Owner)
    }

    pub fn has_scope(&self, scope: Scope) -> bool {
        match self {
            RequestIdentity::Owner => true,
            RequestIdentity::Calibration { scopes, .. } => scopes.contains(&scope),
            RequestIdentity::Anonymous => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AuthError {
    MissingToken,
    InvalidToken,
    ExpiredToken,
    RevokedToken,
    LeaseRevoked,
}

impl AuthError {
    pub fn status_code(&self) -> &'static str {
        match self {
            AuthError::MissingToken
            | AuthError::InvalidToken
            | AuthError::ExpiredToken
            | AuthError::RevokedToken => "401 Unauthorized",
            AuthError::LeaseRevoked => "403 Forbidden",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            AuthError::MissingToken => {
                "Authorization header present but bearer token is missing or empty"
            }
            AuthError::InvalidToken => "Invalid authorization token",
            AuthError::ExpiredToken => "Authorization token has expired",
            AuthError::RevokedToken => "Authorization token has been revoked",
            AuthError::LeaseRevoked => "Calibration lease has been revoked or expired",
        }
    }
}

pub struct AuthManager {
    owner_token: Option<String>,
    tokens: Mutex<HashMap<String, CalibrationToken>>,
}

impl Default for AuthManager {
    fn default() -> Self {
        Self::new(std::env::var("MINI_OWNER_TOKEN").ok())
    }
}

impl AuthManager {
    pub fn new(owner_token: Option<String>) -> Self {
        Self {
            owner_token,
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Issue a new calibration identity with bounded duration, scopes, and lease.
    pub fn issue_calibration_token(
        &self,
        identity_id: String,
        ttl: Duration,
        scopes: HashSet<Scope>,
        lease: CalibrationLease,
    ) -> (String, CalibrationToken) {
        let token_secret = format!("calib_{}", uuid::Uuid::new_v4());
        let now = Instant::now();
        let cal_token = CalibrationToken {
            token: token_secret.clone(),
            identity_id,
            created_at: now,
            expires_at: now + ttl,
            scopes,
            revoked: false,
            lease,
        };
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token_secret.clone(), cal_token.clone());
        (token_secret, cal_token)
    }

    pub fn authenticate(
        &self,
        auth_header: Option<&str>,
        is_same_origin: bool,
    ) -> Result<RequestIdentity, AuthError> {
        if let Some(header) = auth_header {
            let trimmed = header.trim();
            let token_str = if let Some(stripped) = trimmed.strip_prefix("Bearer ") {
                stripped.trim()
            } else if let Some(stripped) = trimmed.strip_prefix("bearer ") {
                stripped.trim()
            } else if trimmed.eq_ignore_ascii_case("bearer") {
                ""
            } else {
                trimmed
            };

            if token_str.is_empty() {
                return Err(AuthError::MissingToken);
            }

            if let Some(ref owner) = self.owner_token {
                if token_str == owner {
                    return Ok(RequestIdentity::Owner);
                }
            }

            let mut tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cal) = tokens.get_mut(token_str) {
                if cal.revoked {
                    return Err(AuthError::RevokedToken);
                }
                if !cal.lease.active {
                    return Err(AuthError::LeaseRevoked);
                }
                if Instant::now() >= cal.expires_at {
                    return Err(AuthError::ExpiredToken);
                }
                return Ok(RequestIdentity::Calibration {
                    identity_id: cal.identity_id.clone(),
                    lease_id: cal.lease.lease_id.clone(),
                    scopes: cal.scopes.clone(),
                    token: token_str.to_string(),
                });
            }

            return Err(AuthError::InvalidToken);
        }

        // No Authorization header provided:
        // Same-origin browser UI without token is treated as Owner.
        // External clients without Authorization header are Anonymous.
        if is_same_origin {
            Ok(RequestIdentity::Owner)
        } else {
            Ok(RequestIdentity::Anonymous)
        }
    }

    /// Check if a request with given identity is allowed to access the specified endpoint.
    pub fn authorize_route(
        &self,
        identity: &RequestIdentity,
        method: &str,
        path: &str,
    ) -> Result<(), (&'static str, &'static str)> {
        // Owner has full access to all routes
        if identity.is_owner() {
            return Ok(());
        }

        // Administrative and device control endpoints: strictly Owner only!
        let is_admin_or_system = matches!(
            path,
            "/api/v1/system/restart"
                | "/api/v1/system/reboot"
                | "/api/v1/voice"
                | "/api/v1/volume"
                | "/api/v1/led"
                | "/api/v1/acoustics/tokens"
                | "/api/v1/acoustics/tokens/revoke"
                | "/api/v1/acoustics/lease/revoke"
                | "/api/v1/calibration/tokens"
                | "/api/v1/calibration/tokens/revoke"
                | "/api/v1/calibration/lease/revoke"
        ) || path.starts_with("/api/spotify")
            || path.starts_with("/spotify")
            || path.starts_with("/api/v1/wakeword");

        if is_admin_or_system {
            if matches!(identity, RequestIdentity::Calibration { .. }) {
                return Err((
                    "403 Forbidden",
                    "Forbidden: calibration identity cannot execute administrative operations",
                ));
            } else {
                return Err((
                    "401 Unauthorized",
                    "Unauthorized: administrative operations require owner authentication",
                ));
            }
        }

        // Acoustic read endpoints
        let is_acoustic_read = (method == "GET")
            && matches!(
                path,
                "/api/v1/acoustics"
                    | "/api/v1/calibration"
                    | "/api/v1/acoustics/events"
                    | "/api/v1/calibration/events"
            );

        if is_acoustic_read {
            if identity.has_scope(Scope::AcousticsRead) {
                return Ok(());
            }
            if matches!(identity, RequestIdentity::Anonymous) {
                // Public aggregate status snapshot is allowed anonymously on GET /api/v1/acoustics
                if path == "/api/v1/acoustics" || path == "/api/v1/calibration" {
                    return Ok(());
                }
                return Err((
                    "401 Unauthorized",
                    "Unauthorized: reading acoustic events requires calibration identity",
                ));
            }
            return Err((
                "403 Forbidden",
                "Forbidden: token lacks acoustics_read scope",
            ));
        }

        // Acoustic write endpoints
        let is_acoustic_write = (method == "POST")
            && matches!(
                path,
                "/api/v1/acoustics"
                    | "/api/v1/calibration"
                    | "/api/v1/acoustics/restore"
                    | "/api/v1/calibration/restore"
                    | "/api/v1/acoustics/test_session"
                    | "/api/v1/calibration/test_session"
            );

        if is_acoustic_write {
            if identity.has_scope(Scope::AcousticsWrite) {
                return Ok(());
            }
            if matches!(identity, RequestIdentity::Anonymous) {
                return Err((
                    "401 Unauthorized",
                    "Unauthorized: acoustic mutations require calibration identity",
                ));
            }
            return Err((
                "403 Forbidden",
                "Forbidden: token lacks acoustics_write scope",
            ));
        }

        // Acoustic profiles read endpoints
        let is_profile_read = (method == "GET")
            && (path.starts_with("/api/v1/acoustics/profiles")
                || path.starts_with("/api/v1/calibration/profiles"));

        if is_profile_read {
            if identity.has_scope(Scope::Profiles) || identity.has_scope(Scope::AcousticsRead) {
                return Ok(());
            }
            if matches!(identity, RequestIdentity::Anonymous) {
                return Err((
                    "401 Unauthorized",
                    "Unauthorized: reading profiles requires calibration identity or owner",
                ));
            }
            return Err(("403 Forbidden", "Forbidden: token lacks profiles scope"));
        }

        // Acoustic profiles write / mutation endpoints
        let is_profile_write = (method == "POST" || method == "PUT" || method == "DELETE")
            && (path.starts_with("/api/v1/acoustics/profiles")
                || path.starts_with("/api/v1/calibration/profiles"));

        if is_profile_write {
            if identity.has_scope(Scope::Profiles) {
                return Ok(());
            }
            if matches!(identity, RequestIdentity::Anonymous) {
                return Err((
                    "401 Unauthorized",
                    "Unauthorized: profile mutations require calibration identity with profiles scope",
                ));
            }
            return Err(("403 Forbidden", "Forbidden: token lacks profiles scope"));
        }

        // Candidate preview endpoint
        let is_preview = (method == "POST")
            && matches!(
                path,
                "/api/v1/acoustics/preview" | "/api/v1/calibration/preview"
            );

        if is_preview {
            if identity.has_scope(Scope::AcousticsWrite) || identity.has_scope(Scope::Profiles) {
                return Ok(());
            }
            if matches!(identity, RequestIdentity::Anonymous) {
                return Err((
                    "401 Unauthorized",
                    "Unauthorized: preview requires calibration identity",
                ));
            }
            return Err((
                "403 Forbidden",
                "Forbidden: token lacks acoustics_write or profiles scope",
            ));
        }

        // Static pages and public status
        if method == "GET"
            && matches!(
                path,
                "/" | "/index.html" | "/setup" | "/acoustics" | "/acoustics/" | "/api/v1/status"
            )
        {
            return Ok(());
        }

        // Default: deny unknown mutations or private resources
        if method == "POST" {
            if matches!(identity, RequestIdentity::Anonymous) {
                return Err(("401 Unauthorized", "Unauthorized"));
            }
            return Err(("403 Forbidden", "Forbidden"));
        }

        Ok(())
    }

    /// Record a run for given token's lease; check if max_runs exceeded.
    pub fn record_run(&self, token_str: &str) -> Result<u32, (&'static str, &'static str)> {
        let mut tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cal) = tokens.get_mut(token_str) {
            if !cal.lease.active {
                return Err(("403 Forbidden", "Lease is inactive"));
            }
            if cal.lease.runs_count >= cal.lease.max_runs {
                return Err(("403 Forbidden", "Lease run limit exceeded"));
            }
            cal.lease.runs_count += 1;
            Ok(cal.lease.runs_count)
        } else {
            Err(("401 Unauthorized", "Token not found"))
        }
    }

    pub fn revoke_token(&self, token_str: &str) -> bool {
        let mut tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cal) = tokens.get_mut(token_str) {
            cal.revoked = true;
            true
        } else {
            false
        }
    }

    pub fn revoke_lease(&self, lease_id: &str) -> bool {
        let mut tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        let mut found = false;
        for cal in tokens.values_mut() {
            if cal.lease.lease_id == lease_id {
                cal.lease.active = false;
                found = true;
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_lease(max_runs: u32) -> CalibrationLease {
        CalibrationLease {
            lease_id: uuid::Uuid::new_v4().to_string(),
            baseline_revision: 1,
            max_duration_secs: 300,
            max_volume_pct: 50,
            max_runs,
            runs_count: 0,
            active: true,
        }
    }

    #[test]
    fn test_token_issuance_and_authentication() {
        let auth = AuthManager::new(Some("test-owner-token".to_string()));
        let lease = create_test_lease(10);
        let (token, cal) = auth.issue_calibration_token(
            "agent-calib-1".to_string(),
            Duration::from_secs(3600),
            Scope::all(),
            lease,
        );
        assert!(token.starts_with("calib_"));
        assert_eq!(cal.identity_id, "agent-calib-1");

        // Valid calibration token authentication
        let auth_hdr = format!("Bearer {token}");
        let ident = auth.authenticate(Some(&auth_hdr), false).unwrap();
        assert!(matches!(
            ident,
            RequestIdentity::Calibration {
                ref identity_id,
                ..
            } if identity_id == "agent-calib-1"
        ));

        // Owner token authentication
        let owner_hdr = "Bearer test-owner-token";
        let ident_owner = auth.authenticate(Some(owner_hdr), false).unwrap();
        assert_eq!(ident_owner, RequestIdentity::Owner);

        // Same-origin without token -> Owner
        let same_orig_ident = auth.authenticate(None, true).unwrap();
        assert_eq!(same_orig_ident, RequestIdentity::Owner);

        // External without token -> Anonymous
        let anon_ident = auth.authenticate(None, false).unwrap();
        assert_eq!(anon_ident, RequestIdentity::Anonymous);

        // Invalid token
        assert_eq!(
            auth.authenticate(Some("Bearer invalid-secret"), false)
                .unwrap_err(),
            AuthError::InvalidToken
        );

        // Empty bearer token
        assert_eq!(
            auth.authenticate(Some("Bearer "), false).unwrap_err(),
            AuthError::MissingToken
        );

        // Revoked token
        auth.revoke_token(&token);
        assert_eq!(
            auth.authenticate(Some(&auth_hdr), false).unwrap_err(),
            AuthError::RevokedToken
        );
    }

    #[test]
    fn test_token_expiration() {
        let auth = AuthManager::new(None);
        let lease = create_test_lease(10);
        // Token with 0 TTL expires immediately
        let (token, _) = auth.issue_calibration_token(
            "short-lived".to_string(),
            Duration::from_millis(1),
            Scope::all(),
            lease,
        );
        std::thread::sleep(Duration::from_millis(5));
        let hdr = format!("Bearer {token}");
        assert_eq!(
            auth.authenticate(Some(&hdr), false).unwrap_err(),
            AuthError::ExpiredToken
        );
    }

    #[test]
    fn test_route_authorization_matrix() {
        let auth = AuthManager::new(None);
        let lease = create_test_lease(10);
        let (token, _) = auth.issue_calibration_token(
            "agent-matrix".to_string(),
            Duration::from_secs(3600),
            Scope::all(),
            lease,
        );
        let cal_ident = auth
            .authenticate(Some(&format!("Bearer {token}")), false)
            .unwrap();
        let anon_ident = RequestIdentity::Anonymous;
        let owner_ident = RequestIdentity::Owner;

        // 1. Calibration identity CAN access acoustic routes
        assert!(auth
            .authorize_route(&cal_ident, "GET", "/api/v1/acoustics")
            .is_ok());
        assert!(auth
            .authorize_route(&cal_ident, "GET", "/api/v1/acoustics/events")
            .is_ok());
        assert!(auth
            .authorize_route(&cal_ident, "POST", "/api/v1/acoustics")
            .is_ok());
        assert!(auth
            .authorize_route(&cal_ident, "POST", "/api/v1/acoustics/restore")
            .is_ok());
        assert!(auth
            .authorize_route(&cal_ident, "POST", "/api/v1/acoustics/test_session")
            .is_ok());

        // 2. Calibration identity CANNOT access administrative routes (STRICT 403)
        let admin_routes = [
            "/api/v1/system/restart",
            "/api/v1/system/reboot",
            "/api/v1/voice",
            "/api/v1/volume",
            "/api/v1/led",
            "/api/spotify/play",
            "/api/spotify/pause",
            "/api/v1/wakeword/session/start",
            "/api/v1/wakeword/compile",
            "/api/v1/acoustics/tokens",
        ];
        for route in admin_routes {
            let res = auth.authorize_route(&cal_ident, "POST", route);
            assert!(
                res.is_err(),
                "Route {route} must be forbidden for calibration identity"
            );
            let (status, msg) = res.unwrap_err();
            assert_eq!(status, "403 Forbidden");
            assert!(msg.contains("calibration identity cannot execute administrative operations"));
        }

        // 3. Anonymous caller CANNOT access admin routes or acoustic mutations (STRICT 401)
        assert_eq!(
            auth.authorize_route(&anon_ident, "POST", "/api/v1/system/restart")
                .unwrap_err()
                .0,
            "401 Unauthorized"
        );
        assert_eq!(
            auth.authorize_route(&anon_ident, "POST", "/api/v1/acoustics")
                .unwrap_err()
                .0,
            "401 Unauthorized"
        );
        assert_eq!(
            auth.authorize_route(&anon_ident, "GET", "/api/v1/acoustics/events")
                .unwrap_err()
                .0,
            "401 Unauthorized"
        );

        // 4. Anonymous CAN read public snapshot and static web pages
        assert!(auth
            .authorize_route(&anon_ident, "GET", "/api/v1/acoustics")
            .is_ok());
        assert!(auth
            .authorize_route(&anon_ident, "GET", "/acoustics")
            .is_ok());
        assert!(auth
            .authorize_route(&anon_ident, "GET", "/api/v1/status")
            .is_ok());

        // 5. Owner CAN access all routes
        assert!(auth
            .authorize_route(&owner_ident, "POST", "/api/v1/system/restart")
            .is_ok());
        assert!(auth
            .authorize_route(&owner_ident, "POST", "/api/v1/acoustics/tokens")
            .is_ok());
    }

    #[test]
    fn test_lease_limits_and_revocation() {
        let auth = AuthManager::new(None);
        let lease = create_test_lease(2); // limit: 2 runs
        let lease_id = lease.lease_id.clone();
        let (token, _) = auth.issue_calibration_token(
            "agent-runs".to_string(),
            Duration::from_secs(3600),
            Scope::all(),
            lease,
        );

        // Run 1: Ok
        assert_eq!(auth.record_run(&token).unwrap(), 1);
        // Run 2: Ok
        assert_eq!(auth.record_run(&token).unwrap(), 2);
        // Run 3: Limit exceeded!
        let err = auth.record_run(&token).unwrap_err();
        assert_eq!(err.0, "403 Forbidden");
        assert_eq!(err.1, "Lease run limit exceeded");

        // Revoke lease
        assert!(auth.revoke_lease(&lease_id));
        // Authentication with revoked lease fails
        assert_eq!(
            auth.authenticate(Some(&format!("Bearer {token}")), false)
                .unwrap_err(),
            AuthError::LeaseRevoked
        );
    }
}
