//! Per-request reply language, bound by the orchestrator, read by prompt
//! builders. Port of `stirling.services.language` — the Python contextvar
//! becomes a tokio task-local scoped around the agent run.

use std::future::Future;

tokio::task_local! {
    static REPLY_LOCALE: Option<String>;
}

/// Run `fut` with `locale` bound as the reply language for this task tree.
pub async fn scoped<F: Future>(locale: Option<String>, fut: F) -> F::Output {
    REPLY_LOCALE.scope(locale, fut).await
}

/// Prompt line pinning the reply language; append to any user-facing prompt.
pub fn language_directive() -> String {
    match REPLY_LOCALE.try_with(|l| l.clone()).ok().flatten() {
        None => {
            "Write anything the user will read in the same language as their message.".to_string()
        }
        Some(locale) => format!(
            "Write anything the user will read in the language of locale '{locale}', whatever \
             language this prompt, the documents, or the tool output are in. Only a different \
             language the user explicitly asks for overrides this."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_directive_without_locale() {
        assert_eq!(
            language_directive(),
            "Write anything the user will read in the same language as their message."
        );
    }

    #[tokio::test]
    async fn scoped_locale_changes_directive() {
        let out = scoped(Some("fr-FR".to_string()), async { language_directive() }).await;
        assert!(out.contains("locale 'fr-FR'"));
        assert_eq!(
            language_directive(),
            "Write anything the user will read in the same language as their message."
        );
    }
}
