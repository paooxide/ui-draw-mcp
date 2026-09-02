use std::collections::HashMap;
use std::sync::Arc;

use mcp_types::{ToolDescriptor, ToolModule};

/// Aggregates all engine `ToolModule`s and indexes their tools by name. Built
/// once at startup; rejects duplicate tool names (a wiring bug).
pub struct Registry {
    modules: Vec<Arc<dyn ToolModule>>,
    /// tool name -> (module index, descriptor)
    index: HashMap<String, (usize, ToolDescriptor)>,
    /// stable insertion order of tool names (for deterministic `tools/list`)
    order: Vec<String>,
}

impl Registry {
    pub fn build(modules: Vec<Arc<dyn ToolModule>>) -> Result<Self, String> {
        let mut index = HashMap::new();
        let mut order = Vec::new();
        for (i, module) in modules.iter().enumerate() {
            for descriptor in module.descriptors() {
                if index.contains_key(&descriptor.name) {
                    return Err(format!("duplicate tool name: {}", descriptor.name));
                }
                order.push(descriptor.name.clone());
                index.insert(descriptor.name.clone(), (i, descriptor));
            }
        }
        Ok(Registry {
            modules,
            index,
            order,
        })
    }

    /// Look up the owning module (cheap `Arc` clone) and descriptor for a tool.
    pub fn find(&self, name: &str) -> Option<(Arc<dyn ToolModule>, &ToolDescriptor)> {
        self.index
            .get(name)
            .map(|(i, d)| (self.modules[*i].clone(), d))
    }

    /// All descriptors in stable order.
    pub fn descriptors(&self) -> impl Iterator<Item = &ToolDescriptor> {
        self.order.iter().map(move |name| &self.index[name].1)
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}
