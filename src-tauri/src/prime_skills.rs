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
//!
//! Ce que fait yusAi lui-même (écrire une skill acceptée, changer de
//! niveau, restaurer) est refusé si le nom est déjà pris, à ce niveau ou en
//! Python n'importe où : pas de nouveau conflit de notre fait. Une skill
//! écrite par l'acceptation d'une proposition porte l'id de la proposition
//! dans son front matter (`metadata.yusai-proposal`), qui la retrouve après
//! un déplacement (« Undo » d'une refine). L'archive range une skill sous
//! `archive/<date>-<nom>/`, avec `.yusai-archived` (son ancien dossier).

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, bail, Context, Result};

use pa_core::packages::{
    BundledSkillsDir, MissingSourceAction, PackageManager, PackageManagerOptions,
};
use pa_core::settings::SettingsManager;
use pa_core::skills::frontmatter::parse_frontmatter;
use pa_core::skills::{load_skills, load_skills_from_dir, LoadSkillsOptions, Skill};
use serde::{Deserialize, Serialize};
use sinew_app::store::{normalize_project_type, LessonLevel};

const SKILLS_DIR: &str = "prime-skills";
const OWNER_FILE: &str = ".yusai-owner";
const ARCHIVED_FILE: &str = ".yusai-archived";
/// La clé du front matter qui lie une skill à la proposition acceptée.
const PROPOSAL_KEY: &str = "yusai-proposal";

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

/// Le dossier des skills archivées, hors du relevé.
pub fn archive_dir(data_dir: &Path) -> PathBuf {
    skills_root(data_dir).join("archive")
}

/// Le dossier d'un niveau vu du projet `workspace_id` (de type confirmé
/// `project_type`), créé avec son propriétaire.
pub fn level_dir(
    data_dir: &Path,
    level: LessonLevel,
    workspace_id: &str,
    project_type: Option<&str>,
) -> Result<PathBuf> {
    let (dir, owner) = match level {
        LessonLevel::Project => (
            project_skills_dir(data_dir, workspace_id),
            Some(workspace_id),
        ),
        LessonLevel::Type => {
            let project_type =
                project_type.ok_or_else(|| anyhow!("choose the project's type first"))?;
            (type_skills_dir(data_dir, project_type), Some(project_type))
        }
        LessonLevel::Global => (global_skills_dir(data_dir), None),
    };
    match owner {
        Some(owner) => ensure_owner_dir(&dir, owner),
        None => std::fs::create_dir_all(&dir),
    }
    .with_context(|| format!("unable to create {}", dir.display()))?;
    Ok(dir)
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

/// Le nom Python que deux skills se disputent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "name")]
pub enum SharedName {
    Import(String),
    Distribution(String),
    /// Le nom de skill d'une skill Python qu'elle masquerait : Prime garde
    /// la première skill d'un nom (pa-core/src/skills/loader.rs:93-104) et
    /// ne pré-importe dans le noyau que les skills Python restées
    /// (pa-core/src/session_engine/engine.rs:318), la fonction disparaîtrait.
    Skill(String),
}

impl PythonNames {
    fn shares_with(&self, other: &PythonNames) -> Option<SharedName> {
        if self.import_name == other.import_name {
            Some(SharedName::Import(self.import_name.clone()))
        } else if self.distribution == other.distribution {
            Some(SharedName::Distribution(self.distribution.clone()))
        } else {
            None
        }
    }
}

/// Une skill Python qui en déloge une autre, ou la refuse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    pub shared: SharedName,
    /// Le nom de la skill qui garde ce nom.
    pub holder: String,
    /// Son niveau chez yusAi ; `None` : une skill de Prime.
    pub holder_level: Option<LessonLevel>,
    /// Son projet ou son type chez yusAi.
    pub holder_owner: Option<String>,
    pub holder_dir: PathBuf,
}

impl Conflict {
    fn with_ours(shared: SharedName, holder: &YusaiSkill) -> Self {
        Self {
            shared,
            holder: holder.name.clone(),
            holder_level: Some(holder.level),
            holder_owner: holder.owner.clone(),
            holder_dir: holder.dir.clone(),
        }
    }

