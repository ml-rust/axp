//! Static MCP tool adapter for the provider discovery layer.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::{
    Error, Result,
    provider::{CapabilityListing, Provider, ResolvedCommand},
};

/// Bridge command used to execute an external MCP tool through an adapter process.
///
/// `program` is passed directly to `Command::new`, and `args` are fixed argv prefixes. Tool
/// identity and JSON params are appended by [`McpToolProvider::resolve`] without shell
/// interpolation.
///
/// The final argv shape is:
///
/// `program`, `args...`, `--provider`, `<provider_id>`, `--tool`, `<tool_name>`, `--params`,
/// `<serialized-json-params>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpBridgeCommand {
    /// Executable name/path for the MCP bridge process.
    pub program: String,
    /// Fixed arguments passed before the MCP tool binding arguments.
    pub args: Vec<String>,
}

/// Static metadata for one external MCP tool exposed as an AXP provider capability.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDescriptor {
    /// Stable id of the external MCP provider/server this tool belongs to.
    pub provider_id: String,
    /// Local tool name within the MCP provider.
    pub name: String,
    /// One-line human-readable description.
    pub desc: String,
    /// JSON Schema for the tool input.
    pub schema: serde_json::Value,
    /// Bridge command used to invoke this MCP tool.
    pub bridge: McpBridgeCommand,
}

/// Static MCP tool provider adapter.
///
/// This adapter intentionally does not implement MCP transport or wire protocol support. It
/// exposes already-discovered external MCP tools through the existing [`Provider`] seam and
/// resolves invocations to a configured bridge command.
#[derive(Debug)]
pub struct McpToolProvider {
    id: String,
    tools: HashMap<String, McpToolDescriptor>,
}

impl McpToolProvider {
    /// Creates a provider from static MCP tool descriptors.
    ///
    /// Returns [`Error::DuplicateCapability`] if two descriptors share a local tool name.
    /// Returns [`Error::CapabilityParse`] if any descriptor belongs to a different provider id.
    pub fn new(id: impl Into<String>, tools: Vec<McpToolDescriptor>) -> Result<Self> {
        let id = id.into();
        let mut map = HashMap::with_capacity(tools.len());

        for tool in tools {
            if tool.provider_id != id {
                return Err(Error::CapabilityParse {
                    raw: tool.provider_id,
                    reason: format!("MCP tool provider id must match `{id}`"),
                });
            }

            match map.entry(tool.name.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(tool);
                }
                Entry::Occupied(_) => {
                    return Err(Error::DuplicateCapability {
                        provider: id,
                        name: tool.name,
                    });
                }
            }
        }

        Ok(Self { id, tools: map })
    }
}

impl Provider for McpToolProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn index(&self) -> Result<Vec<CapabilityListing>> {
        let mut listings = Vec::with_capacity(self.tools.len());
        for tool in self.tools.values() {
            listings.push(CapabilityListing {
                name: tool.name.clone(),
                desc: tool.desc.clone(),
            });
        }
        Ok(listings)
    }

    fn describe(&self, local_name: &str) -> Result<axp_proto::CapabilityDetail> {
        self.tools
            .get(local_name)
            .map(|tool| axp_proto::CapabilityDetail {
                signature: mcp_tool_signature(&tool.name),
                schema: tool.schema.clone(),
            })
            .ok_or_else(|| Error::CapabilityNotFound {
                name: local_name.to_owned(),
            })
    }

    fn resolve(&self, local_name: &str, params: &serde_json::Value) -> Result<ResolvedCommand> {
        let tool = self
            .tools
            .get(local_name)
            .ok_or_else(|| Error::CapabilityNotFound {
                name: local_name.to_owned(),
            })?;

        let serialized = serde_json::to_string(params).map_err(|err| Error::CapabilityParse {
            raw: local_name.to_owned(),
            reason: format!("failed to serialize MCP tool params: {err}"),
        })?;

        let mut args = Vec::with_capacity(tool.bridge.args.len() + 6);
        args.extend(tool.bridge.args.iter().cloned());
        args.push("--provider".to_owned());
        args.push(tool.provider_id.clone());
        args.push("--tool".to_owned());
        args.push(tool.name.clone());
        args.push("--params".to_owned());
        args.push(serialized);

        Ok(ResolvedCommand {
            program: tool.bridge.program.clone(),
            args,
        })
    }
}

