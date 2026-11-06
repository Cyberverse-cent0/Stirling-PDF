const CORRUPTION_INDICATORS: &[&str] = &[
    "Missing root object specification",
    "Header doesn't contain versioninfo",
    "Expected trailer",
    "Invalid PDF",
    "Corrupted",
    "damaged",
    "Unknown dir object",
    "Can't dereference COSObject",
    "parseCOSString string should start with",
    "ICCBased colorspace array must have a stream",
    "1-based index not found",
    "Invalid dictionary, found:",
    "End-of-File, expected line",
];

/// Checks whether an error message contains a known PDF structural-damage indicator.
pub fn is_corrupted_pdf_error(message: Option<&str>) -> bool {
    message.is_some_and(|message| {
        CORRUPTION_INDICATORS
            .iter()
            .any(|indicator| message.contains(indicator))
    })
}

#[cfg(test)]
mod tests {
    use super::is_corrupted_pdf_error;

    #[test]
    fn recognizes_pdf_structure_errors() {
        for message in [
            "Missing root object specification",
            "Header doesn't contain versioninfo in file",
            "Expected trailer",
            "Invalid PDF structure",
            "Corrupted",
            "damaged",
            "Unknown dir object",
            "Can't dereference COSObject",
            "parseCOSString string should start with",
            "ICCBased colorspace array must have a stream",
            "1-based index not found",
            "Invalid dictionary, found:",
            "End-of-File, expected line",
        ] {
            assert!(is_corrupted_pdf_error(Some(message)), "{message}");
        }
    }

    #[test]
    fn ignores_missing_or_unrelated_messages() {
        assert!(!is_corrupted_pdf_error(None));
        assert!(!is_corrupted_pdf_error(Some("")));
        assert!(!is_corrupted_pdf_error(Some("File not found")));
    }

    #[test]
    fn ignores_decryption_failures() {
        for message in [
            "AES initialization vector not fully read",
            "BadPaddingException",
            "Given final block not properly padded",
        ] {
            assert!(!is_corrupted_pdf_error(Some(message)), "{message}");
        }
    }
}
