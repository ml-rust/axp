use std::{fs::File, path::Path};

use serde::Deserialize;

fn default_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": true
    })
}

fn default_bridge_args() -> Vec<String> {
    vec!["call".to_owned()]
}

fn reject_empty(field: &str, value: &str) -> std::io::Result<()> {
    if value.trim().is_empty() {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("MCP config {field} must not be empty"),
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct McpConfigMount {
    pub(crate) provider_id: String,
    pub(crate) bridge_program: String,
    pub(crate) bridge_args: Vec<String>,
    pub(crate) tools: Vec<(String, String, serde_json::Value)>,
}

#[derive(Debug, Deserialize)]
struct McpConfigFile {
    provider: String,
    bridge: BridgeConfig,
    tools: Vec<ToolConfig>,
}

#[derive(Debug, Deserialize)]
struct BridgeConfig {
    program: String,
    #[serde(default = "default_bridge_args")]
    args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ToolConfig {
    name: String,
    desc: String,
    #[serde(default = "default_schema")]
    schema: serde_json::Value,
}

impl McpConfigFile {
    fn into_mount(self) -> std::io::Result<McpConfigMount> {
        reject_empty("provider", &self.provider)?;
        reject_empty("bridge.program", &self.bridge.program)?;

        if self.tools.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "MCP config must define at least one tool",
            ));
        }

        let mut tools = Vec::with_capacity(self.tools.len());
        for (index, tool) in self.tools.into_iter().enumerate() {
            if tool.name.trim().is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("MCP config tool[{index}].name must not be empty"),
                ));
            }
            if tool.desc.trim().is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("MCP config tool[{index}].desc must not be empty"),
                ));
            }
            tools.push((tool.name, tool.desc, tool.schema));
        }

        Ok(McpConfigMount {
            provider_id: self.provider,
            bridge_program: self.bridge.program,
            bridge_args: self.bridge.args,
            tools,
        })
    }
}

pub(crate) fn load(path: impl AsRef<Path>) -> std::io::Result<McpConfigMount> {
    let file = File::open(path)?;
    let config: McpConfigFile = serde_json::from_reader(file).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid MCP config JSON: {err}"),
        )
    })?;

    config.into_mount()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(contents: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(file.path(), contents).expect("write config");
        file
    }

    #[test]
    fn load_defaults_schema_and_bridge_args() {
        let file = write_config(
            r#"{
                "provider": "docs",
                "bridge": { "program": "axp-mcp-bridge" },
                "tools": [
                    {
                        "name": "search",
                        "desc": "Search documentation with an external MCP bridge"
                    }
                ]
            }"#,
        );

        let mount = load(file.path()).expect("config");

        assert_eq!(mount.provider_id, "docs");
        assert_eq!(mount.bridge_program, "axp-mcp-bridge");
        assert_eq!(mount.bridge_args, vec!["call"]);
        assert_eq!(mount.tools.len(), 1);
        assert_eq!(
            mount.tools[0].2,
            serde_json::json!({
                "type": "object",
                "additionalProperties": true
            })
        );
    }

    #[test]
    fn load_preserves_supplied_bridge_args() {
        let file = write_config(
            r#"{
                "provider": "docs",
                "bridge": {
                    "program": "axp-mcp-bridge",
                    "args": ["invoke", "--json"]
                },
                "tools": [
                    {
                        "name": "search",
                        "desc": "Search documentation with an external MCP bridge"
                    }
                ]
            }"#,
        );

        let mount = load(file.path()).expect("config");

        assert_eq!(mount.bridge_args, vec!["invoke", "--json"]);
    }

    #[test]
    fn load_rejects_empty_tools() {
        let file = write_config(
            r#"{
                "provider": "docs",
                "bridge": { "program": "axp-mcp-bridge" },
                "tools": []
            }"#,
        );

        let err = load(file.path()).expect_err("empty tools must fail");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("at least one tool"));
    }

    #[test]
    fn load_rejects_empty_public_fields() {
        for (contents, expected) in [
            (
                r#"{
                    "provider": "",
                    "bridge": { "program": "axp-mcp-bridge" },
                    "tools": [
                        { "name": "search", "desc": "Search documentation with an external MCP bridge" }
                    ]
                }"#,
                "provider",
            ),
            (
                r#"{
                    "provider": "docs",
                    "bridge": { "program": "" },
                    "tools": [
                        { "name": "search", "desc": "Search documentation with an external MCP bridge" }
                    ]
                }"#,
                "bridge.program",
            ),
            (
                r#"{
                    "provider": "docs",
                    "bridge": { "program": "axp-mcp-bridge" },
                    "tools": [
                        { "name": "", "desc": "Search documentation with an external MCP bridge" }
                    ]
                }"#,
                "tool[0].name",
            ),
            (
                r#"{
                    "provider": "docs",
                    "bridge": { "program": "axp-mcp-bridge" },
                    "tools": [
                        { "name": "search", "desc": "" }
                    ]
                }"#,
                "tool[0].desc",
            ),
        ] {
            let file = write_config(contents);

            let err = load(file.path()).expect_err("empty field must fail");

            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
            assert!(
                err.to_string().contains(expected),
                "expected `{expected}` in `{err}`"
            );
        }
    }
}
