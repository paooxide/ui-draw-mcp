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
    /// Human-readable name for a client's tool list. Derived from `name` when
    /// unset (see [`ToolDescriptor::display_title`]).
    pub title: Option<String>,
    /// Whether calling twice with the same arguments has the same effect as
    /// calling once. Defaults to "read-tier tools are, others are not", which
    /// is right often enough that only the exceptions are declared.
    pub idempotent: Option<bool>,
    /// Whether the tool touches things outside this machine. Defaults from the
    /// category.
    pub open_world: Option<bool>,
    /// Whether this tool's results contain text from outside the trust
    /// boundary — a web page, a file, terminal output, an application's own
    /// accessibility labels.
    ///
    /// The driving model reads those results as part of its context, so they
    /// are an injection surface. Engines set this because engines know what
    /// their output contains; the core marks the result centrally.
    pub untrusted_output: bool,
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
            title: None,
            idempotent: None,
            open_world: None,
            untrusted_output: false,
        }
    }

    /// Override the derived display title.
    pub fn titled(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Declare idempotence explicitly, where the tier's default is wrong.
    pub fn idempotent(mut self, yes: bool) -> Self {
        self.idempotent = Some(yes);
        self
    }

    /// Declare whether the tool reaches outside this machine.
    pub fn open_world(mut self, yes: bool) -> Self {
        self.open_world = Some(yes);
        self
    }

    /// Mark this tool's results as carrying content from outside the trust
    /// boundary.
    pub fn untrusted_output(mut self) -> Self {
        self.untrusted_output = true;
        self
    }

    /// A display title: the explicit one, else derived from the tool name.
    ///
    /// `get_ui_tree` becomes "Get UI Tree" rather than "Get Ui Tree" — the
    /// acronyms are spelled out because a client renders this to a person.
    pub fn display_title(&self) -> String {
        if let Some(t) = &self.title {
            return t.clone();
        }
        const ACRONYMS: &[&str] = &[
            "ui", "url", "dns", "http", "pty", "ssh", "gpg", "os", "id", "cpu", "js", "dom", "fs",
            "ocr",
        ];
        self.name
            .split('_')
            .map(|w| {
                if ACRONYMS.contains(&w) {
                    w.to_uppercase()
                } else {
                    let mut c = w.chars();
                    match c.next() {
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                        None => String::new(),
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
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
