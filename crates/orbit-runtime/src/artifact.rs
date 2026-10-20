//! Loading, validating and compiling modules.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;

use orbit_abi::{
    CapabilityId, MANIFEST_SECTION, MEMORY_EXPORT, Manifest, SHARED_MEMORY_IMPORT,
    THREAD_START_EXPORT, ValKind,
};
use sha2::{Digest, Sha256};
use wasmparser::{DataKind, Parser, Payload};
use wasmtime::{ExternType, InstancePre, Linker, Module, ValType};

use crate::capability::{Capability, CapabilityRegistry};
use crate::engine::OrbitEngine;
use crate::error::LoadError;
use crate::host::HostState;

/// SHA-256 of the WebAssembly binary.
pub type ContentHash = [u8; 32];

/// One import of a module, as declared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportInfo {
    /// Import module name.
    pub module: String,
    /// Import field name.
    pub name: String,
    /// `func`, `memory`, `table`, `global` or `tag`.
    pub kind: &'static str,
    /// Rendered signature for functions, e.g. `(i32, i64, i64) -> i32`.
    pub signature: Option<String>,
}

/// One export of a module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportInfo {
    /// Export name.
    pub name: String,
    /// `func`, `memory`, `table`, `global` or `tag`.
    pub kind: &'static str,
    /// Rendered signature for functions.
    pub signature: Option<String>,
}

/// What a module declares, without any validation against the ABI. Useful for `inspect`.
#[derive(Clone, Debug)]
pub struct ModuleInfo {
    /// SHA-256 of the binary.
    pub hash: ContentHash,
    /// Declared imports.
    pub imports: Vec<ImportInfo>,
    /// Declared exports.
    pub exports: Vec<ExportInfo>,
    /// The raw `orbit.manifest` sections found (zero or one in a valid module).
    pub manifest_sections: Vec<Vec<u8>>,
}

impl ModuleInfo {
    /// Compiles `bytes` (binary or WAT) and reports its imports and exports.
    ///
    /// # Errors
    /// [`LoadError::Invalid`] / [`LoadError::Compile`] if the module is not valid.
    pub fn inspect(engine: &OrbitEngine, bytes: &[u8]) -> Result<Self, LoadError> {
        let wasm = to_binary(bytes)?;
        let module = compile(engine, &wasm)?;
        Ok(Self::from_module(&module, &wasm))
    }

    fn from_module(module: &Module, wasm: &[u8]) -> Self {
        let imports = module
            .imports()
            .map(|i| {
                let (kind, signature) = describe(&i.ty());
                ImportInfo {
                    module: i.module().to_owned(),
                    name: i.name().to_owned(),
                    kind,
                    signature,
                }
            })
            .collect();
        let exports = module
            .exports()
            .map(|e| {
                let (kind, signature) = describe(&e.ty());
                ExportInfo {
                    name: e.name().to_owned(),
                    kind,
                    signature,
                }
            })
            .collect();
        Self {
            hash: hash_of(wasm),
            imports,
            exports,
            manifest_sections: custom_sections(wasm, MANIFEST_SECTION),
        }
    }
}

pub(crate) fn hash_of(wasm: &[u8]) -> ContentHash {
    Sha256::digest(wasm).into()
}

