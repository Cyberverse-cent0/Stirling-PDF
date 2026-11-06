use crate::validation::{is_collection_empty, is_string_empty};

pub struct ProviderCredentials<'a> {
    pub client_id: Option<&'a str>,
    pub client_secret: Option<&'a str>,
    pub scopes: Option<&'a [String]>,
}

/// Applies the Java provider utility's required client credential and scope checks.
pub fn validate_provider(provider: Option<&ProviderCredentials<'_>>) -> bool {
    let Some(provider) = provider else {
        return false;
    };

    !is_string_empty(provider.client_id)
        && !is_string_empty(provider.client_secret)
        && !is_collection_empty(provider.scopes)
}

#[cfg(test)]
mod tests {
    use super::{validate_provider, ProviderCredentials};

    #[test]
    fn accepts_complete_provider_credentials() {
        let scopes = vec!["read:user".to_owned()];
        let provider = ProviderCredentials {
            client_id: Some("client-id"),
            client_secret: Some("client-secret"),
            scopes: Some(&scopes),
        };
        assert!(validate_provider(Some(&provider)));
    }

    #[test]
    fn rejects_missing_or_empty_provider_fields() {
        let scopes = vec!["scope".to_owned()];
        let valid_scopes = Some(scopes.as_slice());

        assert!(!validate_provider(None));
        for (client_id, client_secret, provider_scopes) in [
            (None, Some("secret"), valid_scopes),
            (Some(" "), Some("secret"), valid_scopes),
            (Some("id"), None, valid_scopes),
            (Some("id"), Some(""), valid_scopes),
            (Some("id"), Some("secret"), None),
            (Some("id"), Some("secret"), Some([].as_slice())),
        ] {
            let provider = ProviderCredentials {
                client_id,
                client_secret,
                scopes: provider_scopes,
            };
            assert!(!validate_provider(Some(&provider)));
        }
    }
}
