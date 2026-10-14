//! The single list of WebAssembly proposals a task module may use. Shared by module
//! validation and the native engine configuration.

use wasmparser::WasmFeatures;

/// A WebAssembly proposal allowed beyond the MVP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Proposal {
    /// Bulk memory operations.
    BulkMemory,
    /// Multiple results and block parameters.
    MultiValue,
    /// Import and export of mutable globals.
    MutableGlobals,
    /// Saturating float-to-int conversions.
    NontrappingFptoint,
    /// Reference types (as emitted by rustc for tables).
    ReferenceTypes,
    /// Sign-extension operators.
    SignExt,
    /// Fixed-width 128-bit SIMD (not relaxed SIMD).
    Simd128,
}

impl Proposal {
    /// Name as used by rustc target features.
    pub fn name(self) -> &'static str {
        match self {
            Proposal::BulkMemory => "bulk-memory",
            Proposal::MultiValue => "multivalue",
            Proposal::MutableGlobals => "mutable-globals",
            Proposal::NontrappingFptoint => "nontrapping-fptoint",
            Proposal::ReferenceTypes => "reference-types",
            Proposal::SignExt => "sign-ext",
            Proposal::Simd128 => "simd128",
        }
    }
}

/// Every allowed proposal.
pub const ALLOWED_PROPOSALS: &[Proposal] = &[
    Proposal::BulkMemory,
    Proposal::MultiValue,
    Proposal::MutableGlobals,
    Proposal::NontrappingFptoint,
    Proposal::ReferenceTypes,
    Proposal::SignExt,
    Proposal::Simd128,
];

/// Validator features: the MVP (floats and its base types) plus [`ALLOWED_PROPOSALS`].
/// `BULK_MEMORY` and `REFERENCE_TYPES` include their `BULK_MEMORY_OPT` and
/// `CALL_INDIRECT_OVERLONG` encodings, which rustc emits by default.
pub const WASM_FEATURES: WasmFeatures = WasmFeatures::FLOATS
    .union(WasmFeatures::GC_TYPES)
    .union(WasmFeatures::BULK_MEMORY)
    .union(WasmFeatures::MULTI_VALUE)
    .union(WasmFeatures::MUTABLE_GLOBAL)
    .union(WasmFeatures::SATURATING_FLOAT_TO_INT)
    .union(WasmFeatures::REFERENCE_TYPES)
    .union(WasmFeatures::SIGN_EXTENSION)
    .union(WasmFeatures::SIMD);
