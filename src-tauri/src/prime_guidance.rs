//! Ce que yusAi ajoute au prompt système d'une session Prime : les leçons
//! qui s'appliquent à la conversation (projet, type du projet, global) et
//! nos consignes, passées au `Create` dans `appendSystemPrompt`. Les
//! consignes gardent `refine.run()` (sa notice est importée, auteur
//! `refine:agent`) et interdisent `rlm.harness.*` : en local il échoue
//! (pas de `RLM_SESSION_DIR`), et son erreur pousse vers `global_=True`.
//!
//! Prime rend chaque chaîne en puce `- …` sous `# Additional Guidance`,
//! dans la partie dynamique du prompt système, sans les espaces autour et
//! sans les doublons exacts (pa-daemon/src/agent_engine/lifecycle.rs:1130,
//! pa-core/src/prompts/system_prompt.rs:214-224, 354-363). Le `Create`
//! durable rejoue ce texte à la relance d'un worker tué
//! (pa-daemon/src/supervisor/worker_lifecycle.rs:235-252).
//!
//! Le texte est figé au `Create` : une leçon apprise ou validée en cours de
//! route n'arrive qu'à la prochaine ouverture du fil.
//!
//! Taille (décision du plan) : 300 caractères au plus par leçon, coupée par
//! « … », et 4 000 au total, consignes comprises. Les leçons arrivent dans
//! l'ordre du magasin (épinglées, puis projet, type, global ; les plus
//! récentes d'abord) ; celles qui ne tiennent pas restent en base et une
//! dernière puce dit combien.
//!
//! Les skills de yusAi partent dans `config.skills` au même `Create`
//! ([`crate::prime_skills`]) ; chaque skill du projet écartée pour un
//! conflit de nom Python a sa puce, avec la raison et la consigne de la
//! renommer, avant les leçons : toujours présente, elle prend sur leur place.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use sinew_app::store::{AppStore, Lesson, LessonKind, LessonLevel, LessonScope};

use crate::prime_skills::{project_skills_dir, thread_skills, DisabledSkill, SkillSources};

/// Taille maximale d'une leçon injectée, en caractères.
pub const LESSON_CHARS: usize = 300;
/// Taille maximale de tout le texte injecté, en caractères.
pub const GUIDANCE_CHARS: usize = 4_000;

/// Le texte injecté et ce qu'il contient.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Guidance {
    /// Les chaînes d'`appendSystemPrompt`, une puce chacune.
    pub lines: Vec<String>,
    /// Les leçons injectées.
    pub injected: Vec<String>,
    /// Les leçons applicables qui n'ont pas tenu dans la limite.
    pub left_out: Vec<String>,
    /// Les `SKILL.md` de `config.skills`, dans l'ordre.
    pub skills: Vec<String>,
    /// Les skills des niveaux de la session écartées (conflit Python).
    pub disabled_skills: Vec<DisabledSkill>,
}

/// La config d'un `Create` avec ce texte dans `appendSystemPrompt` et nos
/// skills dans `skills`.
pub fn with_guidance(mut config: serde_json::Value, guidance: &Guidance) -> serde_json::Value {
    config["appendSystemPrompt"] = serde_json::json!(guidance.lines);
    config["skills"] = serde_json::json!(guidance.skills);
    config
}

/// Le texte et les skills pour une conversation du projet `workspace_id`.
pub fn thread_guidance(
    store: &AppStore,
    sources: &SkillSources,
    workspace_id: &str,
) -> Result<Guidance> {
    let project_type = store.confirmed_project_type(workspace_id)?;
    let skills = thread_skills(sources, workspace_id, project_type.as_deref());
    let lessons = store.applicable_lessons(&LessonScope {
        workspace_id: workspace_id.to_string(),
        project_type,
    })?;
    let mut guidance = guidance(
        &lessons,
        &project_skills_dir(&sources.data_dir, workspace_id),
        &skills.disabled,
    );
    guidance.skills = skills.files;
    guidance.disabled_skills = skills.disabled;
    Ok(guidance)
}

