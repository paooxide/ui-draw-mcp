use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Safety tier of a tool. The policy layer gates on this (see
/// `docs/architecture.md` §8): `read`/`standard` are enabled within an enabled
/// category; `dangerous` additionally requires a per-tool opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Read,
    Standard,
    Dangerous,
}

/// Capability category. The coarse `policy.categories` allowlist gates on this,
/// and `tools/list` only advertises tools whose category is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Vision,
    Input,
    Window,
    Terminal,
    Filesystem,
    Network,
    System,
    Credentials,
    Memory,
    Desktop,
    Browser,
    /// Application install/uninstall (planned, §5.12/D13 — no engine yet).
    Packages,
}

impl Category {
    /// Stable config slug (matches `planning.md` §5 and `policy.categories`).
    pub fn slug(self) -> &'static str {
        match self {
            Category::Vision => "vision",
            Category::Input => "input",
            Category::Window => "window",
            Category::Terminal => "terminal",
            Category::Filesystem => "filesystem",
            Category::Network => "network",
            Category::System => "system",
            Category::Credentials => "credentials",
            Category::Memory => "memory",
            Category::Desktop => "desktop",
            Category::Browser => "browser",
            Category::Packages => "packages",
        }
    }

    /// Parse a config slug back into a category.
    pub fn from_slug(s: &str) -> Option<Category> {
        Some(match s {
            "vision" => Category::Vision,
            "input" => Category::Input,
            "window" => Category::Window,
            "terminal" => Category::Terminal,
            "filesystem" => Category::Filesystem,
            "network" => Category::Network,
            "system" => Category::System,
            "credentials" => Category::Credentials,
            "memory" => Category::Memory,
            "desktop" => Category::Desktop,
            "browser" => Category::Browser,
            "packages" => Category::Packages,
            _ => return None,
        })
    }
}

/// A tool's public contract: name, category/tier metadata, human/agent-readable
/// description, and a JSON Schema for arguments (restricted to the Gemini-safe
/// subset — no `pattern`/`format`/`additionalProperties`).
#[derive(Debug, Clone, Serialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub category: Category,
    pub tier: Tier,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDescriptor {
    pub fn new(
        name: impl Into<String>,
        category: Category,
        tier: Tier,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        ToolDescriptor {
            name: name.into(),
            category,
            tier,
            description: description.into(),
            input_schema,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_slug_round_trips_all() {
        for c in [
            Category::Vision,
            Category::Input,
            Category::Window,
            Category::Terminal,
            Category::Filesystem,
            Category::Network,
            Category::System,
            Category::Credentials,
            Category::Memory,
            Category::Desktop,
            Category::Browser,
            Category::Packages,
        ] {
            assert_eq!(Category::from_slug(c.slug()), Some(c));
        }
    }

    #[test]
    fn unknown_slug_is_none() {
        assert_eq!(Category::from_slug("nope"), None);
    }

    #[test]
    fn tier_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Tier::Dangerous).unwrap(),
            "\"dangerous\""
        );
    }
}
