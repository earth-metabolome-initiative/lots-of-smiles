//! Enamine login credentials and their builder.
//!
//! The credential type redacts its password in `Debug` output.

use crate::{LosError, Result};

/// Default environment variable holding the Enamine username.
pub const ENV_USERNAME: &str = "ENAMINE_USERNAME";
/// Default environment variable holding the Enamine password.
pub const ENV_PASSWORD: &str = "ENAMINE_PASSWORD";

/// Validated Enamine.net credentials. Build via [`EnamineCredentials::builder`].
///
/// `Debug` deliberately redacts the password.
#[derive(Clone)]
pub struct EnamineCredentials {
    pub(crate) username: String,
    // Consumed by the Enamine adapter's login flow.
    pub(crate) password: String,
}

impl EnamineCredentials {
    /// Starts a new [`EnamineCredentialsBuilder`].
    pub fn builder() -> EnamineCredentialsBuilder {
        EnamineCredentialsBuilder::default()
    }
}

impl core::fmt::Debug for EnamineCredentials {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EnamineCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Builder for [`EnamineCredentials`].
#[derive(Debug, Clone, Default)]
pub struct EnamineCredentialsBuilder {
    username: Option<String>,
    password: Option<String>,
}

impl EnamineCredentialsBuilder {
    /// Sets the username explicitly.
    pub fn username(mut self, u: impl Into<String>) -> Self {
        self.username = Some(u.into());
        self
    }
    /// Sets the password explicitly.
    pub fn password(mut self, p: impl Into<String>) -> Self {
        self.password = Some(p.into());
        self
    }

    /// Fills any unset field from the [`ENV_USERNAME`] / [`ENV_PASSWORD`]
    /// environment variables.
    ///
    /// Explicitly set values take precedence over the environment.
    ///
    /// # Errors
    ///
    /// Returns [`LosError::Credentials`] if a field is neither set explicitly
    /// nor present in the environment.
    pub fn from_env(mut self) -> Result<Self> {
        if self.username.is_none() {
            self.username = std::env::var(ENV_USERNAME).ok();
        }
        if self.password.is_none() {
            self.password = std::env::var(ENV_PASSWORD).ok();
        }
        if self.username.is_none() {
            return Err(LosError::Credentials(format!(
                "username not set and ${ENV_USERNAME} unset"
            )));
        }
        if self.password.is_none() {
            return Err(LosError::Credentials(format!(
                "password not set and ${ENV_PASSWORD} unset"
            )));
        }
        Ok(self)
    }

    /// Validates and finalizes the credentials.
    ///
    /// # Errors
    ///
    /// Returns [`LosError::Credentials`] if username or password is missing or
    /// empty.
    pub fn build(self) -> Result<EnamineCredentials> {
        let username = self
            .username
            .filter(|s| !s.is_empty())
            .ok_or_else(|| LosError::Credentials("username is required".into()))?;
        let password = self
            .password
            .filter(|s| !s.is_empty())
            .ok_or_else(|| LosError::Credentials("password is required".into()))?;
        Ok(EnamineCredentials { username, password })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_credentials_build() {
        let c = EnamineCredentials::builder()
            .username("alice")
            .password("secret")
            .build()
            .unwrap();
        assert_eq!(c.username, "alice");
        // Debug must not leak the password.
        let dbg = format!("{c:?}");
        assert!(dbg.contains("alice"));
        assert!(!dbg.contains("secret"));
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn missing_fields_rejected() {
        assert!(matches!(
            EnamineCredentials::builder().username("x").build(),
            Err(LosError::Credentials(_))
        ));
        assert!(matches!(
            EnamineCredentials::builder().password("x").build(),
            Err(LosError::Credentials(_))
        ));
    }

    #[test]
    fn empty_fields_rejected() {
        assert!(matches!(
            EnamineCredentials::builder()
                .username("")
                .password("p")
                .build(),
            Err(LosError::Credentials(_))
        ));
    }
}
