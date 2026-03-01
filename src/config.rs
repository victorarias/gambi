use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tempfile::Builder;
use url::Url;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);
const KEYCHAIN_ACCOUNT_NAME: &str = "oauth-tokens";
pub const MAX_SERVER_INSTRUCTION_LENGTH: usize = 8_192;

#[derive(Debug, Clone)]
pub struct ConfigStore {
    paths: Paths,
    runtime_profile: RuntimeProfile,
    token_storage: TokenStorage,
}

#[derive(Debug, Clone)]
struct Paths {
    config_dir: PathBuf,
    config_file: PathBuf,
    tokens_file: PathBuf,
    lock_file: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeProfile {
    Local,
    Dev,
    Production,
}

impl RuntimeProfile {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "dev" | "development" => Ok(Self::Dev),
            "prod" | "production" => Ok(Self::Production),
            other => bail!("invalid GAMBI_PROFILE value '{other}', expected local|dev|production"),
        }
    }

    fn from_env() -> Result<Self> {
        match std::env::var("GAMBI_PROFILE") {
            Ok(value) => Self::parse(&value),
            Err(std::env::VarError::NotPresent) => Ok(Self::Local),
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!("GAMBI_PROFILE must be valid UTF-8")
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Dev => "dev",
            Self::Production => "production",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenStorePreference {
    Auto,
    File,
    Keychain,
}

impl TokenStorePreference {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "file" | "json" => Ok(Self::File),
            "keychain" => Ok(Self::Keychain),
            other => {
                bail!("invalid GAMBI_TOKEN_STORE value '{other}', expected auto|file|keychain")
            }
        }
    }

    fn from_env() -> Result<Self> {
        match std::env::var("GAMBI_TOKEN_STORE") {
            Ok(value) => Self::parse(&value),
            Err(std::env::VarError::NotPresent) => Ok(Self::Auto),
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!("GAMBI_TOKEN_STORE must be valid UTF-8")
            }
        }
    }

    fn resolve(self, profile: RuntimeProfile) -> TokenStorage {
        match self {
            Self::Auto => {
                if profile == RuntimeProfile::Production {
                    TokenStorage::Keychain
                } else {
                    TokenStorage::File
                }
            }
            Self::File => TokenStorage::File,
            Self::Keychain => TokenStorage::Keychain,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenStorage {
    File,
    Keychain,
}

impl TokenStorage {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Keychain => "keychain",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TransportMode {
    #[default]
    Auto,
    Sse,
    StreamableHttp,
}

impl TransportMode {
    fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExposureMode {
    #[default]
    Passthrough,
    Compact,
    NamesOnly,
    ServerOnly,
}

impl ExposureMode {
    fn is_passthrough(&self) -> bool {
        *self == Self::Passthrough
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passthrough => "passthrough",
            Self::Compact => "compact",
            Self::NamesOnly => "names-only",
            Self::ServerOnly => "server-only",
        }
    }
}

fn default_server_enabled() -> bool {
    true
}

fn is_server_enabled(value: &bool) -> bool {
    *value
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ToolPolicyMode {
    #[default]
    Heuristic,
    AllSafe,
    AllEscalated,
    Custom,
}

impl ToolPolicyMode {
    fn is_heuristic(&self) -> bool {
        *self == Self::Heuristic
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Heuristic => "heuristic",
            Self::AllSafe => "all-safe",
            Self::AllEscalated => "all-escalated",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ToolActivationMode {
    #[default]
    All,
    None,
}

impl ToolActivationMode {
    fn is_all(&self) -> bool {
        *self == Self::All
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::None => "none",
        }
    }

    pub fn default_enabled(self) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ToolPolicyLevel {
    Safe,
    Escalated,
}

impl ToolPolicyLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Escalated => "escalated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPolicySource {
    System,
    Heuristic,
    Override,
    CatalogAllSafe,
    CatalogAllEscalated,
}

impl ToolPolicySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Heuristic => "heuristic",
            Self::Override => "override",
            Self::CatalogAllSafe => "catalog-all-safe",
            Self::CatalogAllEscalated => "catalog-all-escalated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolActivationSource {
    CatalogAll,
    CatalogNone,
    Override,
}

impl ToolActivationSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CatalogAll => "catalog-all",
            Self::CatalogNone => "catalog-none",
            Self::Override => "override",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolActivationDecision {
    pub enabled: bool,
    pub source: ToolActivationSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPolicyDecision {
    pub level: ToolPolicyLevel,
    pub source: ToolPolicySource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerInstructionSource {
    Override,
    Upstream,
    None,
}

impl ServerInstructionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Upstream => "upstream",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInstructionDecision {
    pub instruction: Option<String>,
    pub source: ServerInstructionSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerConfig {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthConfig>,
    #[serde(default, skip_serializing_if = "TransportMode::is_auto")]
    pub transport: TransportMode,
    #[serde(default, skip_serializing_if = "ExposureMode::is_passthrough")]
    pub exposure_mode: ExposureMode,
    #[serde(
        default = "default_server_enabled",
        skip_serializing_if = "is_server_enabled"
    )]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_url: Option<String>,
    pub authorize_url: String,
    pub token_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub server_tool_activation_modes: BTreeMap<String, ToolActivationMode>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_activation_overrides: BTreeMap<String, BTreeMap<String, bool>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub server_tool_policy_modes: BTreeMap<String, ToolPolicyMode>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_description_overrides: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub server_instruction_overrides: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_policy_overrides: BTreeMap<String, BTreeMap<String, ToolPolicyLevel>>,
}

impl AppConfig {
    pub fn add_server(&mut self, server: ServerConfig) -> Result<()> {
        let name = validate_server_name(&server.name)?;
        let url = validate_server_url(&server.url)?;
        if let Some(oauth) = &server.oauth {
            validate_oauth_config(oauth)?;
        }

        if self.servers.iter().any(|s| s.name == name) {
            bail!("server '{name}' already exists");
        }

        self.servers.push(ServerConfig {
            name,
            url,
            oauth: server.oauth,
            transport: server.transport,
            exposure_mode: server.exposure_mode,
            enabled: server.enabled,
        });
        self.servers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    }

    pub fn remove_server(&mut self, server_name: &str) -> bool {
        let before = self.servers.len();
        self.servers.retain(|s| s.name != server_name);
        let removed = self.servers.len() != before;
        if removed {
            self.server_tool_activation_modes.remove(server_name);
            self.tool_activation_overrides.remove(server_name);
            self.server_tool_policy_modes.remove(server_name);
            self.tool_description_overrides.remove(server_name);
            self.server_instruction_overrides.remove(server_name);
            self.tool_policy_overrides.remove(server_name);
        }
        removed
    }

    pub fn set_server_exposure_mode(
        &mut self,
        server_name: &str,
        exposure_mode: ExposureMode,
    ) -> Result<()> {
        let Some(server) = self
            .servers
            .iter_mut()
            .find(|server| server.name == server_name)
        else {
            bail!("unknown server '{server_name}'");
        };
        server.exposure_mode = exposure_mode;
        Ok(())
    }

    pub fn set_server_enabled(&mut self, server_name: &str, enabled: bool) -> Result<()> {
        let Some(server) = self
            .servers
            .iter_mut()
            .find(|server| server.name == server_name)
        else {
            bail!("unknown server '{server_name}'");
        };
        server.enabled = enabled;
        Ok(())
    }

    pub fn enabled_servers(&self) -> Vec<ServerConfig> {
        self.servers
            .iter()
            .filter(|server| server.enabled)
            .cloned()
            .collect()
    }

    pub fn set_server_tool_activation_mode(
        &mut self,
        server_name: &str,
        activation_mode: ToolActivationMode,
    ) -> Result<()> {
        if !self.servers.iter().any(|server| server.name == server_name) {
            bail!("unknown server '{server_name}'");
        }

        if activation_mode.is_all() {
            self.server_tool_activation_modes.remove(server_name);
        } else {
            self.server_tool_activation_modes
                .insert(server_name.to_string(), activation_mode);
        }
        self.prune_tool_activation_overrides_for_server(server_name);
        Ok(())
    }

    pub fn server_tool_activation_mode_for(&self, server_name: &str) -> ToolActivationMode {
        self.server_tool_activation_modes
            .get(server_name)
            .copied()
            .unwrap_or_default()
    }

    pub fn set_tool_activation_override(
        &mut self,
        server_name: &str,
        upstream_tool_name: &str,
        enabled: bool,
    ) -> Result<()> {
        if !self.servers.iter().any(|server| server.name == server_name) {
            bail!("unknown server '{server_name}'");
        }

        let tool_name = upstream_tool_name.trim();
        if tool_name.is_empty() {
            bail!("tool name cannot be empty");
        }

        let default_enabled = self
            .server_tool_activation_mode_for(server_name)
            .default_enabled();
        if enabled == default_enabled {
            if let Some(overrides) = self.tool_activation_overrides.get_mut(server_name) {
                overrides.remove(tool_name);
                if overrides.is_empty() {
                    self.tool_activation_overrides.remove(server_name);
                }
            }
            return Ok(());
        }

        self.tool_activation_overrides
            .entry(server_name.to_string())
            .or_default()
            .insert(tool_name.to_string(), enabled);
        Ok(())
    }

    pub fn tool_activation_override_for(
        &self,
        server_name: &str,
        upstream_tool_name: &str,
    ) -> Option<bool> {
        self.tool_activation_overrides
            .get(server_name)
            .and_then(|overrides| overrides.get(upstream_tool_name))
            .copied()
    }

    pub fn evaluate_tool_activation(
        &self,
        server_name: &str,
        upstream_tool_name: &str,
    ) -> ToolActivationDecision {
        if let Some(enabled) = self.tool_activation_override_for(server_name, upstream_tool_name) {
            return ToolActivationDecision {
                enabled,
                source: ToolActivationSource::Override,
            };
        }

        let mode = self.server_tool_activation_mode_for(server_name);
        match mode {
            ToolActivationMode::All => ToolActivationDecision {
                enabled: true,
                source: ToolActivationSource::CatalogAll,
            },
            ToolActivationMode::None => ToolActivationDecision {
                enabled: false,
                source: ToolActivationSource::CatalogNone,
            },
        }
    }

    pub fn is_tool_enabled(&self, server_name: &str, upstream_tool_name: &str) -> bool {
        self.evaluate_tool_activation(server_name, upstream_tool_name)
            .enabled
    }

    pub fn set_tool_description_override(
        &mut self,
        server_name: &str,
        upstream_tool_name: &str,
        description: &str,
    ) -> Result<()> {
        if !self.servers.iter().any(|server| server.name == server_name) {
            bail!("unknown server '{server_name}'");
        }
        let tool_name = upstream_tool_name.trim();
        if tool_name.is_empty() {
            bail!("tool name cannot be empty");
        }
        let description = description.trim();
        if description.is_empty() {
            bail!("description cannot be empty");
        }

        self.tool_description_overrides
            .entry(server_name.to_string())
            .or_default()
            .insert(tool_name.to_string(), description.to_string());
        Ok(())
    }

    pub fn set_server_instruction_override(
        &mut self,
        server_name: &str,
        instruction: &str,
    ) -> Result<()> {
        if !self.servers.iter().any(|server| server.name == server_name) {
            bail!("unknown server '{server_name}'");
        }
        let instruction = instruction.trim();
        if instruction.is_empty() {
            bail!("instruction cannot be empty");
        }
        if instruction.chars().count() > MAX_SERVER_INSTRUCTION_LENGTH {
            bail!(
                "instruction exceeds max length of {} characters",
                MAX_SERVER_INSTRUCTION_LENGTH
            );
        }

        self.server_instruction_overrides
            .insert(server_name.to_string(), instruction.to_string());
        Ok(())
    }

    pub fn remove_server_instruction_override(&mut self, server_name: &str) -> bool {
        self.server_instruction_overrides
            .remove(server_name)
            .is_some()
    }

    pub fn server_instruction_override_for(&self, server_name: &str) -> Option<&str> {
        self.server_instruction_overrides
            .get(server_name)
            .map(String::as_str)
    }

    pub fn resolve_server_instruction(
        &self,
        upstream_instructions: &BTreeMap<String, String>,
        server_name: &str,
    ) -> ServerInstructionDecision {
        if let Some(override_instruction) = self.server_instruction_override_for(server_name) {
            return ServerInstructionDecision {
                instruction: Some(override_instruction.to_string()),
                source: ServerInstructionSource::Override,
            };
        }
        if let Some(upstream_instruction) = upstream_instructions.get(server_name) {
            return ServerInstructionDecision {
                instruction: Some(upstream_instruction.to_string()),
                source: ServerInstructionSource::Upstream,
            };
        }
        ServerInstructionDecision {
            instruction: None,
            source: ServerInstructionSource::None,
        }
    }

    pub fn set_server_tool_policy_mode(
        &mut self,
        server_name: &str,
        policy_mode: ToolPolicyMode,
    ) -> Result<()> {
        if !self.servers.iter().any(|server| server.name == server_name) {
            bail!("unknown server '{server_name}'");
        }
        if policy_mode.is_heuristic() {
            self.server_tool_policy_modes.remove(server_name);
        } else {
            self.server_tool_policy_modes
                .insert(server_name.to_string(), policy_mode);
        }
        Ok(())
    }

    pub fn server_tool_policy_mode_for(&self, server_name: &str) -> ToolPolicyMode {
        self.server_tool_policy_modes
            .get(server_name)
            .copied()
            .unwrap_or_default()
    }

    pub fn set_tool_policy_override(
        &mut self,
        server_name: &str,
        upstream_tool_name: &str,
        level: ToolPolicyLevel,
    ) -> Result<()> {
        if !self.servers.iter().any(|server| server.name == server_name) {
            bail!("unknown server '{server_name}'");
        }
        let tool_name = upstream_tool_name.trim();
        if tool_name.is_empty() {
            bail!("tool name cannot be empty");
        }

        self.tool_policy_overrides
            .entry(server_name.to_string())
            .or_default()
            .insert(tool_name.to_string(), level);
        Ok(())
    }

    pub fn remove_tool_policy_override(
        &mut self,
        server_name: &str,
        upstream_tool_name: &str,
    ) -> bool {
        let tool_name = upstream_tool_name.trim();
        let Some(overrides) = self.tool_policy_overrides.get_mut(server_name) else {
            return false;
        };
        let removed = overrides.remove(tool_name).is_some();
        if overrides.is_empty() {
            self.tool_policy_overrides.remove(server_name);
        }
        removed
    }

    pub fn tool_policy_override_for(
        &self,
        server_name: &str,
        upstream_tool_name: &str,
    ) -> Option<ToolPolicyLevel> {
        self.tool_policy_overrides
            .get(server_name)
            .and_then(|overrides| overrides.get(upstream_tool_name))
            .copied()
    }

    pub fn evaluate_tool_policy(
        &self,
        server_name: &str,
        upstream_tool_name: &str,
        description: Option<&str>,
    ) -> ToolPolicyDecision {
        let mode = self.server_tool_policy_mode_for(server_name);
        match mode {
            ToolPolicyMode::AllSafe => ToolPolicyDecision {
                level: ToolPolicyLevel::Safe,
                source: ToolPolicySource::CatalogAllSafe,
            },
            ToolPolicyMode::AllEscalated => ToolPolicyDecision {
                level: ToolPolicyLevel::Escalated,
                source: ToolPolicySource::CatalogAllEscalated,
            },
            ToolPolicyMode::Custom | ToolPolicyMode::Heuristic => {
                if let Some(level) = self.tool_policy_override_for(server_name, upstream_tool_name)
                {
                    return ToolPolicyDecision {
                        level,
                        source: ToolPolicySource::Override,
                    };
                }
                let safe = tool_matches_safe_heuristic(upstream_tool_name, description);
                ToolPolicyDecision {
                    level: if safe {
                        ToolPolicyLevel::Safe
                    } else {
                        ToolPolicyLevel::Escalated
                    },
                    source: ToolPolicySource::Heuristic,
                }
            }
        }
    }

    pub fn remove_tool_description_override(
        &mut self,
        server_name: &str,
        upstream_tool_name: &str,
    ) -> bool {
        let tool_name = upstream_tool_name.trim();
        let Some(overrides) = self.tool_description_overrides.get_mut(server_name) else {
            return false;
        };
        let removed = overrides.remove(tool_name).is_some();
        if overrides.is_empty() {
            self.tool_description_overrides.remove(server_name);
        }
        removed
    }

    pub fn tool_description_override_for(
        &self,
        server_name: &str,
        upstream_tool_name: &str,
    ) -> Option<&str> {
        self.tool_description_overrides
            .get(server_name)
            .and_then(|overrides| overrides.get(upstream_tool_name))
            .map(String::as_str)
    }

    fn prune_tool_activation_overrides_for_server(&mut self, server_name: &str) {
        let default_enabled = self
            .server_tool_activation_mode_for(server_name)
            .default_enabled();
        let Some(overrides) = self.tool_activation_overrides.get_mut(server_name) else {
            return;
        };
        overrides.retain(|_, enabled| *enabled != default_enabled);
        if overrides.is_empty() {
            self.tool_activation_overrides.remove(server_name);
        }
    }

    fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for server in &self.servers {
            let valid_name = validate_server_name(&server.name)?;
            if valid_name != server.name {
                bail!(
                    "server '{}' has invalid formatting; remove leading/trailing whitespace",
                    server.name
                );
            }
            let valid_url = validate_server_url(&server.url)?;
            if valid_url != server.url {
                bail!(
                    "server '{}' url has invalid formatting; remove leading/trailing whitespace",
                    server.name
                );
            }
            if let Some(oauth) = &server.oauth {
                validate_oauth_config(oauth).with_context(|| {
                    format!("invalid oauth config for server '{}'", server.name)
                })?;
            }
            if !seen.insert(server.name.clone()) {
                bail!("duplicate server '{}' in config", server.name);
            }
        }
        for (server_name, overrides) in &self.tool_description_overrides {
            if !seen.contains(server_name) {
                bail!("tool description overrides reference unknown server '{server_name}'");
            }
            for (upstream_tool_name, description) in overrides {
                if upstream_tool_name.trim().is_empty() {
                    bail!(
                        "tool description override has empty tool name for server '{server_name}'"
                    );
                }
                if description.trim().is_empty() {
                    bail!(
                        "tool description override for '{server_name}:{upstream_tool_name}' cannot be empty"
                    );
                }
            }
        }
        for (server_name, instruction) in &self.server_instruction_overrides {
            if !seen.contains(server_name) {
                bail!("server instruction overrides reference unknown server '{server_name}'");
            }
            let trimmed = instruction.trim();
            if trimmed.is_empty() {
                bail!("server instruction override for '{server_name}' cannot be empty");
            }
            if trimmed.chars().count() > MAX_SERVER_INSTRUCTION_LENGTH {
                bail!(
                    "server instruction override for '{server_name}' exceeds max length of {} characters",
                    MAX_SERVER_INSTRUCTION_LENGTH
                );
            }
        }
        for (server_name, mode) in &self.server_tool_activation_modes {
            if !seen.contains(server_name) {
                bail!("tool activation mode references unknown server '{server_name}'");
            }
            if mode.is_all() {
                bail!("tool activation mode for '{server_name}' should omit all default");
            }
        }
        for (server_name, overrides) in &self.tool_activation_overrides {
            if !seen.contains(server_name) {
                bail!("tool activation overrides reference unknown server '{server_name}'");
            }
            for upstream_tool_name in overrides.keys() {
                if upstream_tool_name.trim().is_empty() {
                    bail!(
                        "tool activation override has empty tool name for server '{server_name}'"
                    );
                }
            }
        }
        for (server_name, mode) in &self.server_tool_policy_modes {
            if !seen.contains(server_name) {
                bail!("tool policy mode references unknown server '{server_name}'");
            }
            if mode.is_heuristic() {
                bail!("tool policy mode for '{server_name}' should omit heuristic default");
            }
        }
        for (server_name, overrides) in &self.tool_policy_overrides {
            if !seen.contains(server_name) {
                bail!("tool policy overrides reference unknown server '{server_name}'");
            }
            for upstream_tool_name in overrides.keys() {
                if upstream_tool_name.trim().is_empty() {
                    bail!("tool policy override has empty tool name for server '{server_name}'");
                }
            }
        }
        Ok(())
    }
}

pub fn tool_matches_safe_heuristic(tool_name: &str, description: Option<&str>) -> bool {
    let normalized_tool = tool_name
        .rsplit(':')
        .next()
        .unwrap_or(tool_name)
        .trim()
        .to_ascii_lowercase();
    const SAFE_PREFIXES: [&str; 5] = ["get", "list", "search", "lookup", "fetch"];
    if SAFE_PREFIXES
        .iter()
        .any(|prefix| normalized_tool.starts_with(prefix))
    {
        return true;
    }

    description
        .map(str::trim_start)
        .map(|value| value.to_ascii_lowercase().starts_with("get"))
        .unwrap_or(false)
}

impl ConfigStore {
    pub fn new_default() -> Result<Self> {
        let config_dir = match std::env::var_os("GAMBI_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => {
                let config_root = dirs::config_dir()
                    .ok_or_else(|| anyhow!("unable to resolve config directory"))?;
                config_root.join("gambi")
            }
        };
        let runtime_profile = RuntimeProfile::from_env()?;
        let token_storage = TokenStorePreference::from_env()?.resolve(runtime_profile);
        Ok(Self::with_base_dir_and_storage(
            config_dir,
            runtime_profile,
            token_storage,
        ))
    }

    #[cfg(test)]
    pub fn with_base_dir(config_dir: PathBuf) -> Self {
        Self::with_base_dir_and_storage(config_dir, RuntimeProfile::Local, TokenStorage::File)
    }

    fn with_base_dir_and_storage(
        config_dir: PathBuf,
        runtime_profile: RuntimeProfile,
        token_storage: TokenStorage,
    ) -> Self {
        let config_file = config_dir.join("config.json");
        let tokens_file = config_dir.join("tokens.json");
        let lock_file = config_dir.join(".lock");
        Self {
            paths: Paths {
                config_dir,
                config_file,
                tokens_file,
                lock_file,
            },
            runtime_profile,
            token_storage,
        }
    }

    pub fn runtime_profile(&self) -> RuntimeProfile {
        self.runtime_profile
    }

    pub fn token_store_mode(&self) -> &'static str {
        self.token_storage.as_str()
    }

    pub fn daemon_state_file(&self) -> PathBuf {
        self.paths.config_dir.join("daemon.state")
    }

    pub fn load(&self) -> Result<AppConfig> {
        let _guard = self.acquire_lock(LOCK_WAIT_TIMEOUT)?;
        self.load_unlocked()
    }

    pub async fn load_async(&self) -> Result<AppConfig> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.load())
            .await
            .context("config load task failed")?
    }

    pub async fn load_tokens_async<T>(&self) -> Result<T>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.load_tokens::<T>())
            .await
            .context("token load task failed")?
    }

    pub fn update<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut AppConfig) -> Result<T>,
    {
        let _guard = self.acquire_lock(LOCK_WAIT_TIMEOUT)?;
        let mut cfg = self.load_unlocked()?;
        let result = f(&mut cfg)?;
        self.save_unlocked(&cfg)?;
        Ok(result)
    }

    pub async fn update_async<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut AppConfig) -> Result<T> + Send + 'static,
    {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.update(f))
            .await
            .context("config update task failed")?
    }

    pub fn replace(&self, cfg: AppConfig) -> Result<()> {
        cfg.validate().context("invalid config for replace")?;
        let _guard = self.acquire_lock(LOCK_WAIT_TIMEOUT)?;
        self.save_unlocked(&cfg)
    }

    pub async fn replace_async(&self, cfg: AppConfig) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.replace(cfg))
            .await
            .context("config replace task failed")?
    }

    pub fn load_tokens<T>(&self) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let _guard = self.acquire_lock(LOCK_WAIT_TIMEOUT)?;
        self.load_tokens_unlocked()
    }

    pub fn update_tokens<T, R, F>(&self, f: F) -> Result<R>
    where
        T: DeserializeOwned + Serialize,
        F: FnOnce(&mut T) -> Result<R>,
    {
        let _guard = self.acquire_lock(LOCK_WAIT_TIMEOUT)?;
        let mut value: T = self.load_tokens_unlocked()?;
        let result = f(&mut value)?;
        self.save_tokens_unlocked(&value)?;
        Ok(result)
    }

    pub async fn update_tokens_async<T, R, F>(&self, f: F) -> Result<R>
    where
        T: DeserializeOwned + Serialize + Send + 'static,
        R: Send + 'static,
        F: FnOnce(&mut T) -> Result<R> + Send + 'static,
    {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.update_tokens::<T, R, F>(f))
            .await
            .context("token update task failed")?
    }

    pub fn weak_permission_paths(&self) -> Result<Vec<PathBuf>> {
        self.ensure_layout()?;
        let mut weak = Vec::new();

        if is_directory_weaker_than_owner_only(&self.paths.config_dir)? {
            weak.push(self.paths.config_dir.clone());
        }
        if is_weaker_than_owner_only(&self.paths.config_file)? {
            weak.push(self.paths.config_file.clone());
        }
        if self.token_storage == TokenStorage::File
            && is_weaker_than_owner_only(&self.paths.tokens_file)?
        {
            weak.push(self.paths.tokens_file.clone());
        }

        Ok(weak)
    }

    fn ensure_layout(&self) -> Result<()> {
        fs::create_dir_all(&self.paths.config_dir)
            .with_context(|| format!("failed to create {}", self.paths.config_dir.display()))?;

        #[cfg(unix)]
        enforce_owner_only_directory_permissions(&self.paths.config_dir)?;

        if !self.paths.config_file.exists() {
            write_new_secure_file(&self.paths.config_file, b"{\n  \"servers\": []\n}\n")?;
        }

        if self.token_storage == TokenStorage::File && !self.paths.tokens_file.exists() {
            write_new_secure_file(&self.paths.tokens_file, b"{}\n")?;
        }

        if !self.paths.lock_file.exists() {
            create_secure_lock_file(&self.paths.lock_file)?;
        }

        Ok(())
    }

    fn load_unlocked(&self) -> Result<AppConfig> {
        self.ensure_layout()?;

        let mut raw = fs::read_to_string(&self.paths.config_file)
            .with_context(|| format!("failed to read {}", self.paths.config_file.display()))?;

        let cfg: AppConfig = match serde_json::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(err) if raw.trim().is_empty() => {
                // Concurrent first-writer bootstrap can momentarily expose an empty file.
                thread::sleep(Duration::from_millis(10));
                raw = fs::read_to_string(&self.paths.config_file).with_context(|| {
                    format!("failed to read {}", self.paths.config_file.display())
                })?;
                serde_json::from_str(&raw).with_context(|| {
                    format!("invalid JSON in {}", self.paths.config_file.display())
                })?
            }
            Err(_) => serde_json::from_str(&raw)
                .with_context(|| format!("invalid JSON in {}", self.paths.config_file.display()))?,
        };
        cfg.validate().with_context(|| {
            format!(
                "invalid server definitions in {}",
                self.paths.config_file.display()
            )
        })?;

        Ok(cfg)
    }

    fn save_unlocked(&self, cfg: &AppConfig) -> Result<()> {
        self.ensure_layout()?;
        atomic_write_json(&self.paths.config_file, cfg)
    }

    fn load_tokens_unlocked<T>(&self) -> Result<T>
    where
        T: DeserializeOwned,
    {
        match self.token_storage {
            TokenStorage::File => {
                self.ensure_layout()?;
                let raw = fs::read_to_string(&self.paths.tokens_file).with_context(|| {
                    format!("failed to read {}", self.paths.tokens_file.display())
                })?;
                serde_json::from_str(&raw).with_context(|| {
                    format!("invalid JSON in {}", self.paths.tokens_file.display())
                })
            }
            TokenStorage::Keychain => self.load_tokens_from_keychain(),
        }
    }

    fn save_tokens_unlocked<T>(&self, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        match self.token_storage {
            TokenStorage::File => {
                self.ensure_layout()?;
                atomic_write_json(&self.paths.tokens_file, value)
            }
            TokenStorage::Keychain => self.save_tokens_to_keychain(value),
        }
    }

    fn acquire_lock(&self, timeout: Duration) -> Result<LockGuard> {
        self.ensure_layout()?;

        let file = open_lock_file(&self.paths.lock_file).with_context(|| {
            format!(
                "failed to open lock file {}",
                self.paths.lock_file.display()
            )
        })?;

        let start = Instant::now();
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(LockGuard { file }),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if start.elapsed() >= timeout {
                        bail!(
                            "timed out acquiring config lock at {}",
                            self.paths.lock_file.display()
                        );
                    }
                    thread::sleep(LOCK_POLL_INTERVAL);
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "failed to lock config lock file {}",
                            self.paths.lock_file.display()
                        )
                    });
                }
            }
        }
    }

    fn keychain_service_name(&self) -> String {
        format!("gambi-{}", self.runtime_profile.as_str())
    }

    fn load_tokens_from_keychain<T>(&self) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let entry = keyring::Entry::new(&self.keychain_service_name(), KEYCHAIN_ACCOUNT_NAME)
            .context("failed to initialize keychain entry for oauth tokens")?;

        let raw = match entry.get_password() {
            Ok(value) => value,
            Err(keyring::Error::NoEntry) => "{}".to_string(),
            Err(err) => {
                return Err(anyhow!(
                    "failed to read oauth tokens from keychain entry '{}': {err}",
                    self.keychain_service_name()
                ));
            }
        };

        serde_json::from_str(&raw)
            .with_context(|| "invalid token JSON in keychain entry".to_string())
    }

    fn save_tokens_to_keychain<T>(&self, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        let entry = keyring::Entry::new(&self.keychain_service_name(), KEYCHAIN_ACCOUNT_NAME)
            .context("failed to initialize keychain entry for oauth tokens")?;
        let payload =
            serde_json::to_string(value).context("failed to serialize oauth token payload")?;
        entry
            .set_password(&payload)
            .with_context(|| "failed to persist oauth tokens to keychain".to_string())
    }
}

