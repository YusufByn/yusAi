//! Les skills de yusAi, par niveau, dans les données de l'app :
//! `<données>/prime-skills/{projects/<hash du chemin>, types/<hash du type>,
//! global}/<skill>/SKILL.md`, plus `archive/` hors du relevé. Chaque dossier
//! de projet ou de type porte un fichier `.yusai-owner` (son chemin ou son
//! nom de type), pour nommer son propriétaire dans les messages.
//!
//! Au `Create`, `config.skills` reçoit chaque `SKILL.md` un par un, projet
//! puis type confirmé puis global : Prime place ces chemins avant ses propres
//! sources (pa-core/src/resources/resolution.rs:208-212) et garde la première
//! skill d'un nom (pa-core/src/skills/loader.rs:93-104), donc une skill du
//! projet masque une skill globale du même nom. Un chemin de fichier garde la
//! détection du paquet Python, faite sur son dossier
//! (pa-core/src/skills/discovery.rs:84-122). Le `Create` durable rejoue
//! `skills` (pa-daemon/src/supervisor/worker_lifecycle.rs:237-249).
//!
//! Conflit Python : tous les noyaux partagent un venv, où chaque skill Python
//! est installée en éditable sous son nom d'import et son chemin
//! (pa-core/src/kernel/bootstrap/venv/version.rs:24-50, venv.rs:179-235).
//! Deux dossiers qui portent le même nom d'import (ou la même distribution
//! dans leur `pyproject.toml`) se remplacent l'un l'autre sans bruit. Une
//! skill Python de yusAi doit donc être seule sur ses noms dans tout
//! `prime-skills/` (archive exclue) et parmi les skills que Prime charge
//! lui-même pour le projet : `<agent_dir>/skills/`, `~/.agents/skills/`, les
//! skills intégrées, `.prime/agent/skills/` et les `.agents/skills/` du
//! projet et de ses parents jusqu'à la racine git, les tableaux `skills` des
//! réglages et les paquets configurés (pa-core/src/packages/resolve/auto.rs:24-140,
//! manager.rs:142-191). Une skill de Prime gagne toujours (on ne la contrôle
//! pas) ; entre les nôtres, le niveau le plus haut gagne (global, type,
//! projet), puis le dossier le plus ancien. Une perdante n'est pas passée au
//! `Create` : jamais installée, elle ne déloge personne.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use pa_core::packages::{
    BundledSkillsDir, MissingSourceAction, PackageManager, PackageManagerOptions,
};
use pa_core::settings::SettingsManager;
use pa_core::skills::{load_skills, load_skills_from_dir, LoadSkillsOptions, Skill};
use serde::Serialize;
use sinew_app::store::{normalize_project_type, LessonLevel};

const SKILLS_DIR: &str = "prime-skills";
const OWNER_FILE: &str = ".yusai-owner";

/// Les dossiers qui décident des skills d'une session.
#[derive(Debug, Clone)]
pub struct SkillSources {
    /// Les données de yusAi (parent de `prime-skills/`).
    pub data_dir: PathBuf,
    /// Le dossier d'état de Prime (ses `skills/` et ses réglages).
    pub agent_dir: PathBuf,
    /// Les ressources packagées de Prime (ses skills intégrées).
    pub package_dir: PathBuf,
}

impl SkillSources {
    /// Les dossiers de l'app.
    pub fn app() -> Self {
        Self {
            data_dir: crate::prime::data_dir(),
            agent_dir: crate::prime::agent_dir(),
            package_dir: crate::prime::package_dir(),
        }
    }
}

/// `<données>/prime-skills`.
pub fn skills_root(data_dir: &Path) -> PathBuf {
    data_dir.join(SKILLS_DIR)
}

/// Le dossier des skills d'un projet, où `skill-creator` doit écrire.
pub fn project_skills_dir(data_dir: &Path, workspace_id: &str) -> PathBuf {
    skills_root(data_dir)
        .join("projects")
        .join(pa_daemon::paths::hash_key(workspace_id, 16))
}

