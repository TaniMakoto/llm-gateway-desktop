//! Tool assembly for observing native streams without rewriting their bytes.
use crate::{validate_tool_completion, ToolArgumentFormat};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
struct Tool {
    name: String,
    arguments: String,
    format: ToolArgumentFormat,
    had_delta: bool,
    terminal: bool,
    global_terminal_sufficient: bool,
}

impl Tool {
    fn new(format: ToolArgumentFormat, global_terminal_sufficient: bool) -> Self {
        Self {
            name: String::new(),
            arguments: String::new(),
            format,
            had_delta: false,
            terminal: false,
            global_terminal_sufficient,
        }
    }
    fn delta(&mut self, value: &str) {
        if !self.had_delta {
            self.arguments.clear();
        }
        self.had_delta = true;
        self.arguments.push_str(value);
    }
    // Compatible Responses streams can supply the name on a later item event.
    // Early done checks validate arguments; the overall terminal requires a name.
    fn early_validation_name(&self) -> &str {
        if self.name.trim().is_empty() {
            "pending_name"
        } else {
            &self.name
        }
    }
}

#[derive(Debug, Default)]
pub struct ToolCompletionObserver {
    tools: BTreeMap<String, Tool>,
    aliases: BTreeMap<String, String>,
    failure: Option<String>,
    custom_chat_names: BTreeSet<String>,
}

