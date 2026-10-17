use serde::{Deserialize, Serialize};

use crate::{ABI_VERSION, CapabilityId, IdParseError};

/// Name of the custom section that carries the manifest.
pub const MANIFEST_SECTION: &str = "orbit.manifest";

/// The capability manifest of a module (`docs/host-abi.md` section 6).
///
/// A manifest *describes* requirements; it never authorises anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// ABI version the module was built against.
    pub abi: u32,
    /// Capabilities in manifest form (`log/v1`).
    #[serde(default)]
    pub requires: Vec<String>,
    /// Free-form producer string, e.g. `orin 0.1.0`.
    #[serde(default)]
    pub producer: Option<String>,
}

/// Failure to read a manifest.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The section is not valid JSON of the expected shape.
    #[error("manifest is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The manifest targets an ABI version this crate does not implement.
    #[error("unsupported ABI version {found} (this host implements {ABI_VERSION})")]
    UnsupportedAbi {
        /// The version found.
        found: u32,
    },
    /// A `requires` entry is malformed.
    #[error(transparent)]
    BadRequirement(#[from] IdParseError),
}

impl Manifest {
    /// Parses and validates the contents of the `orbit.manifest` section.
    ///
    /// # Errors
    /// Fails on malformed JSON, an unsupported ABI version, or a malformed `requires` entry.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let m: Manifest = serde_json::from_slice(bytes)?;
        if m.abi != ABI_VERSION {
            return Err(ManifestError::UnsupportedAbi { found: m.abi });
        }
        m.required()?;
        Ok(m)
    }

    /// The declared requirements as ids.
    ///
    /// # Errors
    /// Fails when an entry is not of the form `name/vN`.
    pub fn required(&self) -> Result<Vec<CapabilityId>, IdParseError> {
        self.requires
            .iter()
            .map(|s| CapabilityId::from_manifest_form(s))
            .collect()
    }

    /// Serialises the manifest for embedding as a custom section.
    #[must_use]
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("manifest serialisation is infallible")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example() {
        let m = Manifest::parse(
            br#"{"abi": 1, "requires": ["log/v1", "kv/v1"], "producer": "orin 0.1.0"}"#,
        )
        .unwrap();
        assert_eq!(
            m.required().unwrap(),
            vec![CapabilityId::log_v1(), CapabilityId::kv_v1()]
        );
        assert_eq!(m.producer.as_deref(), Some("orin 0.1.0"));
    }

    #[test]
    fn rejects_bad() {
        assert!(matches!(
            Manifest::parse(b"nope"),
            Err(ManifestError::Json(_))
        ));
        assert!(matches!(
            Manifest::parse(br#"{"abi": 2}"#),
            Err(ManifestError::UnsupportedAbi { found: 2 })
        ));
        assert!(matches!(
            Manifest::parse(br#"{"abi": 1, "requires": ["x"]}"#),
            Err(ManifestError::BadRequirement(_))
        ));
    }
}
