/// Prints a JSON value when `--json` was passed, otherwise the human text.
pub fn emit(json_flag: bool, human: &str, value: Option<&serde_json::Value>) {
    if json_flag {
        match value {
            Some(v) => println!("{}", serde_json::to_string_pretty(v).unwrap_or_default()),
            None => println!("null"),
        }
    } else {
        println!("{human}");
    }
}