impl ToolCompletionObserver {
    /// A Responses custom tool can be adapted to a Chat function on the wire.
    /// Its original text argument semantics must survive that adaptation.
    pub fn with_custom_chat_names(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            custom_chat_names: names.into_iter().collect(),
            ..Self::default()
        }
    }
    pub fn observe(&mut self, event: &Value) -> Option<String> {
        if self.failure.is_some() {
            return self.failure.clone();
        }
        if let Some(choices) = event.get("choices").and_then(Value::as_array) {
            for choice in choices {
                let choice_index = choice.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(calls) = choice
                    .pointer("/delta/tool_calls")
                    .and_then(Value::as_array)
                {
                    for call in calls {
                        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                        let key = format!("chat:{choice_index}:{index}");
                        let custom = call.get("type").and_then(Value::as_str) == Some("custom");
                        let value = call
                            .get(if custom { "custom" } else { "function" })
                            .unwrap_or(&Value::Null);
                        let tool = self.tools.entry(key).or_insert_with(|| {
                            Tool::new(
                                if custom {
                                    ToolArgumentFormat::CustomText
                                } else {
                                    ToolArgumentFormat::JsonObject
                                },
                                false,
                            )
                        });
                        if let Some(name) = value.get("name").and_then(Value::as_str) {
                            if tool.name != name {
                                tool.name.push_str(name);
                            }
                            if self.custom_chat_names.contains(&tool.name) {
                                tool.format = ToolArgumentFormat::CustomText;
                            }
                        }
                        if let Some(arguments) = value
                            .get(if custom { "input" } else { "arguments" })
                            .and_then(Value::as_str)
                        {
                            tool.delta(arguments);
                        }
                    }
                }
                if let Some(function) = choice.pointer("/delta/function_call") {
                    let key = format!("chat:{choice_index}:legacy");
                    let tool = self
                        .tools
                        .entry(key)
                        .or_insert_with(|| Tool::new(ToolArgumentFormat::JsonObject, false));
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        if tool.name != name {
                            tool.name.push_str(name);
                        }
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        tool.delta(arguments);
                    }
                }
                if choice
                    .get("finish_reason")
                    .is_some_and(|value| !value.is_null())
                {
                    let prefix = format!("chat:{choice_index}:");
                    let mut finished = Vec::new();
                    for (key, tool) in self
                        .tools
                        .iter_mut()
                        .filter(|(key, _)| key.starts_with(&prefix))
                    {
                        tool.terminal = true;
                        finished.push(key.clone());
                    }
                    for key in finished {
                        self.validate_one(&key, true);
                    }
                }
            }
        }
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "content_block_start" => {
                if let Some(block) = event
                    .get("content_block")
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
                {
                    let key = format!(
                        "anthropic:{}",
                        event.get("index").and_then(Value::as_u64).unwrap_or(0)
                    );
                    let mut tool = Tool::new(ToolArgumentFormat::JsonObject, true);
                    tool.name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into();
                    tool.arguments = block.get("input").map(Value::to_string).unwrap_or_default();
                    self.tools.insert(key, tool);
                }
            }
            "content_block_delta"
                if event.pointer("/delta/type").and_then(Value::as_str)
                    == Some("input_json_delta") =>
            {
                let key = format!(
                    "anthropic:{}",
                    event.get("index").and_then(Value::as_u64).unwrap_or(0)
                );
                if let Some(tool) = self.tools.get_mut(&key) {
                    tool.delta(
                        event
                            .pointer("/delta/partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    );
                }
            }
            "content_block_stop" => {
                let key = format!(
                    "anthropic:{}",
                    event.get("index").and_then(Value::as_u64).unwrap_or(0)
                );
                if let Some(tool) = self.tools.get_mut(&key) {
                    tool.terminal = true;
                }
                self.validate_one(&key, true);
            }
            "response.output_item.added" | "response.output_item.done" => {
                if let Some(item) = event.get("item") {
                    self.responses_item(event, item, kind.ends_with(".done"));
                }
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                let key = self.responses_key(event, event);
                let format = if kind.contains("custom_tool") {
                    ToolArgumentFormat::CustomText
                } else {
                    ToolArgumentFormat::JsonObject
                };
                let tool = self
                    .tools
                    .entry(key)
                    .or_insert_with(|| Tool::new(format, true));
                if let Some(name) = event.get("name").and_then(Value::as_str) {
                    tool.name = name.into();
                }
                tool.delta(event.get("delta").and_then(Value::as_str).unwrap_or(""));
            }
            "response.function_call_arguments.done" | "response.custom_tool_call_input.done" => {
                let key = self.responses_key(event, event);
                let format = if kind.contains("custom_tool") {
                    ToolArgumentFormat::CustomText
                } else {
                    ToolArgumentFormat::JsonObject
                };
                let arguments = event
                    .get(if kind.contains("custom_tool") {
                        "input"
                    } else {
                        "arguments"
                    })
                    .and_then(Value::as_str);
                {
                    let tool = self
                        .tools
                        .entry(key.clone())
                        .or_insert_with(|| Tool::new(format, true));
                    if let Some(name) = event.get("name").and_then(Value::as_str) {
                        tool.name = name.into();
                    }
                    if let Some(arguments) = arguments {
                        if let Err(problem) = validate_tool_completion(
                            tool.early_validation_name(),
                            arguments,
                            tool.format,
                            true,
                        ) {
                            self.failure =
                                Some(format!("Incomplete upstream tool call ({key}): {problem}"));
                        }
                    }
                    if !tool.had_delta {
                        if let Some(arguments) = arguments {
                            tool.arguments = arguments.into();
                        }
                    }
                    tool.terminal = true;
                }
                self.validate_one(&key, true);
            }
            "response.completed" | "response.incomplete" => {
                if let Some(items) = event.pointer("/response/output").and_then(Value::as_array) {
                    for item in items {
                        self.responses_item(&Value::Null, item, true);
                    }
                }
            }
            _ => {}
        }
        self.failure.clone()
    }

    pub fn completion_error(&self, has_terminal: bool) -> Option<String> {
        self.failure.clone().or_else(|| {
            self.tools.iter().find_map(|(key, tool)| {
                validate_tool_completion(
                    &tool.name,
                    &tool.arguments,
                    tool.format,
                    tool.terminal || (has_terminal && tool.global_terminal_sufficient),
                )
                .err()
                .map(|problem| format!("Incomplete upstream tool call ({key}): {problem}"))
            })
        })
    }

    fn validate_one(&mut self, key: &str, terminal: bool) {
        if let Some(tool) = self.tools.get(key) {
            if let Err(problem) = validate_tool_completion(
                tool.early_validation_name(),
                &tool.arguments,
                tool.format,
                terminal,
            ) {
                self.failure = Some(format!("Incomplete upstream tool call ({key}): {problem}"));
            }
        }
    }

    fn responses_key(&mut self, event: &Value, item: &Value) -> String {
        let mut aliases = Vec::new();
        if let Some(id) = item
            .get("id")
            .or_else(|| event.get("item_id"))
            .and_then(Value::as_str)
        {
            aliases.push(format!("responses:id:{id}"));
        }
        if let Some(index) = event.get("output_index").and_then(Value::as_u64) {
            aliases.push(format!("responses:index:{index}"));
        }
        let key = aliases
            .iter()
            .find_map(|alias| self.aliases.get(alias).cloned())
            .or_else(|| aliases.first().cloned())
            .unwrap_or_else(|| "responses:unknown".into());
        for alias in aliases {
            self.aliases.insert(alias, key.clone());
        }
        key
    }

    fn responses_item(&mut self, event: &Value, item: &Value, terminal: bool) {
        let kind = item.get("type").and_then(Value::as_str);
        let format = match kind {
            Some("function_call") => ToolArgumentFormat::JsonObject,
            Some("custom_tool_call") => ToolArgumentFormat::CustomText,
            _ => return,
        };
        let key = self.responses_key(event, item);
        let tool = self
            .tools
            .entry(key.clone())
            .or_insert_with(|| Tool::new(format, true));
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            tool.name = name.into();
        }
        if terminal {
            if let Some(arguments) = item
                .get(if format == ToolArgumentFormat::CustomText {
                    "input"
                } else {
                    "arguments"
                })
                .and_then(Value::as_str)
            {
                if let Err(problem) =
                    validate_tool_completion(tool.early_validation_name(), arguments, format, true)
                {
                    self.failure =
                        Some(format!("Incomplete upstream tool call ({key}): {problem}"));
                }
            }
        }
        if !tool.had_delta {
            if let Some(arguments) = item
                .get(if format == ToolArgumentFormat::CustomText {
                    "input"
                } else {
                    "arguments"
                })
                .and_then(Value::as_str)
            {
                tool.arguments = arguments.into();
            }
        }
        tool.terminal |= terminal;
        if terminal {
            self.validate_one(&key, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_requires_object_arguments_and_an_explicit_choice_finish() {
        for finish in [Value::Null, json!("tool_calls"), json!("length")] {
            let mut observer = ToolCompletionObserver::default();
            observer.observe(&json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"lookup","arguments":"{\"private\": "}}]}}]}));
            observer.observe(&json!({"choices":[{"index":0,"finish_reason":finish}]}));
            let error = observer.completion_error(true).unwrap();
            assert!(!error.contains("private"));
        }
        let mut observer = ToolCompletionObserver::default();
        observer.observe(&json!({"choices":[{"delta":{"tool_calls":[{"function":{"name":"lookup","arguments":"{}"}}]}}]}));
        assert!(
            observer.completion_error(true).is_some(),
            "[DONE] cannot replace finish_reason for a tool"
        );
        observer.observe(&json!({"choices":[{"finish_reason":"tool_calls"}]}));
        assert!(observer.completion_error(true).is_none());
    }

    #[test]
    fn legacy_chat_functions_and_custom_text_keep_their_own_argument_semantics() {
        let mut observer = ToolCompletionObserver::default();
        observer.observe(&json!({"choices":[{"index":0,"delta":{"function_call":{"name":"lookup","arguments":"{}"}}}]}));
        observer.observe(&json!({"choices":[{"index":1,"delta":{"tool_calls":[{"type":"custom","custom":{"name":"shell","input":"ls -la && echo 文本"}}]}}]}));
        observer.observe(&json!({"choices":[{"index":0,"finish_reason":"function_call"},{"index":1,"finish_reason":"tool_calls"}]}));
        assert!(observer.completion_error(true).is_none());
        let mut adapted = ToolCompletionObserver::with_custom_chat_names(["shell".to_string()]);
        adapted.observe(&json!({"choices":[{"delta":{"tool_calls":[{"type":"function","function":{"name":"shell","arguments":"echo 文本"}}]}}]}));
        adapted.observe(&json!({"choices":[{"finish_reason":"tool_calls"}]}));
        assert!(adapted.completion_error(true).is_none());
    }

    #[test]
    fn messages_replace_initial_empty_input_and_reject_a_half_tool_at_block_stop() {
        for arguments in ["{\"q\":\"中文\"}", "{\"q\":"] {
            let mut observer = ToolCompletionObserver::default();
            observer.observe(&json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","name":"lookup","input":{}}}));
            observer.observe(&json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":arguments}}));
            observer.observe(&json!({"type":"content_block_stop","index":2}));
            assert_eq!(
                observer.completion_error(true).is_none(),
                arguments.ends_with('}')
            );
        }
    }

    #[test]
    fn responses_join_id_and_index_and_validate_both_delta_and_final_arguments() {
        for final_arguments in ["{\"q\":\"中文\"}", "{", "[]"] {
            let mut observer = ToolCompletionObserver::default();
            observer.observe(&json!({"type":"response.output_item.added","output_index":2,"item":{"id":"fc_a","type":"function_call","name":"lookup","arguments":""}}));
            observer.observe(&json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"q\":\"中文\"}"}));
            observer.observe(&json!({"type":"response.function_call_arguments.done","item_id":"fc_a","arguments":final_arguments}));
            assert_eq!(
                observer.completion_error(true).is_none(),
                final_arguments.ends_with('}')
            );
        }
    }

    #[test]
    fn responses_terminal_only_tools_and_custom_input_are_observed() {
        let mut observer = ToolCompletionObserver::default();
        observer.observe(&json!({"type":"response.completed","response":{"output":[{"id":"fc_a","type":"function_call","name":"lookup","arguments":"{}"},{"id":"ct_a","type":"custom_tool_call","name":"shell","input":"echo 文本"}]}}));
        assert!(observer.completion_error(true).is_none());
        let mut observer = ToolCompletionObserver::default();
        observer.observe(&json!({"type":"response.incomplete","response":{"output":[{"id":"fc_a","type":"function_call","name":"lookup","arguments":"{"}]}}));
        assert!(observer.completion_error(true).is_some());
    }

    #[test]
    fn valid_accumulated_arguments_cannot_hide_an_invalid_final_output_item() {
        let mut observer = ToolCompletionObserver::default();
        observer.observe(&json!({"type":"response.output_item.added","output_index":0,"item":{"id":"fc_a","type":"function_call","name":"lookup","arguments":""}}));
        observer.observe(
            &json!({"type":"response.function_call_arguments.delta","item_id":"fc_a","delta":"{}"}),
        );
        observer.observe(&json!({"type":"response.completed","response":{"output":[{"id":"fc_a","type":"function_call","name":"lookup","arguments":"{"}]}}));
        assert!(observer.completion_error(true).is_some());
    }
}
