use std::collections::HashMap;

pub struct PropertyConfig<'a> {
    pub system_properties: &'a HashMap<String, String>,
    pub environment: &'a HashMap<String, String>,
}

impl PropertyConfig<'_> {
    pub fn get_boolean_value(&self, keys: &[&str], default_value: bool) -> bool {
        self.get_string_value(keys, if default_value { "true" } else { "false" })
            .eq_ignore_ascii_case("true")
    }

    pub fn get_string_value(&self, keys: &[&str], default_value: &str) -> String {
        for key in keys {
            if let Some(value) = self
                .system_properties
                .get(*key)
                .or_else(|| self.environment.get(*key))
            {
                return value.clone();
            }
        }
        default_value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::PropertyConfig;

    #[test]
    fn resolves_first_key_and_prefers_system_property() {
        let properties = HashMap::from([
            ("second".to_owned(), "system-second".to_owned()),
            ("first".to_owned(), "system-first".to_owned()),
        ]);
        let environment = HashMap::from([
            ("first".to_owned(), "env-first".to_owned()),
            ("second".to_owned(), "env-second".to_owned()),
        ]);
        let config = PropertyConfig {
            system_properties: &properties,
            environment: &environment,
        };

        assert_eq!(
            config.get_string_value(&["first", "second"], "default"),
            "system-first"
        );
    }

    #[test]
    fn falls_back_to_environment_and_default() {
        let properties = HashMap::new();
        let environment = HashMap::from([("key".to_owned(), "value".to_owned())]);
        let config = PropertyConfig {
            system_properties: &properties,
            environment: &environment,
        };

        assert_eq!(config.get_string_value(&["key"], "default"), "value");
        assert_eq!(config.get_string_value(&["missing"], "default"), "default");
    }

    #[test]
    fn java_boolean_value_of_only_accepts_true() {
        let properties = HashMap::from([
            ("true".to_owned(), "TrUe".to_owned()),
            ("false".to_owned(), "not-a-boolean".to_owned()),
        ]);
        let environment = HashMap::new();
        let config = PropertyConfig {
            system_properties: &properties,
            environment: &environment,
        };

        assert!(config.get_boolean_value(&["true"], false));
        assert!(!config.get_boolean_value(&["false"], true));
        assert!(config.get_boolean_value(&["missing"], true));
        assert!(!config.get_boolean_value(&["missing"], false));
    }
}