struct LockGuard {
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn validate_server_name(raw_name: &str) -> Result<String> {
    let name = raw_name.trim();
    if name.is_empty() {
        bail!("server name cannot be empty");
    }

    if name.contains(':') {
        bail!("server name cannot contain ':' because it breaks namespacing");
    }

    if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("server name cannot contain whitespace or control characters");
    }

    if name.contains('-') {
        bail!("server name cannot contain '-'");
    }

    Ok(name.to_string())
}

fn validate_server_url(raw_url: &str) -> Result<String> {
    let url = raw_url.trim();
    if url.is_empty() {
        bail!("server url cannot be empty");
    }

    if url.starts_with("stdio://") {
        let parsed =
            Url::parse(url).with_context(|| format!("invalid stdio server url '{url}'"))?;
        if parsed.scheme() != "stdio" {
            bail!("invalid stdio server url '{url}'");
        }
        let host = parsed.host_str().unwrap_or_default();
        let path = parsed.path();
        if host.is_empty() && (path.is_empty() || path == "/") {
            bail!("stdio server url must include a command host or path");
        }
        return Ok(url.to_string());
    }

    let parsed = Url::parse(url).with_context(|| format!("invalid server url '{url}'"))?;

    match parsed.scheme() {
        "http" | "https" => {}
        other => bail!("unsupported server url scheme '{other}', expected http/https/stdio"),
    }

    if parsed.host_str().is_none() {
        bail!("server url must include a host");
    }

    Ok(url.to_string())
}