fn mcp_tool_signature(tool_name: &str) -> String {
    format!("{tool_name}(input: object): string")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ProviderRegistry,
        provider::{CapabilityArg, CapabilityDescriptor, ExecutionSpec, NativeProvider},
    };

    fn bridge() -> McpBridgeCommand {
        McpBridgeCommand {
            program: "/usr/local/bin/axp-mcp-bridge".to_owned(),
            args: vec!["call".to_owned()],
        }
    }

    fn tool(name: &str, desc: &str) -> McpToolDescriptor {
        McpToolDescriptor {
            provider_id: "mcp_docs".to_owned(),
            name: name.to_owned(),
            desc: desc.to_owned(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            bridge: bridge(),
        }
    }

    fn provider() -> McpToolProvider {
        McpToolProvider::new(
            "mcp_docs",
            vec![tool(
                "search",
                "Search the configured MCP documentation index",
            )],
        )
        .expect("test descriptors have unique names")
    }

    #[test]
    fn index_lists_only_names_and_descriptions() {
        let provider = provider();

        let listings = provider.index().expect("index must succeed");

        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].name, "search");
        assert_eq!(
            listings[0].desc,
            "Search the configured MCP documentation index"
        );
    }

    #[test]
    fn describe_returns_signature_and_schema() {
        let provider = provider();

        let detail = provider.describe("search").expect("describe must succeed");

        assert_eq!(detail.signature, "search(input: object): string");
        assert_eq!(
            detail.schema,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn resolve_binds_provider_tool_and_serialized_params_as_argv() {
        let provider = provider();

        let cmd = provider
            .resolve(
                "search",
                &serde_json::json!({"query": "rust && rm -rf /", "limit": 3}),
            )
            .expect("resolve must succeed");

        assert_eq!(cmd.program, "/usr/local/bin/axp-mcp-bridge");
        assert_eq!(
            cmd.args,
            vec![
                "call",
                "--provider",
                "mcp_docs",
                "--tool",
                "search",
                "--params",
                r#"{"limit":3,"query":"rust && rm -rf /"}"#
            ]
        );
    }

    #[test]
    fn duplicate_tool_name_returns_duplicate_capability() {
        let err = McpToolProvider::new(
            "mcp_docs",
            vec![
                tool("search", "Search the configured MCP documentation index"),
                tool(
                    "search",
                    "Search another configured MCP documentation index",
                ),
            ],
        )
        .unwrap_err();

        assert!(
            matches!(err, Error::DuplicateCapability { ref provider, ref name }
                if provider == "mcp_docs" && name == "search"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn unknown_tool_returns_capability_not_found() {
        let provider = provider();

        let err = provider.describe("fetch").unwrap_err();

        assert!(
            matches!(err, Error::CapabilityNotFound { ref name } if name == "fetch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn registry_collision_with_native_provider_qualifies_both_names() {
        let native = NativeProvider::new(
            "native",
            vec![CapabilityDescriptor {
                name: "search".to_owned(),
                desc: "Search local native project documentation".to_owned(),
                signature: "search(input: object): string".to_owned(),
                schema: serde_json::json!({"type": "object"}),
                exec: ExecutionSpec {
                    program: "true".to_owned(),
                    args_template: vec![CapabilityArg::Literal("search".to_owned())],
                },
            }],
        )
        .expect("native descriptor is unique");

        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(native)).unwrap();
        registry.register(Box::new(provider())).unwrap();

        let mut names: Vec<String> = registry
            .index()
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        names.sort();

        assert_eq!(names, vec!["mcp_docs:search", "native:search"]);
    }
}