/// Le texte pour des leçons déjà triées par priorité ; `disabled` : les
/// skills écartées de la session (seules celles du projet ont leur puce,
/// le modèle ne touche pas aux autres niveaux).
pub fn guidance(lessons: &[Lesson], skills_dir: &Path, disabled: &[DisabledSkill]) -> Guidance {
    let mut lines = vec![rules(skills_dir)];
    lines.extend(
        disabled
            .iter()
            .filter(|disabled| disabled.skill.level == LessonLevel::Project)
            .map(disabled_line),
    );
    let mut injected = Vec::new();
    if !lessons.is_empty() {
        lines.push(LESSONS_INTRO.to_string());
    }
    let mut used: usize = lines.iter().map(|line| chars(line)).sum();
    for lesson in lessons {
        let line = lesson_line(lesson);
        if used + chars(&line) > GUIDANCE_CHARS {
            break;
        }
        used += chars(&line);
        lines.push(line);
        injected.push(lesson.id.clone());
    }
    // Place pour la puce de dépassement, prise sur les dernières leçons.
    if injected.len() < lessons.len() {
        loop {
            let note = left_out_line(lessons.len() - injected.len());
            if used + chars(&note) <= GUIDANCE_CHARS || injected.is_empty() {
                lines.push(note);
                break;
            }
            if let Some(line) = lines.pop() {
                used -= chars(&line);
            }
            injected.pop();
        }
    }
    let left_out = lessons[injected.len()..]
        .iter()
        .map(|lesson| lesson.id.clone())
        .collect();
    Guidance {
        lines,
        injected,
        left_out,
        ..Guidance::default()
    }
}

const LESSONS_INTRO: &str = "Leçons retenues par yusAi pour ce projet, de la plus spécifique à la plus générale (les puces suivantes). Signale celles qui te semblent fausses.";

fn rules(skills_dir: &Path) -> String {
    format!(
        "Consignes de yusAi : pour retenir quelque chose, appelle `await refine.run(\"…\")` sans `global_=True` ; yusAi range la leçon dans ce projet, l'utilisateur la partage s'il le veut. N'appelle jamais `rlm.harness.*` (create, update, delete) : en local ces appels échouent ici, et `global_=True` contournerait la validation de l'utilisateur. Crée les skills de ce projet dans `{}`, pas dans `.prime/agent/skills/` ni `~/.prime/agent/skills/` ; une skill Python y prend un nom qu'aucune autre skill ne porte (tous les projets partagent un même environnement Python).",
        skills_dir.display()
    )
}

fn disabled_line(disabled: &DisabledSkill) -> String {
    format!(
        "Skill `{}` du projet désactivée par yusAi : {}. Tous les projets partagent un même environnement Python, deux skills ne peuvent pas y porter le même nom. Renomme-la dans `{}` : dossier, `name` du SKILL.md, paquet `src/<nom_d_import>/` et `name` du pyproject.toml ; elle reviendra à la prochaine ouverture du fil.",
        disabled.skill.name,
        disabled.reason,
        disabled.skill.dir.display()
    )
}