/// Lower-case hex of a content hash.
#[must_use]
pub fn hash_hex(hash: &ContentHash) -> String {
    hash.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn describe(ty: &ExternType) -> (&'static str, Option<String>) {
    match ty {
        ExternType::Func(f) => {
            let params = f
                .params()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let results = f.results().map(|p| p.to_string()).collect::<Vec<_>>();
            let sig = match results.as_slice() {
                [] => format!("({params})"),
                r => format!("({params}) -> {}", r.join(", ")),
            };
            ("func", Some(sig))
        }
        ExternType::Memory(_) => ("memory", None),
        ExternType::Table(_) => ("table", None),
        ExternType::Global(_) => ("global", None),
        ExternType::Tag(_) => ("tag", None),
    }
}

fn custom_sections(wasm: &[u8], name: &str) -> Vec<Vec<u8>> {
    Parser::new(0)
        .parse_all(wasm)
        .filter_map(|p| match p {
            Ok(Payload::CustomSection(c)) if c.name() == name => Some(c.data().to_vec()),
            _ => None,
        })
        .collect()
}

pub(crate) fn to_binary(bytes: &[u8]) -> Result<Cow<'_, [u8]>, LoadError> {
    if bytes.starts_with(b"\0asm") {
        Ok(Cow::Borrowed(bytes))
    } else {
        wat::parse_bytes(bytes).map_err(|e| LoadError::Invalid(e.to_string()))
    }
}

fn compile(engine: &OrbitEngine, wasm: &[u8]) -> Result<Module, LoadError> {
    Module::new(engine.wasmtime(), wasm).map_err(|e| LoadError::Compile(format!("{e:#}")))
}

fn kind_of(v: &ValType) -> Option<ValKind> {
    match v {
        ValType::I32 => Some(ValKind::I32),
        ValType::I64 => Some(ValKind::I64),
        _ => None,
    }
}

/// The shared memory a threaded module imports as `env.memory`, as declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedMemoryDecl {
    /// Declared minimum, in 64 KiB pages.
    pub min_pages: u64,
    /// Declared maximum, in 64 KiB pages.
    pub max_pages: u64,
}

