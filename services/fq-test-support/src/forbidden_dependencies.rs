//! Cargo-manifest dependency discovery for source-level dependency gates.

use std::collections::BTreeSet;

/// Return every dependency name that can reach a normal build.
///
/// Both the manifest key and a renamed dependency's `package` value are
/// returned. Development dependencies are deliberately excluded because they
/// do not reach binaries that depend on the crate under test.
pub fn dependency_names(manifest: &str) -> Result<BTreeSet<String>, toml::de::Error> {
    let manifest = manifest.parse::<toml::Table>()?;
    let mut names = BTreeSet::new();

    collect_dependency_table(&manifest, "dependencies", &mut names);
    collect_dependency_table(&manifest, "build-dependencies", &mut names);

    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            collect_dependency_table(target, "dependencies", &mut names);
            collect_dependency_table(target, "build-dependencies", &mut names);
        }
    }

    Ok(names)
}

fn collect_dependency_table(
    manifest: &toml::Table,
    table_name: &str,
    names: &mut BTreeSet<String>,
) {
    let Some(dependencies) = manifest.get(table_name).and_then(toml::Value::as_table) else {
        return;
    };

    for (name, specification) in dependencies {
        names.insert(name.clone());
        if let Some(package) = specification
            .as_table()
            .and_then(|table| table.get("package"))
            .and_then(toml::Value::as_str)
        {
            names.insert(package.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::dependency_names;

    #[test]
    fn finds_all_non_development_dependency_forms() {
        let manifest = r#"
[dependencies]
serde = "1"
reqwest = { version = "0.12", features = ["json"] }
renamed_nats = { package = "async-nats", version = "0.38" }

[dependencies.sqlx]
version = "0.8"
features = ["sqlite"]

[build-dependencies]
cc = "1"

[target.'cfg(unix)'.dependencies]
libc = "0.2"

[target.'cfg(windows)'.build-dependencies]
windows-bindgen = "0.62"

[dev-dependencies]
genai = "0.4"

[target.'cfg(unix)'.dev-dependencies]
rmcp = "0.3"
"#;

        let names = dependency_names(manifest).expect("valid inline manifest");
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            [
                "async-nats",
                "cc",
                "libc",
                "renamed_nats",
                "reqwest",
                "serde",
                "sqlx",
                "windows-bindgen",
            ]
        );
    }

    #[test]
    fn malformed_manifest_is_reported() {
        assert!(dependency_names("[dependencies").is_err());
    }
}