/// Le dossier des skills d'un type, nommé par son nom normalisé (« CLI
/// Rust » et « cli rust » sont le même type, comme pour les leçons).
pub fn type_skills_dir(data_dir: &Path, project_type: &str) -> PathBuf {
    skills_root(data_dir)
        .join("types")
        .join(pa_daemon::paths::hash_key(
            &normalize_project_type(project_type),
            16,
        ))
}

/// Le dossier des skills globales.
pub fn global_skills_dir(data_dir: &Path) -> PathBuf {
    skills_root(data_dir).join("global")
}

/// Crée le dossier d'un projet ou d'un type avec son fichier de
/// propriétaire.
pub fn ensure_owner_dir(dir: &Path, owner: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let marker = dir.join(OWNER_FILE);
    if std::fs::read_to_string(&marker).ok().as_deref() != Some(owner) {
        std::fs::write(marker, owner)?;
    }
    Ok(())
}

/// Les noms qu'une skill Python occupe dans le venv partagé.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonNames {
    pub import_name: String,
    /// Le `name` de `[project]` du `pyproject.toml`, normalisé.
    pub distribution: String,
}

impl PythonNames {
    fn shares_with(&self, other: &PythonNames) -> Option<String> {
        if self.import_name == other.import_name {
            Some(format!("nom d'import Python `{}`", self.import_name))
        } else if self.distribution == other.distribution {
            Some(format!("distribution Python `{}`", self.distribution))
        } else {
            None
        }
    }
}

/// Une skill d'un dossier de yusAi.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct YusaiSkill {
    pub name: String,
    pub description: String,
    pub level: LessonLevel,
    /// Le chemin du projet ou le nom du type ; `None` pour le global (ou un
    /// dossier sans fichier de propriétaire).
    pub owner: Option<String>,
    /// Le dossier de la skill.
    pub dir: PathBuf,
    /// Son `SKILL.md`.
    pub file: PathBuf,
    pub python: Option<PythonNames>,
    pub created_ms: u64,
}

/// Une skill de yusAi écartée d'une session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisabledSkill {
    pub skill: YusaiSkill,
    pub reason: String,
}

/// Les skills de yusAi pour une session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSkills {
    /// Les `SKILL.md` de `config.skills`, dans l'ordre.
    pub files: Vec<String>,
    /// Les skills des niveaux de la session laissées dehors.
    pub disabled: Vec<DisabledSkill>,
}

/// Toutes les skills de yusAi, archive exclue.
pub fn our_skills(data_dir: &Path) -> Vec<YusaiSkill> {
    let root = skills_root(data_dir);
    let mut skills = skills_in(&root.join("global"), LessonLevel::Global, None);
    for (group, level) in [
        ("types", LessonLevel::Type),
        ("projects", LessonLevel::Project),
    ] {
        for dir in subdirs(&root.join(group)) {
            let owner = std::fs::read_to_string(dir.join(OWNER_FILE))
                .ok()
                .map(|owner| owner.trim().to_string())
                .filter(|owner| !owner.is_empty());
            skills.extend(skills_in(&dir, level, owner));
        }
    }
    skills
}

/// Les skills d'un dossier de niveau : chaque `SKILL.md` que Prime y
/// trouverait (pa-core/src/skills/discovery.rs:225-330), sans les `.md` à
/// la racine.
pub fn skills_in(dir: &Path, level: LessonLevel, owner: Option<String>) -> Vec<YusaiSkill> {
    let mut skills: Vec<YusaiSkill> = load_skills_from_dir(dir, "path")
        .skills
        .into_iter()
        .filter(|skill| {
            skill
                .file_path
                .file_name()
                .is_some_and(|name| name == "SKILL.md")
        })
        .map(|skill| {
            let dir = skill.base_dir.clone();
            YusaiSkill {
                python: python_names(&skill),
                name: skill.name,
                description: skill.description,
                level,
                owner: owner.clone(),
                created_ms: created_ms(&dir),
                file: skill.file_path,
                dir,
            }
        })
        .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name).then(a.dir.cmp(&b.dir)));
    skills
}

