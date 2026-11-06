//! Per-request user identity, bound from the `X-User-Id` header by middleware
//! and read by code paths that touch per-user document storage. Port of the
//! contextvar half of `stirling.services.tracking` (`current_user_id` +
//! `require_current_user_id`); the PostHog/OTel span processor is tracked
//! separately.

use std::future::Future;

tokio::task_local! {
    static CURRENT_USER_ID: Option<String>;
}

/// Run `fut` with `user_id` bound as the request principal.
pub async fn scoped<F: Future>(user_id: Option<String>, fut: F) -> F::Output {
    CURRENT_USER_ID.scope(user_id, fut).await
}

/// The request's user id, or `None` outside a scoped request / when the
/// `X-User-Id` header was absent.
pub fn current_user_id() -> Option<String> {
    CURRENT_USER_ID.try_with(|id| id.clone()).ok().flatten()
}

/// Fail-closed accessor for per-user storage boundaries: errors when no user
/// id is bound, mirroring Python's `RuntimeError`.
pub fn require_current_user_id() -> Result<String, String> {
    current_user_id().ok_or_else(|| {
        "X-User-Id is required for this operation but was not set on the request".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn require_fails_closed_outside_scope() {
        assert!(require_current_user_id().is_err());
    }

    #[tokio::test]
    async fn scoped_user_id_is_readable() {
        let id = scoped(Some("u-1".to_string()), async {
            require_current_user_id().unwrap()
        })
        .await;
        assert_eq!(id, "u-1");
        assert!(require_current_user_id().is_err());
    }
}