    /// Pour le modèle : « nom d'import Python `fmt` déjà pris par la skill
    /// `fmt` du niveau global ».
    pub fn french(&self) -> String {
        let shared = match &self.shared {
            SharedName::Import(name) => format!("nom d'import Python `{name}`"),
            SharedName::Distribution(name) => format!("distribution Python `{name}`"),
            SharedName::Skill(name) => format!("nom `{name}`"),
        };
        let place = match (self.holder_level, self.holder_owner.as_deref()) {
            (None, _) => format!("de Prime ({})", self.holder_dir.display()),
            (Some(LessonLevel::Global), _) => "du niveau global".to_string(),
            (Some(LessonLevel::Type), Some(owner)) => format!("du type {owner}"),
            (Some(LessonLevel::Type), None) => "d'un type".to_string(),
            (Some(LessonLevel::Project), Some(owner)) => format!("du projet {owner}"),
            (Some(LessonLevel::Project), None) => "d'un projet".to_string(),
        };
        match &self.shared {
            SharedName::Skill(_) => format!(
                "{shared} déjà pris par la skill Python `{}` {place} : elle la masquerait et sa fonction disparaîtrait du noyau",
                self.holder
            ),
            _ => format!("{shared} déjà pris par la skill `{}` {place}", self.holder),
        }
    }