/// Les skills que Prime charge lui-même pour un projet, sans les nôtres :
/// la résolution de ses ressources, sans installer de paquet manquant
/// (pa-core/src/packages/resolve/manager.rs:142-191, 279-290).
pub fn prime_skills(sources: &SkillSources, cwd: &Path) -> Vec<Skill> {
    let mut manager = PackageManager::with_options(PackageManagerOptions {
        cwd: cwd.to_path_buf(),
        agent_dir: sources.agent_dir.clone(),
        settings: SettingsManager::create(cwd, &sources.agent_dir),
        bundled_skills_dir: BundledSkillsDir::Directory(sources.package_dir.join("skills")),
        extra_builtin_skill_overrides: Vec::new(),
    });
    let mut skip = |_: &str| MissingSourceAction::Skip;
    let resolved = match manager.resolve_with_on_missing(Some(&mut skip)) {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::warn!(error = %error, "prime skill resolution failed");
            return Vec::new();
        }
    };
    let ours = skills_root(&sources.data_dir);
    // Un dossier de skill vaut son `SKILL.md`
    // (pa-core/src/resources/resolution.rs:88-105).
    let skill_paths = resolved
        .skills
        .into_iter()
        .filter(|resource| resource.enabled)
        .map(|resource| {
            let skill_file = resource.path.join("SKILL.md");
            if resource.path.is_dir() && skill_file.is_file() {
                skill_file
            } else {
                resource.path
            }
        })
        .filter(|path| !path.starts_with(&ours))
        .map(|path| path.display().to_string())
        .collect();
    load_skills(&LoadSkillsOptions {
        cwd: cwd.to_path_buf(),
        agent_dir: sources.agent_dir.clone(),
        skill_paths,
        include_defaults: false,
    })
    .skills
}

/// Les skills de yusAi d'une session du projet `workspace_id`, de type
/// confirmé `project_type`. Crée le dossier du projet au passage, pour
/// `skill-creator`.
pub fn thread_skills(
    sources: &SkillSources,
    workspace_id: &str,
    project_type: Option<&str>,
) -> ThreadSkills {
    let project_dir = project_skills_dir(&sources.data_dir, workspace_id);
    if let Err(error) = ensure_owner_dir(&project_dir, workspace_id) {
        tracing::warn!(error = %error, "prime project skills folder not created");
    }
    let ours = our_skills(&sources.data_dir);
    let prime = prime_skills(sources, Path::new(workspace_id));
    let mut chain = vec![project_dir];
    if let Some(project_type) = project_type {
        chain.push(type_skills_dir(&sources.data_dir, project_type));
    }
    chain.push(global_skills_dir(&sources.data_dir));
    select_skills(&ours, &prime, &chain)
}

/// La sélection, sans effet : les skills de `ours` rangées dans `chain`
/// (dans l'ordre de `chain`), moins celles qui perdent un conflit Python.
pub fn select_skills(ours: &[YusaiSkill], prime: &[Skill], chain: &[PathBuf]) -> ThreadSkills {
    let mut selected = ThreadSkills::default();
    for level_dir in chain {
        for skill in ours
            .iter()
            .filter(|skill| skill.dir.parent() == Some(level_dir.as_path()))
        {
            match conflict(skill, ours, prime) {
                Some(reason) => selected.disabled.push(DisabledSkill {
                    skill: skill.clone(),
                    reason,
                }),
                None => selected.files.push(skill.file.display().to_string()),
            }
        }
    }
    selected
}

