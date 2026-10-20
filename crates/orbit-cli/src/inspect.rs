use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use orbit_abi::{CapabilityId, Manifest};
use orbit_runtime::{ModuleInfo, Runtime, RuntimeConfig, hash_hex};

pub fn inspect(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let runtime = Runtime::new(
        RuntimeConfig::default(),
        crate::run::registry(crate::run::LogFormat::Plain),
    )?;
    let info = ModuleInfo::inspect(runtime.engine(), &bytes).context("not a valid module")?;

    println!("module:   {}", path.display());
    println!("sha256:   {}", hash_hex(&info.hash));

    println!("imports:");
    if info.imports.is_empty() {
        println!("  (none)");
    }
    for i in &info.imports {
        let sig = i.signature.as_deref().unwrap_or("");
        let tag = match CapabilityId::from_import_module(&i.module) {
            Ok(id) => format!("  [capability {id}]"),
            Err(_) => String::new(),
        };
        println!("  {}.{} {} {sig}{tag}", i.module, i.name, i.kind);
    }

    println!("exports:");
    if info.exports.is_empty() {
        println!("  (none)");
    }
    for e in &info.exports {
        println!(
            "  {} {} {}",
            e.name,
            e.kind,
            e.signature.as_deref().unwrap_or("")
        );
    }

    let mut required: BTreeSet<CapabilityId> = info
        .imports
        .iter()
        .filter_map(|i| CapabilityId::from_import_module(&i.module).ok())
        .collect();
    match info.manifest_sections.as_slice() {
        [] => println!("manifest: none"),
        sections => {
            for s in sections {
                match Manifest::parse(s) {
                    Ok(m) => {
                        println!(
                            "manifest: abi {} requires [{}] producer {}",
                            m.abi,
                            m.requires.join(", "),
                            m.producer.as_deref().unwrap_or("-")
                        );
                        required.extend(m.required().unwrap_or_default());
                    }
                    Err(e) => println!("manifest: invalid ({e})"),
                }
            }
        }
    }
    println!(
        "requires: {}",
        if required.is_empty() {
            "(no capabilities)".to_owned()
        } else {
            required
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        }
    );

    match runtime.load(&bytes) {
        Ok(_) => {
            println!("validation: ok");
            Ok(())
        }
        Err(e) => {
            println!("validation: FAILED");
            bail!("{e}");
        }
    }
}
