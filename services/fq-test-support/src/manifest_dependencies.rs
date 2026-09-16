//! Cargo manifest dependency inspection for source-level architecture gates.

use std::collections::BTreeSet;

/// Parse a Cargo manifest and return every dependency name that can affect a
/// normal build.
///
/// Both the manifest key and a dependency's `package` value are returned, so
/// aliases cannot hide a forbidden package. Development dependencies are
/// intentionally excluded because they do not reach consumer binaries.
pub fn manifest_dependency_names(manifest: &str) -> Result<BTreeSet<String>, toml::de::Error> {
    let manifest: toml::Value = toml::from_str(manifest)?;
    let mut names = BTreeSet::new();

    if let Some(root) = manifest.as_table() {
        collect_dependency_table(root.get("dependencies"), &mut names);
        collect_dependency_table(root.get("build-dependencies"), &mut names);

        if let Some(targets) = root.get("target").and_then(toml::Value::as_table) {
            for target in targets.values().filter_map(toml::Value::as_table) {
                collect_dependency_table(target.get("dependencies"), &mut names);
                collect_dependency_table(target.get("build-dependencies"), &mut names);
            }
        }
    }

    Ok(names)
}

fn collect_dependency_table(value: Option<&toml::Value>, names: &mut BTreeSet<String>) {
    let Some(dependencies) = value.and_then(toml::Value::as_table) else {
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
    use super::manifest_dependency_names;

    fn names(manifest: &str) -> std::collections::BTreeSet<String> {
        manifest_dependency_names(manifest).expect("valid test manifest")
    }

    #[test]
    fn reads_plain_dependency_table() {
        assert!(names("[dependencies]\nserde = \"1\"\n").contains("serde"));
    }

    #[test]
    fn reads_dependency_subtable() {
        assert!(names("[dependencies.sqlx]\nversion = \"0.8\"\n").contains("sqlx"));
    }

    #[test]
    fn reads_inline_dependency_with_features() {
        assert!(
            names("[dependencies]\ntokio = { version = \"1\", features = [\"rt\"] }\n")
                .contains("tokio")
        );
    }

    #[test]
    fn reads_target_dependency_table() {
        assert!(names("[target.'cfg(unix)'.dependencies]\nlibc = \"0.2\"\n").contains("libc"));
    }

    #[test]
    fn reads_key_and_package_name_for_renamed_dependency() {
        let dependencies =
            names("[dependencies]\ndatabase = { package = \"sqlx\", version = \"0.8\" }\n");
        assert!(dependencies.contains("database"));
        assert!(dependencies.contains("sqlx"));
    }

    #[test]
    fn ignores_development_dependencies() {
        assert!(!names("[dev-dependencies]\nsqlx = \"0.8\"\n").contains("sqlx"));
    }
}