fn validate_oauth_config(oauth: &OAuthConfig) -> Result<()> {
    let authorize = oauth.authorize_url.trim();
    let token = oauth.token_url.trim();
    let client_id = oauth.client_id.as_deref().map(str::trim);
    let discovery = oauth.discovery_url.as_deref().map(str::trim);

    if authorize.is_empty() {
        bail!("oauth authorize_url cannot be empty");
    }
    if token.is_empty() {
        bail!("oauth token_url cannot be empty");
    }
    if let Some(client_id) = client_id
        && client_id.is_empty()
    {
        bail!("oauth client_id cannot be empty when provided");
    }

    if let Some(discovery_url) = discovery {
        if discovery_url.is_empty() {
            bail!("oauth discovery_url cannot be empty when provided");
        }
        let parsed = Url::parse(discovery_url)
            .with_context(|| format!("invalid oauth discovery_url '{discovery_url}'"))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            bail!("oauth discovery_url must use http or https");
        }
        if parsed.host_str().is_none() {
            bail!("oauth discovery_url must include a host");
        }
    }

    let authorize_url = Url::parse(authorize)
        .with_context(|| format!("invalid oauth authorize_url '{authorize}'"))?;
    let token_url =
        Url::parse(token).with_context(|| format!("invalid oauth token_url '{token}'"))?;

    if authorize_url.scheme() != "http" && authorize_url.scheme() != "https" {
        bail!("oauth authorize_url must use http or https");
    }
    if token_url.scheme() != "http" && token_url.scheme() != "https" {
        bail!("oauth token_url must use http or https");
    }
    if authorize_url.host_str().is_none() {
        bail!("oauth authorize_url must include a host");
    }
    if token_url.host_str().is_none() {
        bail!("oauth token_url must include a host");
    }

    Ok(())
}

