//! Wire-side authentication validation. PAM itself lives in the session launcher.

use arcen_protocol::messages::AuthResponse;
use thiserror::Error;

const MAX_USERNAME_BYTES: usize = 255;
const MAX_CREDENTIAL_BYTES: usize = 1024;
const MAX_MONITORS: usize = 16;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("authentication request is invalid: {0}")]
    InvalidRequest(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionDisplayMode {
    SinglePrimary,
    Windowed,
    MatchLayout,
}

impl SessionDisplayMode {
    pub(crate) fn allows_live_resize(self) -> bool {
        matches!(self, Self::Windowed)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SinglePrimary => "single_primary",
            Self::Windowed => "windowed",
            Self::MatchLayout => "match_layout",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionDisplayPlan {
    pub(crate) mode: SessionDisplayMode,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) served_monitor_id: Option<u32>,
    pub(crate) degradation: Option<SessionDisplayDegradation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionDisplayDegradation {
    MultiMonitorMatchLayout { requested_monitors: usize },
}

pub fn validate_pam_response(response: &AuthResponse) -> Result<(), AuthError> {
    if response.method != "pam" {
        return Err(AuthError::InvalidRequest("expected PAM method"));
    }
    if response.username.is_empty() || response.username.len() > MAX_USERNAME_BYTES {
        return Err(AuthError::InvalidRequest("invalid username length"));
    }
    if response
        .username
        .chars()
        .any(|character| matches!(character, '\0' | '/' | '\\'))
    {
        return Err(AuthError::InvalidRequest("invalid username characters"));
    }
    if response.credential.is_empty() || response.credential.len() > MAX_CREDENTIAL_BYTES {
        return Err(AuthError::InvalidRequest("invalid credential length"));
    }
    if response.monitors.len() > MAX_MONITORS {
        return Err(AuthError::InvalidRequest("too many client monitors"));
    }
    if response.resume_grant.is_some() {
        return Err(AuthError::InvalidRequest(
            "initial PAM authentication carried a resume grant",
        ));
    }
    if response.resume_requested {
        if response
            .resume_holder_nonce
            .as_deref()
            .and_then(super::resume::decode_holder_nonce)
            .is_none()
        {
            return Err(AuthError::InvalidRequest("invalid resume holder nonce"));
        }
    } else if response.resume_holder_nonce.is_some() {
        return Err(AuthError::InvalidRequest(
            "resume holder nonce supplied without opt-in",
        ));
    }
    Ok(())
}

pub(crate) fn session_display_plan(response: &AuthResponse) -> Result<SessionDisplayPlan, String> {
    if response.monitors.len() > MAX_MONITORS {
        return Err("client monitor count exceeds safety limit".to_string());
    }
    let mode = match response.displays_mode.as_str() {
        "" | "single_primary" => SessionDisplayMode::SinglePrimary,
        "windowed" => SessionDisplayMode::Windowed,
        "match_layout" if response.monitors.len() > 1 => SessionDisplayMode::SinglePrimary,
        "match_layout" => SessionDisplayMode::MatchLayout,
        other => {
            return Err(format!(
                "display mode {other:?} is not supported by this host"
            ));
        }
    };
    let served_monitor = response
        .monitors
        .iter()
        .find(|monitor| monitor.is_primary)
        .or_else(|| response.monitors.first());
    let (width, height, served_monitor_id) = served_monitor.map_or(
        (response.screen_width, response.screen_height, None),
        |monitor| (monitor.width_px, monitor.height_px, Some(monitor.id)),
    );
    let degradation = (response.displays_mode == "match_layout" && response.monitors.len() > 1)
        .then_some(SessionDisplayDegradation::MultiMonitorMatchLayout {
            requested_monitors: response.monitors.len(),
        });
    Ok(SessionDisplayPlan {
        mode,
        width,
        height,
        served_monitor_id,
        degradation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::messages::ClientMonitor;

    fn response(username: &str, credential: &str) -> AuthResponse {
        AuthResponse::pam(username, credential)
    }

    fn monitor(id: u32, width_px: u32, height_px: u32, is_primary: bool) -> ClientMonitor {
        ClientMonitor {
            id,
            width_px,
            height_px,
            is_primary,
            scale: 1.0,
            refresh_hz: 60,
            name: format!("Monitor {id}"),
            ..ClientMonitor::default()
        }
    }

    #[test]
    fn accepts_valid_pam_response() {
        assert!(validate_pam_response(&response("artist", "correct horse")).is_ok());
    }

    #[test]
    fn rejects_non_pam_method() {
        let mut response = response("artist", "password");
        response.method = "password".to_string();
        assert!(matches!(
            validate_pam_response(&response),
            Err(AuthError::InvalidRequest("expected PAM method"))
        ));
    }

    #[test]
    fn rejects_empty_or_path_like_username() {
        for username in ["", "../root", "domain\\artist", "bad\0name"] {
            assert!(
                validate_pam_response(&response(username, "password")).is_err(),
                "{username:?} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_empty_or_oversized_credential() {
        assert!(validate_pam_response(&response("artist", "")).is_err());
        assert!(
            validate_pam_response(&response("artist", &"x".repeat(MAX_CREDENTIAL_BYTES + 1)))
                .is_err()
        );
    }

    #[test]
    fn rejects_excessive_monitor_count() {
        let mut response = response("artist", "password");
        response.monitors = vec![Default::default(); MAX_MONITORS + 1];
        assert!(validate_pam_response(&response).is_err());
    }

    #[test]
    fn rejects_resume_material_on_the_pam_route() {
        let mut response = response("artist", "password");
        response.resume_grant = Some("opaque".to_string());
        assert!(validate_pam_response(&response).is_err());

        response.resume_grant = None;
        response.resume_holder_nonce = Some("00".repeat(32));
        assert!(validate_pam_response(&response).is_err());

        response.resume_requested = true;
        response.resume_holder_nonce = Some("malformed".to_string());
        assert!(validate_pam_response(&response).is_err());
    }

    #[test]
    fn session_display_plan_accepts_single_primary_mode() {
        let response = response("artist", "password")
            .with_displays(vec![monitor(7, 2560, 1440, true)], "single_primary");
        let plan = session_display_plan(&response).unwrap();
        assert_eq!(plan.mode, SessionDisplayMode::SinglePrimary);
        assert_eq!((plan.width, plan.height), (2560, 1440));
    }

    #[test]
    fn session_display_plan_accepts_windowed_mode() {
        let response = response("artist", "password")
            .with_displays(vec![monitor(7, 1512, 982, true)], "windowed");
        let plan = session_display_plan(&response).unwrap();
        assert_eq!(plan.mode, SessionDisplayMode::Windowed);
        assert!(plan.mode.allows_live_resize());
        assert_eq!((plan.width, plan.height), (1512, 982));
    }

    #[test]
    fn session_display_plan_accepts_single_monitor_match_layout_mode_without_degradation() {
        let response = response("artist", "password")
            .with_displays(vec![monitor(7, 1920, 1080, true)], "match_layout");
        let plan = session_display_plan(&response).unwrap();
        assert_eq!(plan.mode, SessionDisplayMode::MatchLayout);
        assert_eq!((plan.width, plan.height), (1920, 1080));
        assert_eq!(plan.served_monitor_id, Some(7));
        assert_eq!(plan.degradation, None);
    }

    #[test]
    fn session_display_plan_rejects_unknown_mode_token() {
        let response = response("artist", "password")
            .with_displays(vec![monitor(7, 1920, 1080, true)], "pick");
        let error = session_display_plan(&response).unwrap_err();
        assert_eq!(error, "display mode \"pick\" is not supported by this host");
    }

    #[test]
    fn session_display_plan_degrades_multi_monitor_match_layout_to_primary_monitor() {
        let response = response("artist", "password").with_displays(
            vec![monitor(7, 1920, 1080, true), monitor(8, 1280, 720, false)],
            "match_layout",
        );
        let plan = session_display_plan(&response).unwrap();
        assert_eq!(plan.mode, SessionDisplayMode::SinglePrimary);
        assert_eq!((plan.width, plan.height), (1920, 1080));
        assert_eq!(plan.served_monitor_id, Some(7));
        assert_eq!(
            plan.degradation,
            Some(SessionDisplayDegradation::MultiMonitorMatchLayout {
                requested_monitors: 2
            })
        );
    }

    #[test]
    fn session_display_plan_accepts_legacy_empty_mode() {
        let mut response = response("artist", "password");
        response.screen_width = 1600;
        response.screen_height = 900;
        let plan = session_display_plan(&response).unwrap();
        assert_eq!(plan.mode, SessionDisplayMode::SinglePrimary);
        assert_eq!((plan.width, plan.height), (1600, 900));
    }

    #[test]
    fn session_display_plan_uses_primary_monitor_falling_back_to_first() {
        let explicit_primary = response("artist", "password").with_displays(
            vec![monitor(7, 1280, 720, false), monitor(8, 2560, 1440, true)],
            "single_primary",
        );
        assert_eq!(
            (
                explicit_primary.screen_width,
                explicit_primary.screen_height
            ),
            (2560, 1440)
        );
        let plan = session_display_plan(&explicit_primary).unwrap();
        assert_eq!((plan.width, plan.height), (2560, 1440));

        let no_primary_flag =
            response("artist", "password").with_displays(vec![monitor(7, 1280, 720, false)], "");
        let plan = session_display_plan(&no_primary_flag).unwrap();
        assert_eq!((plan.width, plan.height), (1280, 720));
    }
}
