//! Le type d'un projet, pour les leçons de niveau type : une suggestion
//! tirée des fichiers à la racine du projet, que l'utilisateur confirme ou
//! change dans le sélecteur du chat Prime (noms libres, retenus par projet
//! dans `prime_projects`). Une suggestion n'écrase jamais un choix de
//! l'utilisateur (`AppStore::set_project_type`).

use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use sinew_app::store::{AppStore, ProjectTypeSource};

/// Le type du projet tel que le sélecteur l'affiche.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimeProjectType {
    /// `None` : pas de type (aucune suggestion, ou « aucun » choisi).
    pub project_type: Option<String>,
    /// `None` tant que rien n'est enregistré.
    pub source: Option<ProjectTypeSource>,
    /// Les types déjà connus, proposés dans le sélecteur.
    pub known_types: Vec<String>,
}

/// Le type d'un projet. Tant que l'utilisateur n'a pas choisi, la
/// suggestion est recalculée depuis les fichiers (un `Cargo.toml` ajouté
/// change la suggestion) et enregistrée.
pub fn project_type(store: &AppStore, workspace_id: &str) -> Result<PrimeProjectType> {
    let current = store.project_type(workspace_id)?;
    let setting = match current {
        Some(setting) if setting.source == ProjectTypeSource::User => setting,
        current => {
            let suggested = suggest_project_type(Path::new(workspace_id));
            if current
                .as_ref()
                .is_some_and(|setting| setting.project_type == suggested)
            {
                current.expect("checked above")
            } else {
                store.set_project_type(
                    workspace_id,
                    suggested.as_deref(),
                    ProjectTypeSource::Suggested,
                )?
            }
        }
    };
    Ok(PrimeProjectType {
        project_type: setting.project_type,
        source: Some(setting.source),
        known_types: store.known_project_types()?,
    })
}

/// Le choix de l'utilisateur ; `None` : pas de type.
pub fn set_project_type(
    store: &AppStore,
    workspace_id: &str,
    project_type: Option<&str>,
) -> Result<PrimeProjectType> {
    let setting = store.set_project_type(workspace_id, project_type, ProjectTypeSource::User)?;
    Ok(PrimeProjectType {
        project_type: setting.project_type,
        source: Some(setting.source),
        known_types: store.known_project_types()?,
    })
}

/// Le type suggéré par les fichiers à la racine du projet, du plus
/// spécifique au plus général ; `None` si rien n'est reconnu.
pub fn suggest_project_type(root: &Path) -> Option<String> {
    let has = |name: &str| root.join(name).exists();
    if has("src-tauri/tauri.conf.json") || has("tauri.conf.json") {
        return Some("tauri".to_string());
    }
    if has("Cargo.toml") {
        return Some("rust".to_string());
    }
    if has("package.json") {
        return Some(node_project_type(root));
    }
    if has("pyproject.toml") || has("setup.py") || has("requirements.txt") {
        return Some("python".to_string());
    }
    let simple = [
        ("go.mod", "go"),
        ("Package.swift", "swift"),
        ("pubspec.yaml", "flutter"),
        ("build.gradle.kts", "kotlin"),
        ("build.gradle", "java"),
        ("pom.xml", "java"),
        ("Gemfile", "ruby"),
        ("composer.json", "php"),
        ("mix.exs", "elixir"),
        ("CMakeLists.txt", "cpp"),
    ];
    if let Some((_, name)) = simple.iter().find(|(file, _)| has(file)) {
        return Some((*name).to_string());
    }
    let extensions = [
        ("xcodeproj", "swift"),
        ("csproj", "dotnet"),
        ("sln", "dotnet"),
    ];
    let entries: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    extensions
        .iter()
        .find(|(extension, _)| {
            entries
                .iter()
                .any(|name| name.ends_with(&format!(".{extension}")))
        })
        .map(|(_, name)| (*name).to_string())
}

