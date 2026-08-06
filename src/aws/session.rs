use std::future::Future;

use anyhow::Result;
use aws_config::{BehaviorVersion, Region, SdkConfig};

use crate::aws::sso::{self, TokenStatus};

/// Build an [`SdkConfig`] for the given profile and region.
///
/// Credential resolution (including SSO) is delegated entirely to the AWS SDK,
/// which reads `~/.aws/config` and `~/.aws/credentials` just like the AWS CLI.
pub async fn load_config(profile: &str, region: &str) -> SdkConfig {
    aws_config::defaults(BehaviorVersion::latest())
        .profile_name(profile)
        .region(Region::new(region.to_string()))
        .load()
        .await
}

/// Run `op` (which is expected to build its own [`SdkConfig`] via
/// [`load_config`] and make whatever AWS calls it needs); if it fails *and*
/// `profile`'s cached SSO token turns out to be expired or missing, run
/// `aws sso login` for it and try `op` exactly once more.
///
/// This only ever refreshes reactively, in response to a real failure: a
/// healthy token costs nothing extra here, and an unrelated failure (wrong
/// cluster name, no network, a non-SSO profile, ...) is returned unchanged
/// rather than masked by a pointless login attempt.
pub async fn with_sso_retry<T, F, Fut>(profile: &str, op: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let err = match op().await {
        Ok(value) => return Ok(value),
        Err(err) => err,
    };

    let target = match sso::resolve_for_profile(profile) {
        Ok(Some(target)) => target,
        // Not an SSO profile, or we could not even tell: the failure is
        // unrelated to SSO, surface it as-is.
        _ => return Err(err),
    };

    if matches!(sso::token_status(&target), TokenStatus::Valid { .. }) {
        // The token is fine, so whatever failed is unrelated to SSO.
        return Err(err);
    }

    eprintln!(
        "SSO token for {} has expired or is missing; running `aws sso login`...",
        target.label()
    );
    sso::login(&target)?;

    op().await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[tokio::test]
    async fn returns_ok_without_retry_when_op_succeeds() {
        let calls = AtomicU32::new(0);
        let result: Result<u32> = with_sso_retry("does-not-matter", || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(42)
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn propagates_error_once_for_a_profile_with_no_sso_target() {
        let calls = AtomicU32::new(0);
        // This profile does not exist in any local AWS config, so
        // `resolve_for_profile` resolves to `None`: the failure below must be
        // treated as unrelated to SSO and `op` must not be retried.
        let result: Result<u32> = with_sso_retry("definitely-not-a-real-profile-xyz", || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("boom"))
        })
        .await;

        assert_eq!(result.unwrap_err().to_string(), "boom");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
