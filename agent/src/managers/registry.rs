use std::sync::Arc;

use ahash::AHashMap;
use parking_lot::RwLock;

use crate::progs::service_map::program::ServiceMap;
use crate::progs::types::Program;

#[derive(Debug, Clone)]
pub struct BuiltinRegistry {
    inner: Arc<RwLock<AHashMap<String, Arc<dyn Program>>>>,
}

impl BuiltinRegistry {
    pub fn new() -> Self {
        let registry = Self {
            inner: Arc::new(RwLock::new(AHashMap::new())),
        };
        registry
    }

    pub fn register_builtin_progs(&self) {
        let mut inner = self.inner.write();
        inner.insert("service_map".to_string(), Arc::new(ServiceMap::new()));
    }

    pub fn get(&self, name: String) -> Option<Arc<dyn Program>> {
        let inner = self.inner.read();
        inner.get(&name).cloned()
    }

    pub fn insert(&self, name: String, program: Arc<dyn Program>) -> Option<Arc<dyn Program>> {
        let mut inner = self.inner.write();
        inner.insert(name, program)
    }

    pub fn remove(&self, name: String) -> Option<Arc<dyn Program>> {
        let mut inner = self.inner.write();
        inner.remove(&name)
    }

    pub fn list(&self) -> Vec<Arc<dyn Program>> {
        let inner = self.inner.read();
        inner.values().cloned().collect()
    }
}

#[derive(Debug, Clone)]
pub struct WasmRegistry {
    inner: Arc<RwLock<AHashMap<String, Arc<dyn Program>>>>,
}

impl WasmRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(AHashMap::new())),
        }
    }

    pub fn get(&self, name: String) -> Option<Arc<dyn Program>> {
        let inner = self.inner.read();
        inner.get(&name).cloned()
    }

    pub fn insert(&self, name: String, program: Arc<dyn Program>) -> Option<Arc<dyn Program>> {
        let mut inner = self.inner.write();
        inner.insert(name, program)
    }

    pub fn remove(&self, name: String) -> Option<Arc<dyn Program>> {
        let mut inner = self.inner.write();
        inner.remove(&name)
    }

    pub fn list(&self) -> Vec<Arc<dyn Program>> {
        let inner = self.inner.read();
        inner.values().cloned().collect()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RegistryManager {
    pub builtin: BuiltinRegistry,
    pub wasm: WasmRegistry,
}

impl RegistryManager {
    pub fn new() -> Self {
        let reg_mgr = Self {
            builtin: BuiltinRegistry::new(),
            wasm: WasmRegistry::new(),
        };
        reg_mgr.builtin.register_builtin_progs();
        reg_mgr
    }
}