fn create_secure_lock_file(path: &Path) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);

    #[cfg(unix)]
    {
        options.mode(0o600);
    }

    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            #[cfg(unix)]
            enforce_owner_only_permissions(path)?;
            return Ok(());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to create {}", path.display()));
        }
    };
    file.write_all(b"gambi lock\n")
        .with_context(|| format!("failed to initialize {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("failed to fsync {}", path.display()))?;

    #[cfg(unix)]
    enforce_owner_only_permissions(path)?;

    fsync_parent_dir(path)
}

fn open_lock_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);

    #[cfg(unix)]
    {
        options.mode(0o600);
    }

    options.open(path)
}

fn write_new_secure_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);

    #[cfg(unix)]
    {
        options.mode(0o600);
    }

    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            #[cfg(unix)]
            enforce_owner_only_permissions(path)?;
            return Ok(());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to create {}", path.display()));
        }
    };
    file.write_all(bytes)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("failed to fsync {}", path.display()))?;

    #[cfg(unix)]
    enforce_owner_only_permissions(path)?;

    fsync_parent_dir(path)
}

fn atomic_write_json<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    atomic_write_json_inner(path, data, None::<fn() -> Result<()>>)
}

fn atomic_write_json_inner<T, F>(path: &Path, data: &T, before_persist: Option<F>) -> Result<()>
where
    T: Serialize,
    F: FnOnce() -> Result<()>,
{
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path '{}' has no parent", path.display()))?;

    let mut temp = Builder::new()
        .prefix(".gambi-tmp-")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create temp file in {}", parent.display()))?;

    #[cfg(unix)]
    {
        let perms = fs::Permissions::from_mode(0o600);
        temp.as_file_mut().set_permissions(perms)?;
    }

    serde_json::to_writer_pretty(temp.as_file_mut(), data)
        .with_context(|| format!("failed to serialize JSON for {}", path.display()))?;
    temp.as_file_mut().write_all(b"\n")?;
    temp.as_file_mut().flush()?;
    temp.as_file_mut().sync_data()?;

    if let Some(hook) = before_persist {
        hook()?;
    }

    temp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| {
            format!(
                "failed to atomically persist temp file to {}",
                path.display()
            )
        })?;

    #[cfg(unix)]
    enforce_owner_only_permissions(path)?;

    fsync_parent_dir(path)
}

