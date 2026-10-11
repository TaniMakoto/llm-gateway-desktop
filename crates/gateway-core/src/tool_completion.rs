//! Shared semantic completion checks. Never include tool arguments in errors.
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolArgumentFormat {
    JsonObject,
    /// Custom tools carry text, rather than function-call JSON objects.
    CustomText,
}

pub fn validate_tool_completion(
    name: &str,
    arguments: &str,
    format: ToolArgumentFormat,
    has_terminal: bool,
) -> Result<(), &'static str> {
    if name.trim().is_empty() {
        return Err("tool name is missing");
    }
    if !has_terminal {
        return Err("tool call has no semantic completion marker");
    }
    if format == ToolArgumentFormat::JsonObject
        && !serde_json::from_str::<Value>(arguments).is_ok_and(|value| value.is_object())
    {
        return Err("tool arguments are incomplete or are not a JSON object");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_arguments_require_a_complete_object_and_terminal() {
        for arguments in ["", " ", "{", "null", "[]", "{} trailing"] {
            assert!(validate_tool_completion(
                "lookup",
                arguments,
                ToolArgumentFormat::JsonObject,
                true
            )
            .is_err());
        }
        assert!(
            validate_tool_completion("lookup", "{}", ToolArgumentFormat::JsonObject, false)
                .is_err()
        );
        assert!(validate_tool_completion("", "{}", ToolArgumentFormat::JsonObject, true).is_err());
        assert!(validate_tool_completion(
            "lookup",
            "{\"城市\":\"北京\"}",
            ToolArgumentFormat::JsonObject,
            true
        )
        .is_ok());
    }

    #[test]
    fn custom_tools_keep_text_semantics_but_still_require_a_terminal() {
        for arguments in ["", "ls -la", "{unfinished text"] {
            assert!(validate_tool_completion(
                "shell",
                arguments,
                ToolArgumentFormat::CustomText,
                true
            )
            .is_ok());
            assert!(validate_tool_completion(
                "shell",
                arguments,
                ToolArgumentFormat::CustomText,
                false
            )
            .is_err());
        }
    }
}