/// `[niveau · genre] Titre : contenu`, sur une ligne, coupée à
/// [`LESSON_CHARS`].
fn lesson_line(lesson: &Lesson) -> String {
    let level = match lesson.level {
        LessonLevel::Project => "projet",
        LessonLevel::Type => "type",
        LessonLevel::Global => "global",
    };
    let kind = match lesson.kind {
        LessonKind::Memory => "fait",
        LessonKind::Prompt => "consigne",
        LessonKind::Subagent => "rôle",
    };
    let line = format!(
        "[{level} · {kind}] {} : {}",
        one_line(&lesson.title),
        one_line(&lesson.content)
    );
    if chars(&line) <= LESSON_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(LESSON_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

fn left_out_line(count: usize) -> String {
    if count == 1 {
        "1 autre leçon de yusAi n'est pas injectée (limite de taille).".to_string()
    } else {
        format!("{count} autres leçons de yusAi ne sont pas injectées (limite de taille).")
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn chars(text: &str) -> usize {
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sinew_app::store::LessonStatus;

    fn lesson(
        id: &str,
        level: LessonLevel,
        kind: LessonKind,
        title: &str,
        content: &str,
    ) -> Lesson {
        Lesson {
            id: id.to_string(),
            level,
            workspace_id: Some("/work/a".to_string()),
            project_type: (level == LessonLevel::Type).then(|| "rust".to_string()),
            kind,
            title: title.to_string(),
            content: content.to_string(),
            status: LessonStatus::Active,
            pinned: false,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn total(guidance: &Guidance) -> usize {
        guidance.lines.iter().map(|line| chars(line)).sum()
    }

    #[test]
    fn without_lessons_only_the_rules_go_in() {
        let guidance = guidance(&[], Path::new("/data/prime-skills/projects/abc"), &[]);
        assert_eq!(guidance.lines.len(), 1);
        assert!(guidance.lines[0].contains("`/data/prime-skills/projects/abc`"));
        assert!(guidance.lines[0].contains("`await refine.run(\"…\")` sans `global_=True`"));
        assert!(guidance.lines[0].contains("N'appelle jamais `rlm.harness.*`"));
        assert!(guidance.injected.is_empty() && guidance.left_out.is_empty());
    }

    #[test]
    fn each_lesson_is_one_labelled_line_in_store_order() {
        let lessons = [
            lesson(
                "yl_1",
                LessonLevel::Project,
                LessonKind::Memory,
                "Tests",
                "Lancer\ncargo   test.",
            ),
            lesson(
                "yl_2",
                LessonLevel::Type,
                LessonKind::Prompt,
                "Erreurs",
                "Préférer anyhow.",
            ),
            lesson(
                "yl_3",
                LessonLevel::Global,
                LessonKind::Subagent,
                "Relecteur",
                "Relit les diffs.",
            ),
        ];
        let guidance = guidance(&lessons, Path::new("/skills"), &[]);
        assert_eq!(
            guidance.lines[2..],
            [
                "[projet · fait] Tests : Lancer cargo test.".to_string(),
                "[type · consigne] Erreurs : Préférer anyhow.".to_string(),
                "[global · rôle] Relecteur : Relit les diffs.".to_string(),
            ]
        );
        assert_eq!(guidance.lines[1], LESSONS_INTRO);
        assert_eq!(guidance.injected, vec!["yl_1", "yl_2", "yl_3"]);
        assert!(guidance.left_out.is_empty());
    }

    #[test]
    fn a_long_lesson_is_cut_at_300_characters() {
        let content = "é".repeat(400);
        let guidance = guidance(
            &[lesson(
                "yl_1",
                LessonLevel::Project,
                LessonKind::Memory,
                "Long",
                &content,
            )],
            Path::new("/skills"),
            &[],
        );
        let line = &guidance.lines[2];
        assert_eq!(chars(line), LESSON_CHARS);
        assert!(line.ends_with("é…"));
        assert!(line.starts_with("[projet · fait] Long : é"));
    }

    #[test]
    fn lessons_beyond_4000_characters_stay_out_with_a_note() {
        let lessons: Vec<Lesson> = (0..30)
            .map(|index| {
                lesson(
                    &format!("yl_{index:02}"),
                    LessonLevel::Project,
                    LessonKind::Memory,
                    "Règle",
                    &format!("{index:02} {}", "x".repeat(280)),
                )
            })
            .collect();
        let guidance = guidance(&lessons, Path::new("/skills"), &[]);
        assert!(
            total(&guidance) <= GUIDANCE_CHARS,
            "{} chars",
            total(&guidance)
        );
        let injected = guidance.injected.len();
        assert!(injected > 5 && injected < 30, "{injected} injected");
        // Les premières (les plus prioritaires) entrent, les suivantes non.
        let ids: Vec<String> = lessons.iter().map(|lesson| lesson.id.clone()).collect();
        assert_eq!(guidance.injected, ids[..injected]);
        assert_eq!(guidance.left_out, ids[injected..]);
        assert_eq!(
            guidance.lines.last().unwrap(),
            &format!(
                "{} autres leçons de yusAi ne sont pas injectées (limite de taille).",
                30 - injected
            )
        );
        assert_eq!(guidance.lines.len(), 2 + injected + 1);
    }

    #[test]
    fn the_note_takes_room_from_the_last_lessons_when_needed() {
        // Des leçons qui remplissent exactement les 4 000 caractères, plus
        // une qui ne tient pas : la puce de dépassement prend la place de
        // la dernière leçon entrée.
        let room = GUIDANCE_CHARS - chars(&rules(Path::new("/skills"))) - chars(LESSONS_INTRO);
        let prefix = chars("[projet · fait] R : ");
        let full = (room - 150) / 100;
        let filler = room - full * 100;
        let line = |index: usize, len: usize| {
            lesson(
                &format!("yl_{index:02}"),
                LessonLevel::Project,
                LessonKind::Memory,
                "R",
                &"x".repeat(len - prefix),
            )
        };
        let mut lessons: Vec<Lesson> = (0..full).map(|index| line(index, 100)).collect();
        lessons.push(line(full, filler));
        lessons.push(line(full + 1, 100));
        let guidance = guidance(&lessons, Path::new("/skills"), &[]);
        assert!(total(&guidance) <= GUIDANCE_CHARS);
        assert_eq!(guidance.injected.len(), full, "the filler made room");
        assert_eq!(guidance.left_out.len(), 2);
        assert_eq!(
            guidance.lines.last().unwrap(),
            "2 autres leçons de yusAi ne sont pas injectées (limite de taille)."
        );
    }

    #[test]
    fn a_disabled_project_skill_gets_a_line_before_the_lessons() {
        use crate::prime_skills::YusaiSkill;
        let disabled = |name: &str, level: LessonLevel| DisabledSkill {
            skill: YusaiSkill {
                name: name.to_string(),
                description: "d".to_string(),
                level,
                owner: None,
                dir: Path::new("/skills").join(name),
                file: Path::new("/skills").join(name).join("SKILL.md"),
                python: None,
                created_ms: 0,
            },
            reason: "nom d'import Python `fmt` déjà pris par la skill `fmt` du niveau global"
                .to_string(),
        };
        let lessons = [lesson(
            "yl_1",
            LessonLevel::Project,
            LessonKind::Memory,
            "T",
            "C",
        )];
        let guidance = guidance(
            &lessons,
            Path::new("/skills"),
            &[
                disabled("fmt", LessonLevel::Project),
                disabled("g", LessonLevel::Global),
            ],
        );
        assert_eq!(guidance.lines.len(), 4, "{:?}", guidance.lines);
        assert!(guidance.lines[1].starts_with(
            "Skill `fmt` du projet désactivée par yusAi : nom d'import Python `fmt` déjà pris"
        ));
        assert!(guidance.lines[1].contains("Renomme-la dans `/skills/fmt`"));
        assert_eq!(guidance.lines[2], LESSONS_INTRO);
        assert_eq!(guidance.injected, vec!["yl_1"]);
    }

    #[test]
    fn the_create_config_carries_the_skills() {
        let guidance = Guidance {
            lines: vec!["r".to_string()],
            skills: vec!["/s/a/SKILL.md".to_string()],
            ..Guidance::default()
        };
        let config = with_guidance(serde_json::json!({ "cwd": "/w" }), &guidance);
        assert_eq!(config["skills"], serde_json::json!(["/s/a/SKILL.md"]));
        assert_eq!(config["appendSystemPrompt"], serde_json::json!(["r"]));
        assert_eq!(config["cwd"], "/w");
    }
}