fn fsync_parent_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("path '{}' has no parent", path.display()))?;
        let dir = File::open(parent)
            .with_context(|| format!("failed to open parent directory {}", parent.display()))?;
        dir.sync_all()
            .with_context(|| format!("failed to fsync parent directory {}", parent.display()))?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

fn is_weaker_than_owner_only(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        Ok(mode & 0o077 != 0)
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

fn is_directory_weaker_than_owner_only(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        Ok(mode & 0o077 != 0)
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

#[cfg(unix)]
fn enforce_owner_only_permissions(path: &Path) -> Result<()> {
    let perms = fs::Permissions::from_mode(0o600);
    fs::set_permissions(path, perms)
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

#[cfg(unix)]
fn enforce_owner_only_directory_permissions(path: &Path) -> Result<()> {
    let perms = fs::Permissions::from_mode(0o700);
    fs::set_permissions(path, perms)
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{
        AppConfig, ConfigStore, MAX_SERVER_INSTRUCTION_LENGTH, OAuthConfig, RuntimeProfile,
        ServerConfig, ServerInstructionSource, TokenStorage, TokenStorePreference,
        ToolActivationMode, ToolPolicyLevel, ToolPolicyMode, tool_matches_safe_heuristic,
    };
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn add_and_remove_servers_roundtrip() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = ConfigStore::with_base_dir(temp_dir.path().join("gambi"));

        store
            .update(|cfg| {
                cfg.add_server(ServerConfig {
                    name: "port".to_string(),
                    url: "https://example.com/mcp".to_string(),
                    oauth: None,
                    transport: Default::default(),
                    exposure_mode: Default::default(),
                    enabled: true,
                })
            })
            .expect("add server");

        let cfg = store.load().expect("load config");
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name, "port");

        let removed = store
            .update(|cfg| Ok(cfg.remove_server("port")))
            .expect("remove server");
        assert!(removed);

        let cfg = store.load().expect("load config");
        assert!(cfg.servers.is_empty());
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let mut cfg = AppConfig::default();
        cfg.add_server(ServerConfig {
            name: "github".to_string(),
            url: "https://example.com".to_string(),
            oauth: None,
            transport: Default::default(),
            exposure_mode: Default::default(),
            enabled: true,
        })
        .expect("first add should succeed");

        let err = cfg
            .add_server(ServerConfig {
                name: "github".to_string(),
                url: "https://example.com/2".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            })
            .expect_err("duplicate add should fail");

        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn invalid_server_names_are_rejected() {
        let mut cfg = AppConfig::default();

        let with_colon = cfg
            .add_server(ServerConfig {
                name: "bad:name".to_string(),
                url: "https://example.com".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            })
            .expect_err("name with colon should fail");
        assert!(with_colon.to_string().contains("cannot contain ':'"));

        let with_space = cfg
            .add_server(ServerConfig {
                name: "bad name".to_string(),
                url: "https://example.com".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            })
            .expect_err("name with whitespace should fail");
        assert!(with_space.to_string().contains("whitespace"));

        let with_dash = cfg
            .add_server(ServerConfig {
                name: "bad-name".to_string(),
                url: "https://example.com".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            })
            .expect_err("name with dash should fail");
        assert!(with_dash.to_string().contains("cannot contain '-'"));
    }

    #[test]
    fn invalid_server_urls_are_rejected() {
        let mut cfg = AppConfig::default();

        let bad_scheme = cfg
            .add_server(ServerConfig {
                name: "port".to_string(),
                url: "ftp://example.com".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            })
            .expect_err("bad scheme should fail");
        assert!(
            bad_scheme
                .to_string()
                .contains("unsupported server url scheme")
        );

        let bad_stdio = cfg
            .add_server(ServerConfig {
                name: "stdio_bad".to_string(),
                url: "stdio:///".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            })
            .expect_err("empty stdio command should fail");
        assert!(
            bad_stdio
                .to_string()
                .contains("must include a command host or path")
        );

        cfg.add_server(ServerConfig {
            name: "stdio".to_string(),
            url: "stdio://local-server".to_string(),
            oauth: None,
            transport: Default::default(),
            exposure_mode: Default::default(),
            enabled: true,
        })
        .expect("stdio transport should be allowed");
    }

    #[test]
    fn tool_description_overrides_are_validated_and_pruned_on_server_removal() {
        let mut cfg = AppConfig::default();
        cfg.add_server(ServerConfig {
            name: "port".to_string(),
            url: "https://example.com/mcp".to_string(),
            oauth: None,
            transport: Default::default(),
            exposure_mode: Default::default(),
            enabled: true,
        })
        .expect("valid server should be added");

        cfg.set_tool_description_override("port", "list_entities", "List Port entities")
            .expect("valid override should be added");
        assert_eq!(
            cfg.tool_description_override_for("port", "list_entities"),
            Some("List Port entities")
        );

        assert!(cfg.remove_server("port"));
        assert!(
            cfg.tool_description_override_for("port", "list_entities")
                .is_none(),
            "removing a server should prune related overrides"
        );
    }

    #[test]
    fn tool_description_overrides_reject_unknown_or_empty_values() {
        let mut cfg = AppConfig::default();
        cfg.add_server(ServerConfig {
            name: "github".to_string(),
            url: "https://example.com/mcp".to_string(),
            oauth: None,
            transport: Default::default(),
            exposure_mode: Default::default(),
            enabled: true,
        })
        .expect("valid server should be added");

        let unknown = cfg
            .set_tool_description_override("missing", "search", "desc")
            .expect_err("override should reject unknown server");
        assert!(unknown.to_string().contains("unknown server"));

        let empty_tool = cfg
            .set_tool_description_override("github", "  ", "desc")
            .expect_err("override should reject empty tool");
        assert!(empty_tool.to_string().contains("tool name cannot be empty"));

        let empty_desc = cfg
            .set_tool_description_override("github", "search", " ")
            .expect_err("override should reject empty description");
        assert!(
            empty_desc
                .to_string()
                .contains("description cannot be empty")
        );
    }

    #[test]
    fn server_instruction_overrides_are_validated_and_pruned_on_server_removal() {
        let mut cfg = AppConfig::default();
        cfg.add_server(ServerConfig {
            name: "port".to_string(),
            url: "https://example.com/mcp".to_string(),
            oauth: None,
            transport: Default::default(),
            exposure_mode: Default::default(),
            enabled: true,
        })
        .expect("valid server should be added");

        cfg.set_server_instruction_override("port", "Use this MCP for SDLC workflows")
            .expect("valid override should be added");
        assert_eq!(
            cfg.server_instruction_override_for("port"),
            Some("Use this MCP for SDLC workflows")
        );

        assert!(cfg.remove_server("port"));
        assert!(
            cfg.server_instruction_override_for("port").is_none(),
            "removing a server should prune related instruction override"
        );
    }

    #[test]
    fn server_instruction_overrides_reject_unknown_or_empty_values() {
        let mut cfg = AppConfig::default();
        cfg.add_server(ServerConfig {
            name: "github".to_string(),
            url: "https://example.com/mcp".to_string(),
            oauth: None,
            transport: Default::default(),
            exposure_mode: Default::default(),
            enabled: true,
        })
        .expect("valid server should be added");

        let unknown = cfg
            .set_server_instruction_override("missing", "Use for code search")
            .expect_err("override should reject unknown server");
        assert!(unknown.to_string().contains("unknown server"));

        let empty_instruction = cfg
            .set_server_instruction_override("github", " ")
            .expect_err("override should reject empty instruction");
        assert!(
            empty_instruction
                .to_string()
                .contains("instruction cannot be empty")
        );

        let too_long = "x".repeat(MAX_SERVER_INSTRUCTION_LENGTH + 1);
        let too_long_err = cfg
            .set_server_instruction_override("github", &too_long)
            .expect_err("override should reject excessively long instruction");
        assert!(
            too_long_err
                .to_string()
                .contains("instruction exceeds max length"),
            "unexpected error: {too_long_err}"
        );
    }

    #[test]
    fn resolve_server_instruction_prefers_override_then_upstream() {
        let mut cfg = AppConfig::default();
        cfg.add_server(ServerConfig {
            name: "port".to_string(),
            url: "https://example.com/mcp".to_string(),
            oauth: None,
            transport: Default::default(),
            exposure_mode: Default::default(),
            enabled: true,
        })
        .expect("valid server should be added");

        let mut upstream = BTreeMap::new();
        upstream.insert("port".to_string(), "Use upstream defaults".to_string());

        let from_upstream = cfg.resolve_server_instruction(&upstream, "port");
        assert_eq!(
            from_upstream.source,
            ServerInstructionSource::Upstream,
            "upstream instruction should be used when no override exists"
        );
        assert_eq!(
            from_upstream.instruction.as_deref(),
            Some("Use upstream defaults")
        );

        cfg.set_server_instruction_override("port", "Use overridden workflow")
            .expect("override should be accepted");
        let from_override = cfg.resolve_server_instruction(&upstream, "port");
        assert_eq!(
            from_override.source,
            ServerInstructionSource::Override,
            "override must win over upstream instructions"
        );
        assert_eq!(
            from_override.instruction.as_deref(),
            Some("Use overridden workflow")
        );

        let missing = cfg.resolve_server_instruction(&upstream, "missing");
        assert_eq!(missing.source, ServerInstructionSource::None);
        assert!(missing.instruction.is_none());
    }

    #[test]
    fn tool_policy_heuristic_matches_name_and_description_prefixes() {
        assert!(tool_matches_safe_heuristic("getIssue", None));
        assert!(tool_matches_safe_heuristic("listIssues", None));
        assert!(tool_matches_safe_heuristic(
            "searchJiraIssuesUsingJql",
            None
        ));
        assert!(tool_matches_safe_heuristic(
            "randomTool",
            Some("Get account profile")
        ));
        assert!(!tool_matches_safe_heuristic("createIssue", None));
        assert!(!tool_matches_safe_heuristic(
            "createIssue",
            Some("Create a jira issue")
        ));
    }

    #[test]
    fn tool_policy_modes_and_overrides_apply_and_prune() {
        let mut cfg = AppConfig::default();
        cfg.add_server(ServerConfig {
            name: "atlassian".to_string(),
            url: "https://example.com/mcp".to_string(),
            oauth: None,
            transport: Default::default(),
            exposure_mode: Default::default(),
            enabled: true,
        })
        .expect("valid server should be added");

        cfg.set_server_tool_policy_mode("atlassian", ToolPolicyMode::AllEscalated)
            .expect("mode should be set");
        let decision = cfg.evaluate_tool_policy("atlassian", "getJiraIssue", Some("Get an issue"));
        assert_eq!(decision.level, ToolPolicyLevel::Escalated);

        cfg.set_server_tool_policy_mode("atlassian", ToolPolicyMode::Custom)
            .expect("mode should be set");
        cfg.set_tool_policy_override("atlassian", "createJiraIssue", ToolPolicyLevel::Safe)
            .expect("override should be set");
        let overridden = cfg.evaluate_tool_policy("atlassian", "createJiraIssue", None);
        assert_eq!(overridden.level, ToolPolicyLevel::Safe);

        assert!(cfg.remove_server("atlassian"));
        assert!(cfg.server_tool_policy_modes.is_empty());
        assert!(cfg.tool_policy_overrides.is_empty());
    }

    #[test]
    fn tool_activation_modes_and_overrides_apply_and_prune() {
        let mut cfg = AppConfig::default();
        cfg.add_server(ServerConfig {
            name: "atlassian".to_string(),
            url: "https://example.com/mcp".to_string(),
            oauth: None,
            transport: Default::default(),
            exposure_mode: Default::default(),
            enabled: true,
        })
        .expect("valid server should be added");

        assert!(cfg.is_tool_enabled("atlassian", "getJiraIssue"));

        cfg.set_server_tool_activation_mode("atlassian", ToolActivationMode::None)
            .expect("mode should be set");
        assert!(!cfg.is_tool_enabled("atlassian", "getJiraIssue"));

        cfg.set_tool_activation_override("atlassian", "getJiraIssue", true)
            .expect("override should be set");
        assert!(cfg.is_tool_enabled("atlassian", "getJiraIssue"));

        cfg.set_tool_activation_override("atlassian", "getJiraIssue", false)
            .expect("matching default should prune override");
        assert!(
            cfg.tool_activation_override_for("atlassian", "getJiraIssue")
                .is_none()
        );

        assert!(cfg.remove_server("atlassian"));
        assert!(cfg.server_tool_activation_modes.is_empty());
        assert!(cfg.tool_activation_overrides.is_empty());
    }

    #[test]
    fn oauth_config_is_validated() {
        let mut cfg = AppConfig::default();

        let bad_discovery = cfg
            .add_server(ServerConfig {
                name: "oauth_discovery_bad".to_string(),
                url: "https://example.com/mcp".to_string(),
                oauth: Some(OAuthConfig {
                    discovery_url: Some("ftp://example.com/discovery".to_string()),
                    authorize_url: "https://example.com/authorize".to_string(),
                    token_url: "https://example.com/token".to_string(),
                    client_id: Some("client".to_string()),
                    scopes: vec!["read".to_string()],
                }),
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            })
            .expect_err("invalid oauth discovery URL must fail");
        assert!(bad_discovery.to_string().contains("oauth discovery_url"));

        let bad = cfg
            .add_server(ServerConfig {
                name: "oauth_bad".to_string(),
                url: "https://example.com/mcp".to_string(),
                oauth: Some(OAuthConfig {
                    discovery_url: None,
                    authorize_url: "not-a-url".to_string(),
                    token_url: "https://example.com/token".to_string(),
                    client_id: Some("client".to_string()),
                    scopes: vec!["read".to_string()],
                }),
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            })
            .expect_err("invalid oauth config must fail");
        assert!(bad.to_string().contains("invalid oauth authorize_url"));

        cfg.add_server(ServerConfig {
            name: "oauth_good".to_string(),
            url: "https://example.com/mcp".to_string(),
            oauth: Some(OAuthConfig {
                discovery_url: Some(
                    "https://example.com/.well-known/openid-configuration".to_string(),
                ),
                authorize_url: "https://example.com/authorize".to_string(),
                token_url: "https://example.com/token".to_string(),
                client_id: Some("client".to_string()),
                scopes: vec!["read".to_string(), "write".to_string()],
            }),
            transport: Default::default(),
            exposure_mode: Default::default(),
            enabled: true,
        })
        .expect("valid oauth config should succeed");
    }

    #[test]
    fn invalid_servers_in_existing_config_are_rejected_on_load() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let base = temp_dir.path().join("gambi");
        std::fs::create_dir_all(&base).expect("create base dir");
        std::fs::write(
            base.join("config.json"),
            r#"{ "servers": [ { "name": "bad:name", "url": "https://example.com" } ] }"#,
        )
        .expect("write config");
        std::fs::write(base.join("tokens.json"), "{}").expect("write tokens");

        let store = ConfigStore::with_base_dir(base);
        let err = store
            .load()
            .expect_err("config with invalid namespaced server must fail");

        assert!(err.to_string().contains("invalid server definitions"));
    }

    #[test]
    fn replace_validates_and_persists_config() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = ConfigStore::with_base_dir(temp_dir.path().join("gambi"));

        let mut cfg = AppConfig::default();
        cfg.add_server(ServerConfig {
            name: "port".to_string(),
            url: "https://example.com/mcp".to_string(),
            oauth: None,
            transport: Default::default(),
            exposure_mode: Default::default(),
            enabled: true,
        })
        .expect("valid config");

        store.replace(cfg.clone()).expect("replace should succeed");
        let loaded = store.load().expect("load should succeed");
        assert_eq!(loaded, cfg);

        let invalid = AppConfig {
            servers: vec![ServerConfig {
                name: "bad:name".to_string(),
                url: "https://example.com".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            }],
            server_tool_activation_modes: BTreeMap::new(),
            tool_activation_overrides: BTreeMap::new(),
            server_tool_policy_modes: BTreeMap::new(),
            tool_description_overrides: BTreeMap::new(),
            server_instruction_overrides: BTreeMap::new(),
            tool_policy_overrides: BTreeMap::new(),
        };
        let err = store
            .replace(invalid)
            .expect_err("replace should reject invalid config");
        assert!(err.to_string().contains("invalid config for replace"));
    }

    #[test]
    fn concurrent_updates_are_serialized() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(ConfigStore::with_base_dir(temp_dir.path().join("gambi")));
        let mut workers = Vec::new();

        for idx in 0..8 {
            let store = Arc::clone(&store);
            workers.push(thread::spawn(move || {
                store.update(|cfg| {
                    cfg.add_server(ServerConfig {
                        name: format!("server_{idx}"),
                        url: format!("https://example.com/{idx}"),
                        oauth: None,
                        transport: Default::default(),
                        exposure_mode: Default::default(),
                        enabled: true,
                    })
                })
            }));
        }

        for worker in workers {
            worker
                .join()
                .expect("worker thread should not panic")
                .expect("concurrent update should succeed");
        }

        let cfg = store.load().expect("final config must load");
        assert_eq!(cfg.servers.len(), 8);
        assert_eq!(cfg.servers[0].name, "server_0");
        assert_eq!(cfg.servers[7].name, "server_7");
    }

    #[test]
    fn concurrent_initial_load_is_race_safe() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(ConfigStore::with_base_dir(temp_dir.path().join("gambi")));
        let mut workers = Vec::new();

        for _ in 0..8 {
            let store = Arc::clone(&store);
            workers.push(thread::spawn(move || store.load()));
        }

        for worker in workers {
            worker
                .join()
                .expect("worker should not panic")
                .expect("initial load should succeed under concurrency");
        }
    }

    #[test]
    fn atomic_write_preserves_existing_file_on_pre_persist_failure() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let target = temp_dir.path().join("config.json");
        std::fs::write(&target, "{\n  \"servers\": []\n}\n").expect("seed target");

        let replacement = AppConfig {
            servers: vec![ServerConfig {
                name: "port".to_string(),
                url: "https://example.com/mcp".to_string(),
                oauth: None,
                transport: Default::default(),
                exposure_mode: Default::default(),
                enabled: true,
            }],
            server_tool_activation_modes: BTreeMap::new(),
            tool_activation_overrides: BTreeMap::new(),
            server_tool_policy_modes: BTreeMap::new(),
            tool_description_overrides: BTreeMap::new(),
            server_instruction_overrides: BTreeMap::new(),
            tool_policy_overrides: BTreeMap::new(),
        };

        let err = super::atomic_write_json_inner(
            &target,
            &replacement,
            Some(|| anyhow::bail!("simulated crash before rename")),
        )
        .expect_err("simulated pre-persist failure must propagate");
        assert!(
            err.to_string().contains("simulated crash"),
            "unexpected error: {err}"
        );

        let unchanged = std::fs::read_to_string(&target).expect("target should remain readable");
        assert_eq!(unchanged, "{\n  \"servers\": []\n}\n");
    }

    #[cfg(unix)]
    #[test]
    fn created_files_are_owner_only() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = ConfigStore::with_base_dir(temp_dir.path().join("gambi"));

        store.load().expect("load should create files");

        let cfg_mode = std::fs::metadata(&store.paths.config_file)
            .expect("config metadata")
            .permissions()
            .mode()
            & 0o777;
        let tokens_mode = std::fs::metadata(&store.paths.tokens_file)
            .expect("tokens metadata")
            .permissions()
            .mode()
            & 0o777;
        let dir_mode = std::fs::metadata(&store.paths.config_dir)
            .expect("config dir metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(cfg_mode, 0o600);
        assert_eq!(tokens_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn runtime_profile_parsing_accepts_aliases() {
        assert_eq!(
            RuntimeProfile::parse("production").expect("production parses"),
            RuntimeProfile::Production
        );
        assert_eq!(
            RuntimeProfile::parse("prod").expect("prod alias parses"),
            RuntimeProfile::Production
        );
        assert!(RuntimeProfile::parse("qa").is_err());
    }

    #[test]
    fn token_store_preference_auto_resolves_by_profile() {
        assert_eq!(
            TokenStorePreference::Auto.resolve(RuntimeProfile::Local),
            TokenStorage::File
        );
        assert_eq!(
            TokenStorePreference::Auto.resolve(RuntimeProfile::Dev),
            TokenStorage::File
        );
        assert_eq!(
            TokenStorePreference::Auto.resolve(RuntimeProfile::Production),
            TokenStorage::Keychain
        );
    }

    #[test]
    fn keychain_storage_mode_does_not_require_tokens_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let store = ConfigStore::with_base_dir_and_storage(
            temp_dir.path().join("gambi"),
            RuntimeProfile::Production,
            TokenStorage::Keychain,
        );

        store.load().expect("load should initialize layout");
        assert!(!store.paths.tokens_file.exists());

        let weak = store
            .weak_permission_paths()
            .expect("weak-permission check should succeed");
        assert!(
            !weak.contains(&store.paths.tokens_file),
            "tokens file should be ignored for keychain mode"
        );
    }
}