/// Scans the module for what threads cannot support: a shared memory the module defines itself
/// and (when `threaded`) active data segments.
fn scan_memory_use(wasm: &[u8], threaded: bool) -> Result<(), LoadError> {
    for payload in Parser::new(0).parse_all(wasm) {
        match payload {
            Ok(Payload::MemorySection(reader)) => {
                for m in reader.into_iter().flatten() {
                    if m.shared {
                        return Err(LoadError::BadSharedMemory(
                            "the module defines a shared memory; a shared memory must be imported as \
                             `(import \"env\" \"memory\" (memory N MAX shared))` (link with \
                             --shared-memory --import-memory)"
                                .into(),
                        ));
                    }
                }
            }
            Ok(Payload::DataSection(reader)) if threaded => {
                for d in reader.into_iter().flatten() {
                    if matches!(d.kind, DataKind::Active { .. }) {
                        return Err(LoadError::ThreadModule(
                            "an active data segment would be applied again by every thread instance \
                             and overwrite live memory; use passive segments (link with \
                             --shared-memory, which makes wasm-ld emit `__wasm_init_memory`)"
                                .into(),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// A validated, compiled module ready to be instantiated, together with its capability
/// requirements. Cheap to share (`Arc<Artifact>`); see [`Runtime::load`](crate::Runtime::load).
pub struct Artifact {
    hash: ContentHash,
    module: Module,
    info: ModuleInfo,
    manifest: Option<Manifest>,
    required: BTreeSet<CapabilityId>,
    imported: BTreeSet<CapabilityId>,
    /// For a module without a shared memory import: the linked instantiation template. A
    /// threaded module cannot be pre-linked (its memory is created per isolate).
    pre: Option<InstancePre<HostState>>,
    /// For a threaded module: the linker with every required capability, to which each isolate
    /// adds its own `env.memory`.
    linker: Option<Linker<HostState>>,
    shared_memory: Option<SharedMemoryDecl>,
    linked: Vec<Arc<dyn Capability>>,
}

impl Artifact {
    /// Validates and compiles `bytes` (a binary module or WAT text).
    ///
    /// Implements `docs/host-abi.md` section 7: unknown import modules, non-function imports,
    /// signature mismatches and manifest mismatches are rejected here, before anything can run.
    ///
    /// # Errors
    /// A [`LoadError`] describing the first violation found.
    pub fn load(
        engine: &OrbitEngine,
        registry: &CapabilityRegistry,
        bytes: &[u8],
    ) -> Result<Self, LoadError> {
        let wasm = to_binary(bytes)?;
        let module = compile(engine, &wasm)?;
        let info = ModuleInfo::from_module(&module, &wasm);

        let has_memory_export = module
            .exports()
            .any(|e| e.name() == MEMORY_EXPORT && matches!(e.ty(), ExternType::Memory(_)));

        let memory64 = module.exports().any(|e| {
            e.name() == MEMORY_EXPORT && matches!(e.ty(), ExternType::Memory(m) if m.is_64())
        });

        let mut shared_memory = None;
        let mut imported = BTreeSet::new();
        for import in module.imports() {
            if (import.module(), import.name()) == SHARED_MEMORY_IMPORT {
                let ExternType::Memory(m) = import.ty() else {
                    return Err(LoadError::NonFunctionImport {
                        module: import.module().to_owned(),
                        name: import.name().to_owned(),
                        kind: describe(&import.ty()).0,
                    });
                };
                if !m.is_shared() {
                    return Err(LoadError::BadSharedMemory(
                        "`env.memory` is imported as an unshared memory; Orbit provides only a \
                         shared one: `(import \"env\" \"memory\" (memory N MAX shared))` (link with \
                         --shared-memory --import-memory). A module that exports its own \
                         unshared `memory` needs no import"
                            .into(),
                    ));
                }
                if m.is_64() {
                    return Err(LoadError::BadSharedMemory(
                        "`env.memory` is a 64-bit memory; shared memory is wasm32 only".into(),
                    ));
                }
                shared_memory = Some(SharedMemoryDecl {
                    min_pages: m.minimum(),
                    max_pages: m.maximum().unwrap_or(65_536),
                });
                continue;
            }
            let unknown = || LoadError::UnknownImport {
                module: import.module().to_owned(),
                name: import.name().to_owned(),
            };
            let spec = orbit_abi::import_module(import.module()).ok_or_else(unknown)?;
            let ExternType::Func(found) = import.ty() else {
                return Err(LoadError::NonFunctionImport {
                    module: import.module().to_owned(),
                    name: import.name().to_owned(),
                    kind: describe(&import.ty()).0,
                });
            };
            let expected = spec.func(import.name()).ok_or_else(unknown)?;
            let params: Option<Vec<_>> = found.params().map(|p| kind_of(&p)).collect();
            let results: Option<Vec<_>> = found.results().map(|p| kind_of(&p)).collect();
            if params.as_deref() != Some(expected.params)
                || results.as_deref() != Some(expected.results)
            {
                return Err(LoadError::SignatureMismatch {
                    module: import.module().to_owned(),
                    name: import.name().to_owned(),
                    expected: expected.to_string(),
                    found: describe(&import.ty()).1.unwrap_or_default(),
                });
            }
            if expected.uses_memory && !has_memory_export && shared_memory.is_none() {
                return Err(LoadError::MissingMemoryExport {
                    module: import.module().to_owned(),
                    name: import.name().to_owned(),
                });
            }
            if spec.wasm32_only && memory64 {
                return Err(LoadError::Memory64Unsupported {
                    module: import.module().to_owned(),
                    name: import.name().to_owned(),
                });
            }
            let id = spec.id();
            if !registry.contains(&id) {
                return Err(LoadError::UnsupportedCapability(id));
            }
            imported.insert(id);
        }

        let threaded = imported.contains(&CapabilityId::thread_v1());
        if threaded {
            if shared_memory.is_none() {
                return Err(LoadError::ThreadModule(
                    "`orbit:thread/v1` needs the shared memory import \
                     `(import \"env\" \"memory\" (memory N MAX shared))`"
                        .into(),
                ));
            }
            if imported.contains(&CapabilityId::wasi_p1_v1()) {
                return Err(LoadError::ThreadModule(
                    "`orbit:thread/v1` cannot be combined with `wasi_snapshot_preview1` (the WASI \
                     shim keeps per-instance state)"
                        .into(),
                ));
            }
            let start_ok = module.exports().any(|e| {
                e.name() == THREAD_START_EXPORT
                    && matches!(e.ty(), ExternType::Func(f)
                        if f.params().len() == 2
                            && f.params().all(|p| matches!(p, ValType::I64))
                            && f.results().len() == 0)
            });
            if !start_ok {
                return Err(LoadError::ThreadModule(format!(
                    "`orbit:thread/v1` needs an export `{THREAD_START_EXPORT}` of type (i64, i64) -> ()"
                )));
            }
        }
        scan_memory_use(&wasm, threaded)?;

        let manifest = match info.manifest_sections.as_slice() {
            [] => None,
            [one] => Some(Manifest::parse(one)?),
            _ => return Err(LoadError::DuplicateManifest),
        };
        let mut required = imported.clone();
        if let Some(m) = &manifest {
            let listed: BTreeSet<_> = m
                .required()
                .map_err(orbit_abi::ManifestError::from)?
                .into_iter()
                .collect();
            let missing: Vec<_> = imported.difference(&listed).cloned().collect();
            if !missing.is_empty() {
                return Err(LoadError::ManifestMismatch { missing });
            }
            required.extend(listed);
        }

        // The linker holds exactly the required capabilities this host serves.
        let mut linker: Linker<HostState> = Linker::new(engine.wasmtime());
        let mut linked = Vec::new();
        for id in &required {
            if let Some(cap) = registry.get(id) {
                cap.link(&mut linker)
                    .map_err(|e| LoadError::Link(format!("{e:#}")))?;
                linked.push(Arc::clone(cap));
            }
        }
        let (pre, linker) = if shared_memory.is_some() {
            (None, Some(linker))
        } else {
            let pre = linker
                .instantiate_pre(&module)
                .map_err(|e| LoadError::Link(format!("{e:#}")))?;
            (Some(pre), None)
        };

        Ok(Self {
            hash: info.hash,
            module,
            info,
            manifest,
            required,
            imported,
            pre,
            linker,
            shared_memory,
            linked,
        })
    }

    /// SHA-256 of the WebAssembly binary (the cache key).
    #[must_use]
    pub fn hash(&self) -> &ContentHash {
        &self.hash
    }

    /// The compiled module.
    #[must_use]
    pub fn module(&self) -> &Module {
        &self.module
    }

    /// Declared imports and exports.
    #[must_use]
    pub fn info(&self) -> &ModuleInfo {
        &self.info
    }

    /// The parsed manifest, if the module carries one.
    #[must_use]
    pub fn manifest(&self) -> Option<&Manifest> {
        self.manifest.as_ref()
    }

    /// Capabilities an instance needs a grant for: the imports plus anything the manifest lists.
    #[must_use]
    pub fn required(&self) -> &BTreeSet<CapabilityId> {
        &self.required
    }

    /// Capabilities the module actually imports.
    #[must_use]
    pub fn imported(&self) -> &BTreeSet<CapabilityId> {
        &self.imported
    }

    pub(crate) fn pre(&self) -> Option<&InstancePre<HostState>> {
        self.pre.as_ref()
    }

    pub(crate) fn linker(&self) -> Option<&Linker<HostState>> {
        self.linker.as_ref()
    }

    /// The shared memory the module imports as `env.memory`, if it is a threaded module (it
    /// then runs on a memory Orbit creates per isolate; see `docs/host-abi.md` section 4f).
    #[must_use]
    pub fn shared_memory(&self) -> Option<SharedMemoryDecl> {
        self.shared_memory
    }

    pub(crate) fn linked(&self) -> &[Arc<dyn Capability>] {
        &self.linked
    }
}

impl std::fmt::Debug for Artifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Artifact")
            .field("hash", &hash_hex(&self.hash))
            .field(
                "required",
                &self
                    .required
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}
