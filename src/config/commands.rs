//! Command configuration types for project hooks
//!
//! Handles parsing and representation of commands that run during various phases
//! of worktree and merge operations.

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};

/// Represents a command with its template and optionally expanded form
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// Optional name for the command (e.g., "build", "test", or auto-numbered "1", "2")
    pub name: Option<String>,
    /// Template string that may contain variables like {{ branch }}, {{ worktree }}
    pub template: String,
    /// Expanded command with variables substituted (same as template if not expanded yet)
    pub expanded: String,
    /// When true, this command runs serially (blocking) before concurrent commands are
    /// spawned. Only meaningful for post-* hooks; ignored for pre-* hooks (already serial).
    pub wait: bool,
}

impl Command {
    /// Create a new command from a template (not yet expanded)
    pub fn new(name: Option<String>, template: String) -> Self {
        Self {
            name,
            expanded: template.clone(),
            template,
            wait: false,
        }
    }
}

/// Configuration for commands - canonical representation
///
/// Internally stores commands as `Vec<Command>` for uniform processing.
/// Deserializes from two TOML formats:
/// - Single string: `post-create = "npm install"`
/// - Named table: `[post-create]` followed by `install = "npm install"`
///
/// **Order preservation:** Named commands preserve TOML insertion order (requires
/// `preserve_order` feature on toml crate and IndexMap for deserialization). This
/// allows users to control execution order explicitly.
///
/// This canonical form eliminates branching at call sites - code just iterates over commands.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandConfig {
    commands: Vec<Command>,
}

impl CommandConfig {
    /// Returns the commands as a slice
    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    /// Merge two configs by appending commands (base commands first, then overlay).
    ///
    /// Used for per-project hook overrides where both global and project hooks run.
    pub fn merge_append(&self, other: &Self) -> Self {
        let mut commands = self.commands.clone();
        commands.extend(other.commands.iter().cloned());
        Self { commands }
    }
}

// Custom deserialization to handle 2 TOML formats
impl<'de> Deserialize<'de> for CommandConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// A single command value: either a plain string or `{ run = "...", wait = true }`.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum CommandValue {
            Simple(String),
            Extended {
                run: String,
                #[serde(default)]
                wait: bool,
            },
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum CommandConfigToml {
            Single(String),
            // Named must come before SingleExtended: a table like `{ run = "..." }`
            // matches Named (command named "run") and should stay that way.
            // SingleExtended only matches when Named fails (e.g., `wait = true` is
            // a bool, not a valid CommandValue).
            Named(IndexMap<String, CommandValue>),
            SingleExtended {
                run: String,
                #[serde(default)]
                wait: bool,
            },
        }

        let toml = CommandConfigToml::deserialize(deserializer)?;
        let commands = match toml {
            CommandConfigToml::Single(cmd) => {
                vec![Command::new(None, cmd)]
            }
            CommandConfigToml::SingleExtended { run, wait } => {
                vec![Command {
                    name: None,
                    expanded: run.clone(),
                    template: run,
                    wait,
                }]
            }
            CommandConfigToml::Named(map) => {
                // IndexMap preserves insertion order from TOML
                // Validate hook names don't contain colons (would break log spec parsing)
                for name in map.keys() {
                    if name.contains(':') {
                        return Err(serde::de::Error::custom(format!(
                            "hook name '{}' cannot contain colons",
                            name
                        )));
                    }
                }
                map.into_iter()
                    .map(|(name, value)| match value {
                        CommandValue::Simple(template) => Command::new(Some(name), template),
                        CommandValue::Extended { run, wait } => Command {
                            name: Some(name),
                            expanded: run.clone(),
                            template: run,
                            wait,
                        },
                    })
                    .collect()
            }
        };
        Ok(CommandConfig { commands })
    }
}

// JsonSchema for CommandConfig - describes the two TOML formats
impl JsonSchema for CommandConfig {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "CommandConfig".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // CommandConfig accepts: a string, an extended object { run, wait? },
        // or a named table where each value is a string or extended object.
        schemars::json_schema!({
            "oneOf": [
                { "type": "string" },
                {
                    "type": "object",
                    "properties": {
                        "run": { "type": "string" },
                        "wait": { "type": "boolean", "default": false }
                    },
                    "required": ["run"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "additionalProperties": {
                        "oneOf": [
                            { "type": "string" },
                            {
                                "type": "object",
                                "properties": {
                                    "run": { "type": "string" },
                                    "wait": { "type": "boolean", "default": false }
                                },
                                "required": ["run"],
                                "additionalProperties": false
                            }
                        ]
                    }
                }
            ]
        })
    }
}

