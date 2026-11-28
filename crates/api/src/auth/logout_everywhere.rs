//! "Log out everywhere": the switch and the identity provider's end-session
//! URL it sends the browser to (docs/session-revocation.md, "Log out
//! everywhere").
//!
//! Two settings, both read once at startup:
//!
//! - `LOGOUT_EVERYWHERE_ENABLED` (default false): the route answers `404`
//!   and `GET /auth/session` says `logoutEverywhereEnabled: false`.
//! - `SSO_END_SESSION_URL`: the provider's `end_session_endpoint`, given
//!   explicitly. OIDC discovery is lazy here (nothing at startup touches the
//!   network, `auth::oidc`), and `CoreProviderMetadata` does not carry the
//!   endpoint, so it is not derived. Required when the switch is on; a
//!   value that is not a plain `https` URL stops the api, switch or not.

use anyhow::{Result, bail};
use openidconnect::url::Url;

/// Turns the route on (chart `api.logoutEverywhere.enabled`, default false).
pub const ENABLED_ENV: &str = "LOGOUT_EVERYWHERE_ENABLED";
/// The identity provider's end-session URL (chart
/// `api.logoutEverywhere.endSessionUrl`). Required with [`ENABLED_ENV`].
pub const END_SESSION_URL_ENV: &str = "SSO_END_SESSION_URL";

/// The longest end-session URL accepted.
const MAX_URL_LEN: usize = 2048;

/// The settings. `Default` is off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogoutEverywhere {
    /// `Some` only when the feature is on (so it is never on without a URL).
    end_session_url: Option<String>,
}

impl LogoutEverywhere {
    /// Reads [`ENABLED_ENV`] (default false; an unreadable value is false,
    /// with a warning, as for the other feature switches) and
    /// [`END_SESSION_URL_ENV`].
    pub fn from_env() -> Result<Self> {
        let enabled = crate::data::trip_plan_live::env_bool(ENABLED_ENV, false);
        let url = std::env::var(END_SESSION_URL_ENV).ok();
        Self::from_values(enabled, url.as_deref())
    }

    /// [`Self::from_env`] without the environment. A blank URL is unset.
    pub fn from_values(enabled: bool, url: Option<&str>) -> Result<Self> {
        let url = url.map(str::trim).filter(|url| !url.is_empty());
        let validated = url.map(validate_end_session_url).transpose()?;
        if !enabled {
            return Ok(Self::default());
        }
        let Some(end_session_url) = validated else {
            bail!("{ENABLED_ENV} is on but {END_SESSION_URL_ENV} is not set");
        };
        Ok(Self {
            end_session_url: Some(end_session_url),
        })
    }

    /// Whether the route is on.
    pub fn enabled(&self) -> bool {
        self.end_session_url.is_some()
    }

    /// The URL the browser is sent to; `None` while the feature is off.
    pub fn end_session_url(&self) -> Option<&str> {
        self.end_session_url.as_deref()
    }

    /// On, with `url`, for a test.
    #[cfg(test)]
    pub(crate) fn for_tests(url: &str) -> Self {
        Self {
            end_session_url: Some(url.to_string()),
        }
    }
}

/// A usable end-session URL: absolute `https` with a host, no user info, no
/// query and no fragment (the frontend navigates to it as given, and the
/// provider needs no parameters). Returns the normalised form.
pub fn validate_end_session_url(raw: &str) -> Result<String> {
    if raw.len() > MAX_URL_LEN {
        bail!("{END_SESSION_URL_ENV} is longer than {MAX_URL_LEN} bytes");
    }
    let url = match Url::parse(raw) {
        Ok(url) => url,
        Err(err) => bail!("{END_SESSION_URL_ENV} is not a URL: {err}"),
    };
    if url.scheme() != "https" {
        bail!("{END_SESSION_URL_ENV} must be an https URL");
    }
    if url.host_str().is_none_or(str::is_empty) {
        bail!("{END_SESSION_URL_ENV} must have a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("{END_SESSION_URL_ENV} must not carry a user name or password");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("{END_SESSION_URL_ENV} must have no query or fragment");
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://ds-sso.cursed.solutions/application/o/distant-signal/end-session/";

    #[test]
    fn off_by_default_and_with_no_url() {
        assert!(!LogoutEverywhere::default().enabled());
        assert_eq!(LogoutEverywhere::default().end_session_url(), None);
        let off = LogoutEverywhere::from_values(false, None).expect("off");
        assert!(!off.enabled());
    }

    #[test]
    fn a_valid_url_with_the_switch_on_enables_it_unchanged() {
        let on = LogoutEverywhere::from_values(true, Some(URL)).expect("on");
        assert!(on.enabled());
        assert_eq!(on.end_session_url(), Some(URL));
    }

    #[test]
    fn the_switch_off_keeps_the_feature_off_even_with_a_url() {
        let off = LogoutEverywhere::from_values(false, Some(URL)).expect("off");
        assert!(!off.enabled());
        assert_eq!(off.end_session_url(), None);
    }

    #[test]
    fn on_without_a_url_refuses_to_start() {
        assert!(LogoutEverywhere::from_values(true, None).is_err());
        assert!(LogoutEverywhere::from_values(true, Some("  ")).is_err());
    }

    #[test]
    fn a_bad_url_refuses_to_start_even_with_the_switch_off() {
        assert!(LogoutEverywhere::from_values(false, Some("http://sso.example/end/")).is_err());
    }

    #[test]
    fn only_a_plain_https_url_is_accepted() {
        for bad in [
            "",
            "not a url",
            "http://ds-sso.cursed.solutions/end-session/",
            "//ds-sso.cursed.solutions/end-session/",
            "/application/o/distant-signal/end-session/",
            "javascript:alert(1)",
            "https://",
            "https://user:pass@ds-sso.cursed.solutions/end-session/",
            "https://ds-sso.cursed.solutions/end-session/?state=x",
            "https://ds-sso.cursed.solutions/end-session/#frag",
            "https://ds-sso.cursed.solutions/end-session/?",
        ] {
            assert!(validate_end_session_url(bad).is_err(), "{bad:?} accepted");
        }
        assert_eq!(validate_end_session_url(URL).expect("valid"), URL);
    }
}