/// Pourquoi une skill de yusAi perd un conflit Python, s'il y en a un.
pub fn conflict(skill: &YusaiSkill, ours: &[YusaiSkill], prime: &[Skill]) -> Option<String> {
    let names = skill.python.as_ref()?;
    for other in prime {
        let (Some(other_names), Some(python)) = (python_names(other), other.python.as_ref()) else {
            continue;
        };
        if same_dir(&python.package_path, &skill.dir) {
            continue;
        }
        if let Some(shared) = names.shares_with(&other_names) {
            return Some(format!(
                "{shared} déjà pris par la skill `{}` de Prime ({})",
                other.name,
                python.package_path.display()
            ));
        }
    }
    ours.iter()
        .filter(|other| !same_dir(&other.dir, &skill.dir) && outranks(other, skill))
        .find_map(|other| {
            let shared = names.shares_with(other.python.as_ref()?)?;
            Some(format!(
                "{shared} déjà pris par la skill `{}` {}",
                other.name,
                place(other)
            ))
        })
}

/// `a` passe avant `b` : niveau plus haut, puis dossier plus ancien.
fn outranks(a: &YusaiSkill, b: &YusaiSkill) -> bool {
    let rank = |skill: &YusaiSkill| {
        let level = match skill.level {
            LessonLevel::Global => 0,
            LessonLevel::Type => 1,
            LessonLevel::Project => 2,
        };
        (level, skill.created_ms, skill.dir.clone())
    };
    rank(a) < rank(b)
}

/// « du niveau global », « du type rust », « du projet /chemin ».
pub fn place(skill: &YusaiSkill) -> String {
    match (skill.level, skill.owner.as_deref()) {
        (LessonLevel::Global, _) => "du niveau global".to_string(),
        (LessonLevel::Type, Some(owner)) => format!("du type {owner}"),
        (LessonLevel::Type, None) => "d'un type".to_string(),
        (LessonLevel::Project, Some(owner)) => format!("du projet {owner}"),
        (LessonLevel::Project, None) => "d'un projet".to_string(),
    }
}

fn python_names(skill: &Skill) -> Option<PythonNames> {
    let python = skill.python.as_ref()?;
    let distribution = distribution_name(&python.pyproject_path)
        .unwrap_or_else(|| python.import_name.replace('_', "-"));
    Some(PythonNames {
        import_name: python.import_name.clone(),
        distribution: normalize_distribution(&distribution),
    })
}