// Serialize back to most appropriate format
impl Serialize for CommandConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // If single unnamed command without wait, serialize as string
        if self.commands.len() == 1 && self.commands[0].name.is_none() && !self.commands[0].wait {
            return self.commands[0].template.serialize(serializer);
        }

        // Serialize as named map. Generate keys for unnamed commands (can happen
        // when merging unnamed global hooks with named project hooks, though
        // merged configs are only used for execution, never serialized in production).
        let mut map = serializer.serialize_map(Some(self.commands.len()))?;
        let mut unnamed_counter = 0u32;
        for cmd in &self.commands {
            let key = match &cmd.name {
                Some(name) => name.clone(),
                None => {
                    unnamed_counter += 1;
                    format!("_{unnamed_counter}")
                }
            };
            if cmd.wait {
                // Emit extended form: { run = "...", wait = true }
                #[derive(Serialize)]
                struct Extended<'a> {
                    run: &'a str,
                    wait: bool,
                }
                map.serialize_entry(
                    &key,
                    &Extended {
                        run: &cmd.template,
                        wait: true,
                    },
                )?;
            } else {
                map.serialize_entry(&key, &cmd.template)?;
            }
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use super::*;

    // ============================================================================
    // Command Tests
    // ============================================================================

    #[test]
    fn test_deserialize_single_string() {
        let toml_str = r#"command = "npm install""#;

        #[derive(Deserialize)]
        struct Wrapper {
            command: CommandConfig,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let commands = wrapper.command.commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, None);
        assert_eq!(commands[0].template, "npm install");
    }

    #[test]
    fn test_deserialize_named_table() {
        let toml_str = r#"
[command]
build = "cargo build"
test = "cargo test"
"#;

        #[derive(Deserialize)]
        struct Wrapper {
            command: CommandConfig,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let commands = wrapper.command.commands();
        assert_eq!(commands.len(), 2);
        assert!(commands.iter().any(|c| c.name == Some("build".to_string())));
        assert!(commands.iter().any(|c| c.name == Some("test".to_string())));
    }

    #[test]
    fn test_deserialize_preserves_order() {
        // Order should match TOML insertion order
        let toml_str = r#"
[command]
first = "echo 1"
second = "echo 2"
third = "echo 3"
"#;

        #[derive(Deserialize)]
        struct Wrapper {
            command: CommandConfig,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let commands = wrapper.command.commands();
        assert_eq!(commands.len(), 3);
        // IndexMap preserves insertion order
        assert_eq!(commands[0].name, Some("first".to_string()));
        assert_eq!(commands[1].name, Some("second".to_string()));
        assert_eq!(commands[2].name, Some("third".to_string()));
    }

    #[test]
    fn test_deserialize_rejects_colons_in_name() {
        // Hook names cannot contain colons (would break log spec parsing)
        let toml_str = r#"
[command]
"my:server" = "npm start"
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[serde(rename = "command")]
            _command: CommandConfig,
        }

        let result: Result<Wrapper, _> = toml::from_str(toml_str);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cannot contain colons"),
            "Expected colon rejection error: {}",
            err
        );
    }

    // ============================================================================
    // CommandConfig Serialization Tests
    // ============================================================================

    #[test]
    fn test_serialize_single_unnamed() {
        // A single unnamed command should serialize as a string (when wrapped in a struct)
        #[derive(Serialize)]
        struct Wrapper {
            cmd: CommandConfig,
        }

        let wrapper = Wrapper {
            cmd: CommandConfig {
                commands: vec![Command::new(None, "npm install".to_string())],
            },
        };

        assert_snapshot!(toml::to_string(&wrapper).unwrap(), @r#"cmd = "npm install""#);
    }

    #[test]
    fn test_serialize_named_commands() {
        // Multiple named commands should serialize as a table
        #[derive(Serialize)]
        struct Wrapper {
            cmd: CommandConfig,
        }

        let wrapper = Wrapper {
            cmd: CommandConfig {
                commands: vec![
                    Command::new(Some("build".to_string()), "cargo build".to_string()),
                    Command::new(Some("test".to_string()), "cargo test".to_string()),
                ],
            },
        };

        assert_snapshot!(toml::to_string(&wrapper).unwrap(), @r#"
        [cmd]
        build = "cargo build"
        test = "cargo test"
        "#);
    }

    #[test]
    fn test_serialize_deserialize_roundtrip_single() {
        let config = CommandConfig {
            commands: vec![Command::new(None, "echo hello".to_string())],
        };

        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            cmd: CommandConfig,
        }

        let wrapper = Wrapper { cmd: config };
        let serialized = toml::to_string(&wrapper).unwrap();
        let deserialized: Wrapper = toml::from_str(&serialized).unwrap();

        assert_eq!(deserialized.cmd.commands().len(), 1);
        assert_eq!(deserialized.cmd.commands()[0].template, "echo hello");
    }

    #[test]
    fn test_serialize_deserialize_roundtrip_named() {
        let config = CommandConfig {
            commands: vec![
                Command::new(Some("a".to_string()), "echo a".to_string()),
                Command::new(Some("b".to_string()), "echo b".to_string()),
            ],
        };

        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            cmd: CommandConfig,
        }

        let wrapper = Wrapper { cmd: config };
        let serialized = toml::to_string(&wrapper).unwrap();
        let deserialized: Wrapper = toml::from_str(&serialized).unwrap();

        assert_eq!(deserialized.cmd.commands().len(), 2);
    }

    // ============================================================================
    // CommandConfig Methods Tests
    // ============================================================================

    #[test]
    fn test_commands_returns_slice() {
        let config = CommandConfig {
            commands: vec![
                Command::new(None, "cmd1".to_string()),
                Command::new(None, "cmd2".to_string()),
            ],
        };

        let cmds = config.commands();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].template, "cmd1");
        assert_eq!(cmds[1].template, "cmd2");
    }

    #[test]
    fn test_command_config_equality() {
        let config1 = CommandConfig {
            commands: vec![Command::new(None, "test".to_string())],
        };
        let config2 = CommandConfig {
            commands: vec![Command::new(None, "test".to_string())],
        };
        assert_eq!(config1, config2);
    }

    #[test]
    fn test_command_config_merge_append() {
        let base = CommandConfig {
            commands: vec![
                Command::new(None, "echo base1".to_string()),
                Command::new(Some("named".to_string()), "echo base2".to_string()),
            ],
        };
        let overlay = CommandConfig {
            commands: vec![Command::new(None, "echo overlay".to_string())],
        };

        let merged = base.merge_append(&overlay);
        assert_eq!(merged.commands.len(), 3);
        assert_eq!(merged.commands[0].template, "echo base1");
        assert_eq!(merged.commands[1].template, "echo base2");
        assert_eq!(merged.commands[2].template, "echo overlay");
    }

    #[test]
    fn test_command_config_merge_append_empty_base() {
        let base = CommandConfig { commands: vec![] };
        let overlay = CommandConfig {
            commands: vec![Command::new(None, "echo overlay".to_string())],
        };

        let merged = base.merge_append(&overlay);
        assert_eq!(merged.commands.len(), 1);
        assert_eq!(merged.commands[0].template, "echo overlay");
    }

    #[test]
    fn test_command_config_merge_append_empty_overlay() {
        let base = CommandConfig {
            commands: vec![Command::new(None, "echo base".to_string())],
        };
        let overlay = CommandConfig { commands: vec![] };

        let merged = base.merge_append(&overlay);
        assert_eq!(merged.commands.len(), 1);
        assert_eq!(merged.commands[0].template, "echo base");
    }

    // ============================================================================
    // Serialization Edge Cases (merged configs)
    // ============================================================================
    //
    // Note: Merged configs (from merge_append) are only used for execution,
    // never serialized in production. These tests verify serialization doesn't
    // panic, but round-trip fidelity isn't required.

    /// Serializing a merged config with mixed named/unnamed commands doesn't panic.
    #[test]
    fn test_serialize_mixed_named_unnamed_succeeds() {
        #[derive(Serialize)]
        struct Wrapper {
            cmd: CommandConfig,
        }

        // Simulate merge of unnamed global + named project hooks
        let global = CommandConfig {
            commands: vec![Command::new(None, "npm install".to_string())],
        };
        let per_project = CommandConfig {
            commands: vec![Command::new(
                Some("setup".to_string()),
                "echo setup".to_string(),
            )],
        };

        let merged = global.merge_append(&per_project);
        assert_eq!(merged.commands.len(), 2);

        // Should not panic - generates "_1" for unnamed command
        let wrapper = Wrapper { cmd: merged };
        assert_snapshot!(toml::to_string(&wrapper).unwrap(), @r#"
        [cmd]
        _1 = "npm install"
        setup = "echo setup"
        "#);
    }

    // ============================================================================
    // Wait Flag Tests
    // ============================================================================

    #[test]
    fn test_deserialize_extended_with_wait() {
        let toml_str = r#"
[command]
install = { run = "npm install", wait = true }
build = "npm run build"
"#;

        #[derive(Deserialize)]
        struct Wrapper {
            command: CommandConfig,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let commands = wrapper.command.commands();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].name.as_deref(), Some("install"));
        assert_eq!(commands[0].template, "npm install");
        assert!(commands[0].wait);
        assert_eq!(commands[1].name.as_deref(), Some("build"));
        assert_eq!(commands[1].template, "npm run build");
        assert!(!commands[1].wait);
    }

    #[test]
    fn test_deserialize_extended_wait_defaults_false() {
        let toml_str = r#"
[command]
install = { run = "npm install" }
"#;

        #[derive(Deserialize)]
        struct Wrapper {
            command: CommandConfig,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let commands = wrapper.command.commands();
        assert_eq!(commands[0].template, "npm install");
        assert!(!commands[0].wait);
    }

    #[test]
    fn test_deserialize_single_string_no_wait() {
        let toml_str = r#"command = "npm install""#;

        #[derive(Deserialize)]
        struct Wrapper {
            command: CommandConfig,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        assert!(!wrapper.command.commands()[0].wait);
    }

    #[test]
    fn test_deserialize_command_named_run() {
        // A command named "run" must not be confused with the extended form
        let toml_str = r#"
[command]
run = "npm install"
"#;

        #[derive(Deserialize)]
        struct Wrapper {
            command: CommandConfig,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let commands = wrapper.command.commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name.as_deref(), Some("run"));
        assert_eq!(commands[0].template, "npm install");
        assert!(!commands[0].wait);
    }

    #[test]
    fn test_deserialize_single_extended() {
        // `post-start = { run = "...", wait = true }` — unnamed command with wait
        let toml_str = r#"command = { run = "npm install", wait = true }"#;

        #[derive(Deserialize)]
        struct Wrapper {
            command: CommandConfig,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let commands = wrapper.command.commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, None);
        assert_eq!(commands[0].template, "npm install");
        assert!(commands[0].wait);
    }

    #[test]
    fn test_deserialize_single_extended_without_wait() {
        // `{ run = "..." }` without wait parses as Named (command named "run")
        let toml_str = r#"command = { run = "npm install" }"#;

        #[derive(Deserialize)]
        struct Wrapper {
            command: CommandConfig,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let commands = wrapper.command.commands();
        assert_eq!(commands.len(), 1);
        // Without `wait = true`, this matches Named (command named "run")
        assert_eq!(commands[0].name.as_deref(), Some("run"));
        assert_eq!(commands[0].template, "npm install");
    }

    #[test]
    fn test_serialize_wait_command() {
        #[derive(Serialize)]
        struct Wrapper {
            cmd: CommandConfig,
        }

        let mut install = Command::new(Some("install".to_string()), "npm install".to_string());
        install.wait = true;

        let wrapper = Wrapper {
            cmd: CommandConfig {
                commands: vec![
                    install,
                    Command::new(Some("build".to_string()), "npm run build".to_string()),
                ],
            },
        };

        // TOML puts simple key-value pairs before sub-table headers
        assert_snapshot!(toml::to_string(&wrapper).unwrap(), @r#"
        [cmd]
        build = "npm run build"

        [cmd.install]
        run = "npm install"
        wait = true
        "#);
    }

    #[test]
    fn test_serialize_roundtrip_wait() {
        let toml_str = r#"
[cmd]
install = { run = "npm install", wait = true }
build = "npm run build"
"#;

        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            cmd: CommandConfig,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let commands = wrapper.cmd.commands();
        assert!(commands[0].wait);
        assert!(!commands[1].wait);

        // Roundtrip preserves wait flag and templates (order may change due to
        // TOML sub-table serialization rules)
        let reserialized = toml::to_string(&wrapper).unwrap();
        let roundtrip: Wrapper = toml::from_str(&reserialized).unwrap();
        let cmds = roundtrip.cmd.commands();
        let wait_cmd = cmds.iter().find(|c| c.wait).expect("should have wait cmd");
        assert_eq!(wait_cmd.template, "npm install");
        let bg_cmd = cmds.iter().find(|c| !c.wait).expect("should have bg cmd");
        assert_eq!(bg_cmd.template, "npm run build");
    }

    #[test]
    fn test_merge_preserves_wait() {
        let mut install = Command::new(Some("install".to_string()), "npm install".to_string());
        install.wait = true;

        let base = CommandConfig {
            commands: vec![install],
        };
        let overlay = CommandConfig {
            commands: vec![Command::new(
                Some("build".to_string()),
                "npm run build".to_string(),
            )],
        };

        let merged = base.merge_append(&overlay);
        assert_eq!(merged.commands.len(), 2);
        assert!(merged.commands[0].wait);
        assert!(!merged.commands[1].wait);
    }
}
