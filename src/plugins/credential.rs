//! Plugin-backed credential strategy (§6.1, §8.1).
//!
//! A credential plugin *produces or refreshes* an account credential. To keep
//! the secret out of the WIT return value, the plugin writes the credential to
//! its encrypted KV namespace under `lease:<handle>` and returns the opaque
//! `handle`; the host reads it back (decrypting on the host side) and injects it
//! upstream. Core never lets the plugin choose a Route or account.

use std::sync::Arc;

use async_trait::async_trait;

use crate::credentials::{
    CredentialHealth, CredentialRotationError, CredentialStrategy, ResolvedCredential,
};
use crate::crypto::Crypto;
use crate::db::{AccountRow, Pool};

use super::manager::PluginManager;

/// The KV key prefix under which a plugin stores a leased secret.
const LEASE_PREFIX: &str = "lease:";

fn credential_error(fault: super::runtime::PluginFault) -> CredentialRotationError {
    match fault {
        super::runtime::PluginFault::PluginError {
            code,
            message,
            retryable,
            retry_after,
        } => CredentialRotationError::new(code, message, retryable, retry_after),
        // Runtime/host faults are not evidence that the account credential was
        // revoked. Treat them as transient so core never permanently poisons
        // an account because the plugin host failed.
        other => CredentialRotationError::new(other.code(), other.message(), true, None),
    }
}

pub struct PluginCredentialStrategy {
    manager: Arc<PluginManager>,
    pool: Pool,
    crypto: Arc<Crypto>,
    plugin_id: String,
}

impl PluginCredentialStrategy {
    pub fn new(
        manager: Arc<PluginManager>,
        pool: Pool,
        crypto: Arc<Crypto>,
        plugin_id: impl Into<String>,
    ) -> Self {
        PluginCredentialStrategy {
            manager,
            pool,
            crypto,
            plugin_id: plugin_id.into(),
        }
    }

    async fn lease_secret(
        &self,
        handle: &str,
    ) -> std::result::Result<String, CredentialRotationError> {
        let key = format!("{LEASE_PREFIX}{handle}");
        let bytes = super::store::kv_get(&self.pool, &self.crypto, &self.plugin_id, &key)
            .await
            .map_err(|error| {
                CredentialRotationError::new(
                    "plugin_internal",
                    format!("reading plugin credential lease: {error}"),
                    true,
                    None,
                )
            })?
            .ok_or_else(|| {
                CredentialRotationError::new(
                    "plugin_internal",
                    format!(
                        "plugin '{}' returned lease '{handle}' but stored no secret for it",
                        self.plugin_id
                    ),
                    false,
                    None,
                )
            })?;
        String::from_utf8(bytes).map_err(|_| {
            CredentialRotationError::new(
                "plugin_internal",
                "leased credential is not utf-8",
                false,
                None,
            )
        })
    }
}

#[async_trait]
impl CredentialStrategy for PluginCredentialStrategy {
    fn name(&self) -> &'static str {
        "plugin_credential_strategy"
    }

    async fn resolve(
        &self,
        account: &AccountRow,
    ) -> std::result::Result<ResolvedCredential, CredentialRotationError> {
        let lease = self
            .manager
            .credential_resolve(
                &self.plugin_id,
                &account.provider_id,
                &account.id,
                &account.label,
            )
            .await
            .map_err(credential_error)?;
        let secret = self.lease_secret(&lease.handle).await?;
        Ok(ResolvedCredential {
            secret,
            expires_at: lease.expires_at,
            refresh_after: lease.refresh_after,
            rotated: false,
        })
    }

    async fn rotate(
        &self,
        account: &AccountRow,
    ) -> std::result::Result<(), CredentialRotationError> {
        self.manager
            .credential_rotate(&self.plugin_id, &account.provider_id, &account.id)
            .await
            .map_err(credential_error)
    }

    async fn health(&self, account: &AccountRow) -> CredentialHealth {
        match self
            .manager
            .health_probe(&self.plugin_id, &account.provider_id, &account.id)
            .await
        {
            Ok(obs) => match obs.state.as_str() {
                "healthy" => CredentialHealth::Healthy,
                "degraded" => {
                    CredentialHealth::ExpiringSoon(obs.reset_at.unwrap_or_else(|| "unknown".into()))
                }
                _ => CredentialHealth::Unusable(format!("plugin reports state '{}'", obs.state)),
            },
            Err(f) => CredentialHealth::Unusable(f.message()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::runtime::PluginFault;

    #[test]
    fn rotation_error_preserves_retryable_plugin_evidence() {
        let error = credential_error(PluginFault::PluginError {
            code: "upstream_unavailable".into(),
            message: "refresh endpoint unavailable".into(),
            retryable: true,
            retry_after: Some(5),
        });

        assert_eq!(error.code, "upstream_unavailable");
        assert_eq!(error.message, "refresh endpoint unavailable");
        assert!(error.retryable);
        assert_eq!(error.retry_after_secs, Some(5));
        assert!(!error.invalid_credential());
    }

    #[test]
    fn non_retryable_expired_credential_is_terminal() {
        let error = credential_error(PluginFault::PluginError {
            code: "credential_expired".into(),
            message: "refresh token revoked".into(),
            retryable: false,
            retry_after: None,
        });

        assert!(error.invalid_credential());
    }
}
