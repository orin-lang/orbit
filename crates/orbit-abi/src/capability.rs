use std::fmt;
use std::str::FromStr;

use crate::IMPORT_PREFIX;

/// Core WebAssembly value kinds used in the ABI (integers only).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ValKind {
    /// 32-bit integer.
    I32,
    /// 64-bit integer.
    I64,
}

impl fmt::Display for ValKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ValKind::I32 => "i32",
            ValKind::I64 => "i64",
        })
    }
}

/// Signature of one host function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuncSig {
    /// Function name within the import module.
    pub name: &'static str,
    /// Parameter kinds.
    pub params: &'static [ValKind],
    /// Result kinds.
    pub results: &'static [ValKind],
    /// Whether the function reads or writes guest memory, which makes an exported
    /// `memory` mandatory for modules that import it.
    pub uses_memory: bool,
}

impl fmt::Display for FuncSig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(", self.name)?;
        for (i, p) in self.params.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{p}")?;
        }
        f.write_str(")")?;
        if !self.results.is_empty() {
            f.write_str(" -> ")?;
            for (i, r) in self.results.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{r}")?;
            }
        }
        Ok(())
    }
}

/// A capability at one major version, with its function table.
#[derive(Clone, Copy, Debug)]
pub struct CapabilitySpec {
    /// Capability name, e.g. `log`.
    pub name: &'static str,
    /// Major version.
    pub major: u32,
    /// Functions of this version.
    pub funcs: &'static [FuncSig],
}

impl CapabilitySpec {
    /// The id of this capability.
    #[must_use]
    pub fn id(&self) -> CapabilityId {
        CapabilityId::new(self.name, self.major)
    }

    /// Looks up a function by name.
    #[must_use]
    pub fn func(&self, name: &str) -> Option<&'static FuncSig> {
        self.funcs.iter().find(|f| f.name == name)
    }
}

use ValKind::{I32, I64};

const fn sig(
    name: &'static str,
    params: &'static [ValKind],
    results: &'static [ValKind],
) -> FuncSig {
    FuncSig {
        name,
        params,
        results,
        uses_memory: false,
    }
}

const fn mem_sig(
    name: &'static str,
    params: &'static [ValKind],
    results: &'static [ValKind],
) -> FuncSig {
    FuncSig {
        name,
        params,
        results,
        uses_memory: true,
    }
}

/// All capabilities defined by ABI version 1.
pub static CAPABILITIES: &[CapabilitySpec] = &[
    CapabilitySpec {
        name: "log",
        major: 1,
        funcs: &[mem_sig("write", &[I32, I64, I64], &[I32])],
    },
    CapabilitySpec {
        name: "clock",
        major: 1,
        funcs: &[
            sig("now_unix_nanos", &[], &[I64]),
            sig("monotonic_nanos", &[], &[I64]),
        ],
    },
    CapabilitySpec {
        name: "random",
        major: 1,
        funcs: &[mem_sig("fill", &[I64, I64], &[I32])],
    },
    CapabilitySpec {
        name: "kv",
        major: 1,
        funcs: &[
            mem_sig("open", &[I64, I64], &[I64]),
            mem_sig("get", &[I64, I64, I64, I64, I64], &[I64]),
            mem_sig("put", &[I64, I64, I64, I64, I64], &[I32]),
            mem_sig("delete", &[I64, I64, I64], &[I32]),
            sig("close", &[I64], &[I32]),
        ],
    },
];

/// Finds the spec of a capability id, if it is part of ABI v1.
#[must_use]
pub fn spec(id: &CapabilityId) -> Option<&'static CapabilitySpec> {
    CAPABILITIES
        .iter()
        .find(|s| s.name == id.name && s.major == id.major)
}

/// Resolves an import module name such as `orbit:log/v1` to a known [`CapabilitySpec`].
#[must_use]
pub fn import_module(module: &str) -> Option<&'static CapabilitySpec> {
    let id = CapabilityId::from_import_module(module).ok()?;
    spec(&id)
}

/// Identity of a versioned capability.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CapabilityId {
    name: String,
    major: u32,
}