/// Un projet `package.json` : son framework s'il en a un connu, sinon
/// `typescript` ou `node`.
fn node_project_type(root: &Path) -> String {
    let manifest: Value = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    let depends_on = |name: &str| {
        ["dependencies", "devDependencies"]
            .iter()
            .any(|section| manifest[section].get(name).is_some())
    };
    let frameworks = [
        ("next", "nextjs"),
        ("electron", "electron"),
        ("react-native", "react-native"),
        ("@angular/core", "angular"),
        ("svelte", "svelte"),
        ("vue", "vue"),
        ("react", "react"),
    ];
    if let Some((_, name)) = frameworks
        .iter()
        .find(|(dependency, _)| depends_on(dependency))
    {
        return (*name).to_string();
    }
    if root.join("tsconfig.json").exists() {
        "typescript".to_string()
    } else {
        "node".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "yusai-project-type-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for (path, content) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        root
    }

    fn suggested(files: &[(&str, &str)]) -> Option<String> {
        let root = project(files);
        let suggestion = suggest_project_type(&root);
        let _ = std::fs::remove_dir_all(root);
        suggestion
    }

    #[test]
    fn manifests_suggest_a_type() {
        assert_eq!(suggested(&[]), None);
        assert_eq!(suggested(&[("README.md", "")]), None);
        assert_eq!(suggested(&[("Cargo.toml", "")]).as_deref(), Some("rust"));
        assert_eq!(
            suggested(&[
                ("Cargo.toml", ""),
                ("src-tauri/tauri.conf.json", "{}"),
                ("package.json", "{}")
            ])
            .as_deref(),
            Some("tauri")
        );
        assert_eq!(
            suggested(&[("pyproject.toml", "")]).as_deref(),
            Some("python")
        );
        assert_eq!(
            suggested(&[("requirements.txt", "")]).as_deref(),
            Some("python")
        );
        assert_eq!(suggested(&[("go.mod", "")]).as_deref(), Some("go"));
        assert_eq!(
            suggested(&[("build.gradle.kts", "")]).as_deref(),
            Some("kotlin")
        );
        assert_eq!(
            suggested(&[("App.xcodeproj/project.pbxproj", "")]).as_deref(),
            Some("swift")
        );
        assert_eq!(suggested(&[("Tool.csproj", "")]).as_deref(), Some("dotnet"));
    }

    #[test]
    fn package_json_names_its_framework() {
        let package = |json: &str| suggested(&[("package.json", json)]);
        assert_eq!(package("{}").as_deref(), Some("node"));
        assert_eq!(package("pas du json").as_deref(), Some("node"));
        assert_eq!(
            package(r#"{"dependencies":{"react":"18","next":"14"}}"#).as_deref(),
            Some("nextjs")
        );
        assert_eq!(
            package(r#"{"devDependencies":{"react":"18"}}"#).as_deref(),
            Some("react")
        );
        assert_eq!(
            package(r#"{"dependencies":{"vue":"3"}}"#).as_deref(),
            Some("vue")
        );
        assert_eq!(
            suggested(&[("package.json", "{}"), ("tsconfig.json", "{}")]).as_deref(),
            Some("typescript")
        );
    }

    #[test]
    fn a_suggestion_follows_the_files_until_the_user_chooses() {
        let root = project(&[("package.json", "{}")]);
        let workspace_id = root.to_string_lossy().into_owned();
        let store = AppStore::open_at(root.join("state.sqlite3")).unwrap();

        let shown = project_type(&store, &workspace_id).unwrap();
        assert_eq!(shown.project_type.as_deref(), Some("node"));
        assert_eq!(shown.source, Some(ProjectTypeSource::Suggested));
        assert_eq!(shown.known_types, vec!["node"]);
        // Un Cargo.toml arrive : la suggestion suit.
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        assert_eq!(
            project_type(&store, &workspace_id)
                .unwrap()
                .project_type
                .as_deref(),
            Some("rust")
        );
        // L'utilisateur choisit : plus rien ne bouge.
        let chosen = set_project_type(&store, &workspace_id, Some("CLI Rust")).unwrap();
        assert_eq!(chosen.source, Some(ProjectTypeSource::User));
        std::fs::write(root.join("pyproject.toml"), "").unwrap();
        let shown = project_type(&store, &workspace_id).unwrap();
        assert_eq!(shown.project_type.as_deref(), Some("CLI Rust"));
        assert_eq!(shown.known_types, vec!["CLI Rust"]);
        // « Aucun type » est aussi un choix.
        let none = set_project_type(&store, &workspace_id, None).unwrap();
        assert_eq!(none.project_type, None);
        assert_eq!(
            project_type(&store, &workspace_id).unwrap().project_type,
            None
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
