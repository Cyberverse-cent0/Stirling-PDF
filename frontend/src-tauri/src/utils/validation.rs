/// Returns whether a missing or Java-blank string contains no meaningful characters.
pub fn is_string_empty(value: Option<&str>) -> bool {
    match value {
        None => true,
        Some(value) => value.chars().all(is_java_whitespace),
    }
}

/// Returns whether a missing collection has no elements.
pub fn is_collection_empty<T>(value: Option<&[T]>) -> bool {
    match value {
        None => true,
        Some(value) => value.is_empty(),
    }
}

fn is_java_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'..='\u{000D}'
            | '\u{001C}'..='\u{0020}'
            | '\u{1680}'
            | '\u{2000}'..='\u{2006}'
            | '\u{2008}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{205F}'
            | '\u{3000}'
    )
}

#[cfg(test)]
mod tests {
    use super::{is_collection_empty, is_string_empty};

    #[test]
    fn treats_missing_and_java_blank_strings_as_empty() {
        assert!(is_string_empty(None));
        assert!(is_string_empty(Some("")));
        assert!(is_string_empty(Some(" \t\n\u{2003}")));
    }

    #[test]
    fn does_not_treat_nonbreaking_space_as_java_blank() {
        assert!(!is_string_empty(Some("\u{00A0}")));
        assert!(!is_string_empty(Some("\u{2007}")));
        assert!(!is_string_empty(Some("\u{202F}")));
    }

    #[test]
    fn treats_null_and_empty_collections_as_empty() {
        let empty: [String; 0] = [];
        assert!(is_collection_empty::<String>(None));
        assert!(is_collection_empty(Some(&empty)));
    }

    #[test]
    fn nonempty_collection_is_not_empty() {
        let values = ["", "value"];
        assert!(!is_collection_empty(Some(&values)));
    }
}