/// Failure to parse a capability id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid capability id `{0}`: expected `name/vN` (manifest) or `orbit:name/vN` (import)")]
pub struct IdParseError(pub String);

impl CapabilityId {
    /// Creates an id from a name and a major version.
    #[must_use]
    pub fn new(name: impl Into<String>, major: u32) -> Self {
        Self {
            name: name.into(),
            major,
        }
    }

    /// `orbit:log/v1`.
    #[must_use]
    pub fn log_v1() -> Self {
        Self::new("log", 1)
    }
    /// `orbit:clock/v1`.
    #[must_use]
    pub fn clock_v1() -> Self {
        Self::new("clock", 1)
    }
    /// `orbit:random/v1`.
    #[must_use]
    pub fn random_v1() -> Self {
        Self::new("random", 1)
    }
    /// `orbit:kv/v1`.
    #[must_use]
    pub fn kv_v1() -> Self {
        Self::new("kv", 1)
    }

    /// Capability name, e.g. `log`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Major version.
    #[must_use]
    pub fn major(&self) -> u32 {
        self.major
    }

    /// Import module name form, `orbit:log/v1`.
    #[must_use]
    pub fn import_module(&self) -> String {
        format!("{IMPORT_PREFIX}{}", self.manifest_form())
    }

    /// Manifest form, `log/v1`.
    #[must_use]
    pub fn manifest_form(&self) -> String {
        format!("{}/v{}", self.name, self.major)
    }

    /// Parses an import module name (`orbit:log/v1`).
    ///
    /// # Errors
    /// Returns [`IdParseError`] when the prefix is missing or the rest is malformed.
    pub fn from_import_module(module: &str) -> Result<Self, IdParseError> {
        module
            .strip_prefix(IMPORT_PREFIX)
            .and_then(|rest| Self::from_manifest_form(rest).ok())
            .ok_or_else(|| IdParseError(module.to_owned()))
    }

    /// Parses the manifest form (`log/v1`).
    ///
    /// # Errors
    /// Returns [`IdParseError`] when the text is not `name/vN`.
    pub fn from_manifest_form(text: &str) -> Result<Self, IdParseError> {
        let err = || IdParseError(text.to_owned());
        let (name, ver) = text.split_once('/').ok_or_else(err)?;
        let digits = ver.strip_prefix('v').ok_or_else(err)?;
        let valid_name = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !valid_name || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(err());
        }
        let major = digits.parse::<u32>().map_err(|_| err())?;
        Ok(Self {
            name: name.to_owned(),
            major,
        })
    }
}

impl fmt::Display for CapabilityId {
    /// Formats the manifest form, `log/v1`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/v{}", self.name, self.major)
    }
}

impl FromStr for CapabilityId {
    type Err = IdParseError;

    /// Accepts both `orbit:log/v1` and `log/v1`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with(IMPORT_PREFIX) {
            Self::from_import_module(s)
        } else {
            Self::from_manifest_form(s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let id = CapabilityId::log_v1();
        assert_eq!(id.import_module(), "orbit:log/v1");
        assert_eq!(id.to_string(), "log/v1");
        assert_eq!("orbit:log/v1".parse::<CapabilityId>().unwrap(), id);
        assert_eq!("log/v1".parse::<CapabilityId>().unwrap(), id);
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "log",
            "log/1",
            "log/v",
            "log/vx",
            "orbit:",
            "orbit:log",
            "Log/v1",
            "/v1",
            "log/v-1",
        ] {
            assert!(bad.parse::<CapabilityId>().is_err(), "{bad}");
        }
        assert!(CapabilityId::from_import_module("log/v1").is_err());
    }

    #[test]
    fn table_lookup() {
        let s = import_module("orbit:kv/v1").unwrap();
        assert_eq!(s.funcs.len(), 5);
        assert_eq!(s.func("get").unwrap().params.len(), 5);
        assert!(import_module("orbit:kv/v2").is_none());
        assert!(import_module("env").is_none());
        assert_eq!(CAPABILITIES.len(), 4);
        assert_eq!(s.func("close").unwrap().to_string(), "close(i64) -> i32");
    }
}
