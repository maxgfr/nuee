//! Static checks a task module must pass before it is run anywhere.

use wasmparser::types::EntityType;
use wasmparser::{CompositeInnerType, Parser, Payload, ValType, Validator};

use crate::features::WASM_FEATURES;
use crate::id::Hash;
use crate::limits::{MAX_MODULE, MAX_WASM_MEMORY};

const PAGE: u64 = 64 * 1024;

/// Facts about a valid task module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleInfo {
    /// BLAKE3 of the module bytes.
    pub hash: Hash,
    /// Size in bytes.
    pub size: usize,
    /// Initial memory, in bytes.
    pub min_memory: u64,
    /// Declared maximum memory, in bytes.
    pub max_memory: u64,
}

/// Why a module is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidateError {
    /// Larger than the module cap.
    #[error("module is {0} bytes, above the 32 MiB cap")]
    TooLarge(usize),
    /// Uses a WebAssembly feature outside the allowed list.
    #[error("module uses a disallowed WebAssembly feature: {0}")]
    Feature(String),
    /// Not valid WebAssembly.
    #[error("invalid WebAssembly: {0}")]
    Invalid(String),
    /// Task modules must not import anything.
    #[error("module imports {module}::{name}; task modules must have no imports")]
    HasImports {
        /// Import module.
        module: String,
        /// Import name.
        name: String,
    },
    /// Needs exactly one memory.
    #[error("module must define exactly one memory, found {0}")]
    MemoryCount(u32),
    /// Shared or 64-bit memory.
    #[error("memory must be a plain 32-bit non-shared memory")]
    BadMemory,
    /// The memory has no declared maximum.
    #[error("memory has no declared maximum")]
    NoMaxMemory,
    /// The declared maximum is above 1 GiB.
    #[error("memory maximum of {0} bytes is above 1 GiB")]
    MemoryTooLarge(u64),
    /// A required export is absent or of the wrong kind.
    #[error("missing export `{0}`")]
    MissingExport(&'static str),
    /// A required function export has the wrong signature.
    #[error("export `{name}` must have type {expected}")]
    BadSignature {
        /// Export name.
        name: &'static str,
        /// Expected type.
        expected: &'static str,
    },
    /// Task modules must not have a start function.
    #[error("module has a start function")]
    HasStart,
}

/// Validates a task module against the task ABI.
pub fn validate_module(bytes: &[u8]) -> Result<ModuleInfo, ValidateError> {
    if bytes.len() > MAX_MODULE {
        return Err(ValidateError::TooLarge(bytes.len()));
    }
    let types = Validator::new_with_features(WASM_FEATURES)
        .validate_all(bytes)
        .map_err(|e| {
            if e.missing_wasm_feature().is_some() {
                ValidateError::Feature(e.to_string())
            } else {
                ValidateError::Invalid(e.to_string())
            }
        })?;
    let types = types.as_ref();

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| ValidateError::Invalid(e.to_string()))?;
        if let Payload::StartSection { .. } = payload {
            return Err(ValidateError::HasStart);
        }
    }

    if let Some(mut imports) = types.core_imports()
        && let Some((module, name, _)) = imports.next()
    {
        return Err(ValidateError::HasImports {
            module: module.to_owned(),
            name: name.to_owned(),
        });
    }

    if types.memory_count() != 1 {
        return Err(ValidateError::MemoryCount(types.memory_count()));
    }
    let mem = types.memory_at(0);
    if mem.memory64 || mem.shared || mem.page_size_log2.is_some() {
        return Err(ValidateError::BadMemory);
    }
    let max_pages = mem.maximum.ok_or(ValidateError::NoMaxMemory)?;
    let max_memory = max_pages.saturating_mul(PAGE);
    if max_memory > MAX_WASM_MEMORY {
        return Err(ValidateError::MemoryTooLarge(max_memory));
    }

    let exports: Vec<(&str, EntityType)> = types
        .core_exports()
        .map(|it| it.collect())
        .unwrap_or_default();
    let find = |name: &'static str| {
        exports
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t)
            .ok_or(ValidateError::MissingExport(name))
    };
    match find("memory")? {
        EntityType::Memory(_) => {}
        _ => return Err(ValidateError::MissingExport("memory")),
    }
    let check_fn =
        |name: &'static str, params: &[ValType], results: &[ValType], expected: &'static str| {
            let id = match find(name)? {
                EntityType::Func(id) | EntityType::FuncExact(id) => id,
                _ => return Err(ValidateError::MissingExport(name)),
            };
            let ok = match &types[id].composite_type.inner {
                CompositeInnerType::Func(f) => f.params() == params && f.results() == results,
                _ => false,
            };
            if ok {
                Ok(())
            } else {
                Err(ValidateError::BadSignature { name, expected })
            }
        };
    check_fn(
        "nuee_alloc",
        &[ValType::I32],
        &[ValType::I32],
        "(i32) -> i32",
    )?;
    check_fn(
        "nuee_run",
        &[ValType::I32, ValType::I32],
        &[ValType::I64],
        "(i32, i32) -> i64",
    )?;

    Ok(ModuleInfo {
        hash: Hash::of(bytes),
        size: bytes.len(),
        min_memory: mem.initial.saturating_mul(PAGE),
        max_memory,
    })
}