    /// Pour la vue et les erreurs : « Python import name `fmt` is taken by
    /// the global skill `fmt` ».
    pub fn english(&self) -> String {
        let shared = match &self.shared {
            SharedName::Import(name) => format!("Python import name `{name}`"),
            SharedName::Distribution(name) => format!("Python distribution `{name}`"),
            SharedName::Skill(name) => format!("Skill name `{name}`"),
        };
        let holder = match (self.holder_level, self.holder_owner.as_deref()) {
            (None, _) => format!(
                "the Prime skill `{}` ({})",
                self.holder,
                self.holder_dir.display()
            ),
            (Some(LessonLevel::Global), _) => format!("the global skill `{}`", self.holder),
            (Some(LessonLevel::Type), Some(owner)) => {
                format!("the skill `{}` of type {owner}", self.holder)
            }
            (Some(LessonLevel::Type), None) => format!("the type skill `{}`", self.holder),
            (Some(LessonLevel::Project), Some(owner)) => {
                format!("the skill `{}` of project {owner}", self.holder)
            }
            (Some(LessonLevel::Project), None) => format!("the project skill `{}`", self.holder),
        };
        match &self.shared {
            SharedName::Skill(_) => format!(
                "{shared} is taken by {holder}, a Python skill this one would hide (its function would leave the kernel)"
            ),
            _ => format!("{shared} is taken by {holder}"),
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
    /// La proposition dont l'acceptation l'a écrite.
    pub proposal_id: Option<String>,
}

/// Une skill de yusAi écartée d'une session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisabledSkill {
    pub skill: YusaiSkill,
    pub conflict: Conflict,
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
                proposal_id: proposal_id(&skill.file_path),
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
    let chain = chain_dirs(&sources.data_dir, workspace_id, project_type);
    select_skills(&ours, &prime, &chain)
}

/// Les dossiers de niveau d'un projet, dans l'ordre de `config.skills` :
/// projet, type confirmé, global.
pub fn chain_dirs(data_dir: &Path, workspace_id: &str, project_type: Option<&str>) -> Vec<PathBuf> {
    let mut chain = vec![project_skills_dir(data_dir, workspace_id)];
    if let Some(project_type) = project_type {
        chain.push(type_skills_dir(data_dir, project_type));
    }
    chain.push(global_skills_dir(data_dir));
    chain
}

/// La sélection, sans effet : les skills de `ours` rangées dans `chain`
/// (dans l'ordre de `chain`), moins celles qui perdent un conflit Python.
pub fn select_skills(ours: &[YusaiSkill], prime: &[Skill], chain: &[PathBuf]) -> ThreadSkills {
    let mut selected = ThreadSkills::default();
    for (index, level_dir) in chain.iter().enumerate() {
        for skill in ours
            .iter()
            .filter(|skill| skill.dir.parent() == Some(level_dir.as_path()))
        {
            let found = conflict(skill, ours, prime)
                .or_else(|| masking(skill, ours, prime, &chain[index + 1..]));
            match found {
                Some(conflict) => selected.disabled.push(DisabledSkill {
                    skill: skill.clone(),
                    conflict,
                }),
                None => selected.files.push(skill.file.display().to_string()),
            }
        }
    }
    selected
}

/// Le conflit Python qu'une skill de yusAi perd, s'il y en a un.
pub fn conflict(skill: &YusaiSkill, ours: &[YusaiSkill], prime: &[Skill]) -> Option<Conflict> {
    let names = skill.python.as_ref()?;
    prime_conflict(names, &skill.dir, prime).or_else(|| {
        ours.iter()
            .filter(|other| !same_dir(&other.dir, &skill.dir) && outranks(other, skill))
            .find_map(|other| {
                let shared = names.shares_with(other.python.as_ref()?)?;
                Some(Conflict::with_ours(shared, other))
            })
    })
}

/// La skill Python qu'une skill de yusAi masquerait dans une session :
/// une skill de Prime du même nom (toujours chargée après les nôtres), ou
/// une des nôtres d'un niveau suivant de la session (`later`).
fn masking(
    skill: &YusaiSkill,
    ours: &[YusaiSkill],
    prime: &[Skill],
    later: &[PathBuf],
) -> Option<Conflict> {
    let shared = || SharedName::Skill(skill.name.clone());
    ours.iter()
        .find(|other| {
            other.name == skill.name
                && other.python.is_some()
                && !same_dir(&other.dir, &skill.dir)
                && later
                    .iter()
                    .any(|level| other.dir.parent() == Some(level.as_path()))
        })
        .map(|other| Conflict::with_ours(shared(), other))
        .or_else(|| prime_python_named(&skill.name, &skill.dir, prime, shared()))
}

/// Une skill Python de Prime nommée `name`, hors du dossier `dir`.
fn prime_python_named(
    name: &str,
    dir: &Path,
    prime: &[Skill],
    shared: SharedName,
) -> Option<Conflict> {
    prime.iter().find_map(|other| {
        let python = other.python.as_ref()?;
        (other.name == name && !same_dir(&python.package_path, dir)).then(|| Conflict {
            shared: shared.clone(),
            holder: other.name.clone(),
            holder_level: None,
            holder_owner: None,
            holder_dir: python.package_path.clone(),
        })
    })
}

/// Une skill Python de Prime (hors du dossier `dir`) qui porte déjà un de
/// ces noms.
fn prime_conflict(names: &PythonNames, dir: &Path, prime: &[Skill]) -> Option<Conflict> {
    prime.iter().find_map(|other| {
        let python = other.python.as_ref()?;
        if same_dir(&python.package_path, dir) {
            return None;
        }
        Some(Conflict {
            shared: names.shares_with(&python_names(other)?)?,
            holder: other.name.clone(),
            holder_level: None,
            holder_owner: None,
            holder_dir: python.package_path.clone(),
        })
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

/// La skill de yusAi d'un dossier (hors archive).
pub fn find_skill(data_dir: &Path, dir: &Path) -> Result<YusaiSkill> {
    our_skills(data_dir)
        .into_iter()
        .find(|skill| same_dir(&skill.dir, dir))
        .ok_or_else(|| anyhow!("no yusAi skill in {}", dir.display()))
}

/// Refuse un nom déjà pris : même nom de skill dans `level_dir`, ou un nom
/// Python porté par une autre skill de yusAi (archive exclue) ou par une
/// skill que Prime charge pour le projet `cwd`. `itself` : le dossier de
/// la skill qu'on déplace ou restaure.
pub fn check_free(
    sources: &SkillSources,
    cwd: &Path,
    name: &str,
    python: Option<&PythonNames>,
    level_dir: &Path,
    itself: Option<&Path>,
) -> Result<()> {
    let is_itself = |dir: &Path| itself.is_some_and(|itself| same_dir(itself, dir));
    let taken = level_dir.join(name);
    if taken.exists() && !is_itself(&taken) {
        bail!(
            "a skill named `{name}` already exists there ({})",
            taken.display()
        );
    }
    let ours = our_skills(&sources.data_dir);
    let prime = prime_skills(sources, cwd);
    let here = itself.unwrap_or(&taken);
    // Une skill Python du même nom, à nous ou à Prime : l'une masquerait
    // l'autre là où les deux se chargent.
    let named = ours
        .iter()
        .find(|other| other.name == name && other.python.is_some() && !is_itself(&other.dir))
        .map(|other| Conflict::with_ours(SharedName::Skill(name.to_string()), other))
        .or_else(|| prime_python_named(name, here, &prime, SharedName::Skill(name.to_string())));
    let python_names = python.and_then(|names| {
        ours.iter()
            .filter(|other| !is_itself(&other.dir))
            .find_map(|other| {
                let shared = names.shares_with(other.python.as_ref()?)?;
                Some(Conflict::with_ours(shared, other))
            })
            .or_else(|| prime_conflict(names, here, &prime))
    });
    if let Some(conflict) = python_names.or(named) {
        bail!("{}; rename one of them first", conflict.english());
    }
    Ok(())
}

/// Range la skill du dossier `dir` dans `target` (un dossier de niveau).
/// Renvoie son nouveau dossier.
pub fn move_skill(
    sources: &SkillSources,
    cwd: &Path,
    dir: &Path,
    target: &Path,
) -> Result<PathBuf> {
    let skill = find_skill(&sources.data_dir, dir)?;
    let name = folder_name(&skill.dir)?;
    let destination = target.join(&name);
    if skill
        .dir
        .parent()
        .is_some_and(|parent| same_dir(parent, target))
    {
        return Ok(skill.dir);
    }
    check_free(
        sources,
        cwd,
        &name,
        skill.python.as_ref(),
        target,
        Some(&skill.dir),
    )?;
    std::fs::rename(&skill.dir, &destination).with_context(|| {
        format!(
            "unable to move {} to {}",
            skill.dir.display(),
            destination.display()
        )
    })?;
    Ok(destination)
}

/// Où une skill archivée était rangée.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivedFrom {
    pub dir: PathBuf,
    pub level: LessonLevel,
    pub owner: Option<String>,
    pub archived_ms: u64,
}

/// Une skill de l'archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivedSkill {
    pub skill: YusaiSkill,
    pub from: ArchivedFrom,
}

/// Archive la skill du dossier `dir`. Renvoie son dossier dans l'archive.
pub fn archive_skill(data_dir: &Path, dir: &Path) -> Result<PathBuf> {
    let skill = find_skill(data_dir, dir)?;
    let archived_ms = now_ms();
    let archive = archive_dir(data_dir);
    std::fs::create_dir_all(&archive)
        .with_context(|| format!("unable to create {}", archive.display()))?;
    let destination = archive.join(format!("{archived_ms}-{}", folder_name(&skill.dir)?));
    std::fs::rename(&skill.dir, &destination)
        .with_context(|| format!("unable to archive {}", skill.dir.display()))?;
    let from = ArchivedFrom {
        dir: skill.dir,
        level: skill.level,
        owner: skill.owner,
        archived_ms,
    };
    std::fs::write(
        destination.join(ARCHIVED_FILE),
        serde_json::to_vec_pretty(&from)?,
    )
    .context("unable to note where the skill came from")?;
    Ok(destination)
}

/// Les skills de l'archive, les plus récentes d'abord.
pub fn archived_skills(data_dir: &Path) -> Vec<ArchivedSkill> {
    let mut archived: Vec<ArchivedSkill> = subdirs(&archive_dir(data_dir))
        .into_iter()
        .filter_map(|dir| {
            let from: ArchivedFrom =
                serde_json::from_slice(&std::fs::read(dir.join(ARCHIVED_FILE)).ok()?).ok()?;
            let skill = skills_in(&dir, from.level, from.owner.clone())
                .into_iter()
                .find(|skill| skill.dir == dir)?;
            Some(ArchivedSkill { skill, from })
        })
        .collect();
    archived.sort_by_key(|entry| std::cmp::Reverse(entry.from.archived_ms));
    archived
}

/// Remet une skill archivée à sa place, si son nom y est encore libre et
/// si son nom Python n'est pas pris ailleurs entre-temps. Renvoie son
/// dossier.
pub fn restore_skill(sources: &SkillSources, cwd: &Path, archived: &Path) -> Result<PathBuf> {
    let entry = archived_skills(&sources.data_dir)
        .into_iter()
        .find(|entry| same_dir(&entry.skill.dir, archived))
        .ok_or_else(|| anyhow!("no archived skill in {}", archived.display()))?;
    let destination = entry.from.dir.clone();
    let level = destination
        .parent()
        .ok_or_else(|| anyhow!("no level folder for {}", destination.display()))?;
    let name = folder_name(&destination)?;
    check_free(
        sources,
        cwd,
        &name,
        entry.skill.python.as_ref(),
        level,
        None,
    )?;
    match &entry.from.owner {
        Some(owner) if entry.from.level != LessonLevel::Global => ensure_owner_dir(level, owner),
        _ => std::fs::create_dir_all(level),
    }
    .with_context(|| format!("unable to create {}", level.display()))?;
    std::fs::rename(archived, &destination)
        .with_context(|| format!("unable to restore {}", destination.display()))?;
    let _ = std::fs::remove_file(destination.join(ARCHIVED_FILE));
    Ok(destination)
}

/// Une skill markdown nouvelle : `<level_dir>/<name>/SKILL.md`, liée à la
/// proposition `proposal_id`. Le nom doit être libre (voir [`check_free`]).
pub fn write_skill(
    level_dir: &Path,
    name: &str,
    description: &str,
    body: &str,
    proposal_id: &str,
) -> Result<PathBuf> {
    if !is_valid_skill_name(name) {
        bail!("invalid skill name `{name}`");
    }
    let dir = level_dir.join(name);
    if dir.exists() {
        bail!(
            "a skill named `{name}` already exists there ({})",
            dir.display()
        );
    }
    let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let description: String = description.chars().take(1024).collect();
    if description.is_empty() {
        bail!("the skill `{name}` needs a description");
    }
    // Une chaîne JSON est une chaîne YAML entre guillemets valide.
    let text = format!(
        "---\nname: {name}\ndescription: {}\nmetadata:\n  {PROPOSAL_KEY}: {}\n---\n\n{}\n",
        serde_json::to_string(&description)?,
        serde_json::to_string(proposal_id)?,
        body.trim()
    );
    std::fs::create_dir_all(&dir).with_context(|| format!("unable to create {}", dir.display()))?;
    std::fs::write(dir.join("SKILL.md"), text)
        .with_context(|| format!("unable to write {}", dir.display()))?;
    Ok(dir)
}

/// Un nom de skill selon Prime : minuscules, chiffres, tirets simples, 64
/// caractères au plus (pa-core/src/skills/discovery.rs, `validate_skill_name`).
pub fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// Un nom de skill tiré d'un texte libre : `Format Code_v2` → `format-code-v2`.
pub fn skill_name_from(text: &str) -> Option<String> {
    let mut name = String::new();
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            name.push(ch);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let name: String = name.chars().take(64).collect();
    let name = name.trim_end_matches('-').to_string();
    is_valid_skill_name(&name).then_some(name)
}

fn proposal_id(skill_file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(skill_file).ok()?;
    let (frontmatter, _) = parse_frontmatter(&text);
    frontmatter
        .get("metadata")?
        .get(PROPOSAL_KEY)?
        .as_str()
        .map(str::to_string)
}

fn folder_name(dir: &Path) -> Result<String> {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("no folder name in {}", dir.display()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
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
        // Une skill markdown du projet b du même nom que la Python globale :
        // elle la masquerait dans b, pas dans a.
        write_skill(&other, "fmt", None);

        let skills = thread_skills(&sources, &workspace_id, None);
        let disabled: Vec<(&str, String)> = skills
            .disabled
            .iter()
            .map(|disabled| (disabled.skill.name.as_str(), disabled.conflict.french()))
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
        // Le projet b garde sa skill Python, pas son markdown `fmt`.
        let b = thread_skills(&sources, &other_id, None);
        assert_eq!(b.disabled.len(), 1, "{:?}", b.disabled);
        assert_eq!(b.disabled[0].skill.name, "fmt");
        assert_eq!(
            b.disabled[0].conflict.shared,
            SharedName::Skill("fmt".to_string())
        );
        assert_eq!(
            names(&b.files),
            vec!["projects/lint-old".to_string(), "global/fmt".to_string()]
        );
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
        assert!(skills.disabled[0].conflict.french().contains("de Prime"));
        assert!(skills.disabled[0].conflict.english().starts_with(
            "Python import name `agent_tool` is taken by the Prime skill `agent-tool`"
        ));
        assert_eq!(names(&skills.files), vec!["global/free-tool".to_string()]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_skill_never_hides_a_python_skill_of_the_same_name() {
        let root = scratch();
        let sources = sources(&root);
        let data = &sources.data_dir;
        let workspace = root.join("work-a");
        let workspace_id = workspace.to_string_lossy().into_owned();
        // Prime : une skill Python et une skill markdown.
        write_skill(&sources.agent_dir.join("skills"), "edit", Some("edit"));
        write_skill(&sources.agent_dir.join("skills"), "guide", None);
        let project = project_skills_dir(data, &workspace_id);
        ensure_owner_dir(&project, &workspace_id).unwrap();
        let global = global_skills_dir(data);
        // Masquent une skill Python : écartées.
        write_skill(&project, "edit", None);
        write_skill(&global, "fmt", Some("fmt"));
        write_skill(&project, "fmt", None);
        // Masquent une skill markdown, ou une Python chargée avant : gardées.
        write_skill(&project, "guide", None);
        write_skill(&global, "notes", None);
        write_skill(&project, "notes", None);
        write_skill(&project, "lint", Some("lint"));
        write_skill(&global, "lint", None);

        let skills = thread_skills(&sources, &workspace_id, None);
        let disabled: Vec<(&str, String)> = skills
            .disabled
            .iter()
            .map(|disabled| (disabled.skill.name.as_str(), disabled.conflict.french()))
            .collect();
        assert_eq!(disabled.len(), 2, "{disabled:?}");
        assert_eq!(disabled[0].0, "edit");
        assert!(
            disabled[0]
                .1
                .starts_with("nom `edit` déjà pris par la skill Python `edit` de Prime ("),
            "{}",
            disabled[0].1
        );
        assert!(disabled[0]
            .1
            .ends_with(": elle la masquerait et sa fonction disparaîtrait du noyau"));
        assert_eq!(
            skills.disabled[0].conflict.shared,
            SharedName::Skill("edit".to_string())
        );
        assert_eq!(disabled[1].0, "fmt");
        assert!(
            disabled[1]
                .1
                .contains("la skill Python `fmt` du niveau global"),
            "{}",
            disabled[1].1
        );
        assert_eq!(
            names(&skills.files),
            vec![
                "projects/guide".to_string(),
                "projects/lint".to_string(),
                "projects/notes".to_string(),
                "global/fmt".to_string(),
                "global/lint".to_string(),
                "global/notes".to_string(),
            ]
        );
        // Un autre projet n'a pas le markdown `fmt` : rien n'y est masqué.
        assert!(thread_skills(&sources, "/work/b", None)
            .disabled
            .iter()
            .all(|disabled| disabled.skill.name != "fmt"));

        // Ce que yusAi range lui-même ne prend jamais le nom d'une skill Python.
        let error = check_free(&sources, &workspace, "edit", None, &global, None)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("Skill name `edit` is taken by the Prime skill `edit`"),
            "{error}"
        );
        let error = check_free(
            &sources,
            &workspace,
            "lint",
            None,
            &root.join("elsewhere"),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(&format!("the skill `lint` of project {workspace_id}")),
            "{error}"
        );
        check_free(&sources, &workspace, "guide", None, &global, None).unwrap();
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