/// Le `name` de la section `[project]`, comme le lit Prime
/// (pa-core/src/kernel/bootstrap/venv/skills.rs:23-62).
fn distribution_name(pyproject: &Path) -> Option<String> {
    let text = std::fs::read_to_string(pyproject).ok()?;
    let mut in_project = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_project = trimmed == "[project]";
            continue;
        }
        if !in_project {
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("name")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
        {
            let value = rest.trim().trim_matches(|c| c == '"' || c == '\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Nom de distribution normalisé (PEP 503) : `My_Skill.x` = `my-skill-x`.
fn normalize_distribution(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    for ch in name.chars() {
        if matches!(ch, '-' | '_' | '.') {
            if !normalized.ends_with('-') {
                normalized.push('-');
            }
        } else {
            normalized.extend(ch.to_lowercase());
        }
    }
    normalized
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(a) == canonical(b)
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn created_ms(dir: &Path) -> u64 {
    std::fs::metadata(dir)
        .and_then(|meta| meta.created().or_else(|_| meta.modified()))
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "yusai-skills-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Une skill markdown, ou Python si `distribution` est donné.
    fn write_skill(level_dir: &Path, name: &str, distribution: Option<&str>) -> PathBuf {
        let dir = level_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Skill {name}.\n---\n\nCorps.\n"),
        )
        .unwrap();
        if let Some(distribution) = distribution {
            let import = name.replace('-', "_");
            std::fs::write(
                dir.join("pyproject.toml"),
                format!("[project]\nname = \"{distribution}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            let package = dir.join("src").join(&import);
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(package.join("__init__.py"), "def run():\n    return 1\n").unwrap();
        }
        dir
    }

    fn sources(root: &Path) -> SkillSources {
        SkillSources {
            data_dir: root.join("data"),
            agent_dir: root.join("agent"),
            // Pas de skills intégrées : seules celles des tests comptent.
            package_dir: root.join("package"),
        }
    }

    fn names(files: &[String]) -> Vec<String> {
        files
            .iter()
            .map(|file| {
                let path = Path::new(file);
                let dir = path.parent().unwrap();
                let level = dir.parent().unwrap();
                let group = if level.ends_with("global") {
                    "global".to_string()
                } else {
                    level
                        .parent()
                        .unwrap()
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                };
                format!("{group}/{}", dir.file_name().unwrap().to_string_lossy())
            })
            .collect()
    }

    #[test]
    fn a_type_folder_ignores_case_and_surrounding_spaces() {
        let data = Path::new("/data");
        assert_eq!(
            type_skills_dir(data, "CLI Rust"),
            type_skills_dir(data, " cli rust ")
        );
        assert_ne!(
            type_skills_dir(data, "rust"),
            type_skills_dir(data, "python")
        );
        assert!(type_skills_dir(data, "rust").starts_with("/data/prime-skills/types"));
        let a = project_skills_dir(data, "/work/a");
        assert_eq!(a, project_skills_dir(data, "/work/a"));
        assert_ne!(a, project_skills_dir(data, "/work/b"));
        assert!(a.starts_with("/data/prime-skills/projects"));
    }

    #[test]
    fn a_thread_gets_project_then_confirmed_type_then_global() {
        let root = scratch();
        let sources = sources(&root);
        let data = &sources.data_dir;
        let workspace = root.join("work-a");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace_id = workspace.to_string_lossy().into_owned();
        write_skill(&global_skills_dir(data), "notes", None);
        write_skill(&global_skills_dir(data), "commit-style", None);
        let rust = type_skills_dir(data, "rust");
        ensure_owner_dir(&rust, "rust").unwrap();
        write_skill(&rust, "clippy-fix", None);
        let project = project_skills_dir(data, &workspace_id);
        write_skill(&project, "notes", None);
        // Un autre projet, un autre type, l'archive : jamais passés.
        write_skill(&project_skills_dir(data, "/work/b"), "other", None);
        write_skill(&type_skills_dir(data, "python"), "pytest", None);
        write_skill(&skills_root(data).join("archive"), "old", None);

        let typed = thread_skills(&sources, &workspace_id, Some("Rust"));
        assert_eq!(
            names(&typed.files),
            vec![
                "projects/notes".to_string(),
                "types/clippy-fix".to_string(),
                "global/commit-style".to_string(),
                "global/notes".to_string(),
            ]
        );
        assert!(typed.disabled.is_empty());
        // Sans type confirmé, pas de skills de type.
        let untyped = thread_skills(&sources, &workspace_id, None);
        assert_eq!(untyped.files.len(), 3);
        assert!(!untyped.files.iter().any(|file| file.contains("clippy-fix")));
        // Le dossier du projet existe et nomme son projet.
        assert_eq!(
            std::fs::read_to_string(project.join(OWNER_FILE)).unwrap(),
            workspace_id
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_python_name_has_one_owner_the_highest_level_then_the_oldest() {
        let root = scratch();
        let sources = sources(&root);
        let data = &sources.data_dir;
        let workspace_id = root.join("work-a").to_string_lossy().into_owned();
        let other_id = root.join("work-b").to_string_lossy().into_owned();
        let project = project_skills_dir(data, &workspace_id);
        ensure_owner_dir(&project, &workspace_id).unwrap();
        let other = project_skills_dir(data, &other_id);
        ensure_owner_dir(&other, &other_id).unwrap();

        // Même nom d'import au projet et au global : le global gagne.
        write_skill(&project, "fmt", Some("fmt"));
        write_skill(&global_skills_dir(data), "fmt", Some("fmt"));
        // Même distribution, noms d'import différents, deux projets : le plus
        // ancien gagne.
        write_skill(&other, "lint-old", Some("shared-lint"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_skill(&project, "lint-new", Some("Shared_Lint"));
        // Une skill markdown du même nom qu'une Python : pas de conflit.
        write_skill(&other, "fmt", None);

        let skills = thread_skills(&sources, &workspace_id, None);
        let disabled: Vec<(&str, &str)> = skills
            .disabled
            .iter()
            .map(|disabled| (disabled.skill.name.as_str(), disabled.reason.as_str()))
            .collect();
        assert_eq!(disabled.len(), 2, "{disabled:?}");
        assert_eq!(disabled[0].0, "fmt");
        assert!(
            disabled[0].1.contains("nom d'import Python `fmt`"),
            "{}",
            disabled[0].1
        );
        assert!(
            disabled[0].1.contains("du niveau global"),
            "{}",
            disabled[0].1
        );
        assert_eq!(disabled[1].0, "lint-new");
        assert!(
            disabled[1].1.contains("distribution Python `shared-lint`"),
            "{}",
            disabled[1].1
        );
        assert!(
            disabled[1].1.contains(&format!("du projet {other_id}")),
            "{}",
            disabled[1].1
        );
        assert_eq!(names(&skills.files), vec!["global/fmt".to_string()]);
        // Le projet b garde ses deux skills.
        let b = thread_skills(&sources, &other_id, None);
        assert!(b.disabled.is_empty(), "{:?}", b.disabled);
        assert_eq!(b.files.len(), 3);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_skill_of_prime_keeps_its_python_names() {
        let root = scratch();
        let sources = sources(&root);
        let data = &sources.data_dir;
        let workspace = root.join("work-a");
        let workspace_id = workspace.to_string_lossy().into_owned();
        // Les sources de Prime : `<agent_dir>/skills/`, les skills intégrées,
        // `.prime/agent/skills/` et `.agents/skills/` du dépôt ouvert.
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        write_skill(
            &sources.agent_dir.join("skills"),
            "agent-tool",
            Some("agent-tool"),
        );
        write_skill(
            &sources.package_dir.join("skills"),
            "websearch",
            Some("websearch"),
        );
        write_skill(
            &workspace.join(".prime/agent/skills"),
            "repo-tool",
            Some("repo-tool"),
        );
        write_skill(
            &workspace.join(".agents/skills"),
            "agents-tool",
            Some("agents-tool"),
        );
        let prime: Vec<String> = prime_skills(&sources, &workspace)
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        for name in ["agent-tool", "websearch", "repo-tool", "agents-tool"] {
            assert!(
                prime.contains(&name.to_string()),
                "{name} missing from {prime:?}"
            );
        }

        let project = project_skills_dir(data, &workspace_id);
        ensure_owner_dir(&project, &workspace_id).unwrap();
        let global = global_skills_dir(data);
        for name in ["agent-tool", "repo-tool", "agents-tool"] {
            write_skill(&project, name, Some(&format!("yusai-{name}")));
        }
        write_skill(&global, "websearch", Some("other-websearch"));
        write_skill(&global, "free-tool", Some("free-tool"));

        let skills = thread_skills(&sources, &workspace_id, None);
        let mut disabled: Vec<&str> = skills
            .disabled
            .iter()
            .map(|disabled| disabled.skill.name.as_str())
            .collect();
        disabled.sort_unstable();
        assert_eq!(
            disabled,
            vec!["agent-tool", "agents-tool", "repo-tool", "websearch"]
        );
        assert!(skills.disabled[0].reason.contains("de Prime"));
        assert_eq!(names(&skills.files), vec!["global/free-tool".to_string()]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn distribution_names_are_normalized() {
        assert_eq!(normalize_distribution("My_Skill.x"), "my-skill-x");
        assert_eq!(normalize_distribution("a--b__c"), "a-b-c");
        let root = scratch();
        let pyproject = root.join("pyproject.toml");
        std::fs::write(
            &pyproject,
            "[build-system]\nname = \"no\"\n[project]\nversion = \"1\"\nname = 'word-count'\n[tool.x]\nname = \"no\"\n",
        )
        .unwrap();
        assert_eq!(distribution_name(&pyproject).as_deref(), Some("word-count"));
        let _ = std::fs::remove_dir_all(root);
    }
}
