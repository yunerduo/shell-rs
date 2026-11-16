//! Roles, skills and domain packs: who the assistant is told to be, how it
//! is told to do a class of work, and the bundles both travel in.
//!
//! Built-in roles are code, not settings: they cannot be edited away. A
//! domain pack is a JSON bundle of roles and skills; importing one puts it
//! in `settings.assistant.packs`, and the library merges it with the
//! built-in roles and the roles people wrote.

use serde::{Deserialize, Serialize};

use crate::i18n::t;

use super::settings::AssistantSettings;

/// The most roles, skills and packs the settings file keeps.
pub const ROLES_LIMIT: usize = 50;
pub const SKILLS_LIMIT: usize = 50;
pub const PACKS_LIMIT: usize = 10;
const NAME_LIMIT: usize = 32;
const DESCRIPTION_LIMIT: usize = 160;
const AVATAR_LIMIT: usize = 4;
const INSTRUCTION_LIMIT: usize = 4000;

/// One agent role: a name, a short description, an avatar and the standing
/// instruction that goes into the system prompt when it is loaded.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Role {
    /// Stable within the settings file; `role:<name>` when written by hand.
    pub id: String,
    pub name: String,
    pub description: String,
    /// What the avatar circle shows, usually one emoji; the name's first
    /// character when blank.
    pub avatar: String,
    /// Appended to the system prompt while the role is loaded.
    pub instruction: String,
    /// Whether the role ships with the app. Never written to the file.
    #[serde(skip)]
    pub builtin: bool,
}

impl Role {
    pub fn normalize(&mut self) {
        self.name = trimmed(&self.name, NAME_LIMIT);
        self.description = trimmed(&self.description, DESCRIPTION_LIMIT);
        self.avatar = trimmed(&self.avatar, AVATAR_LIMIT);
        self.instruction = trimmed(&self.instruction, INSTRUCTION_LIMIT);
        self.id = self.id.trim().to_string();
        if self.id.is_empty() && !self.name.is_empty() {
            self.id = format!("role:{}", self.name);
        }
    }

    /// What the avatar circle shows: the avatar, or the name's first
    /// character.
    pub fn initial(&self) -> String {
        if !self.avatar.is_empty() {
            return self.avatar.clone();
        }
        self.name
            .chars()
            .next()
            .map(String::from)
            .unwrap_or_default()
    }
}

/// One skill: a named instruction for a class of work. Enabled skills are
/// put in the system prompt of every chat turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub instruction: String,
    pub enabled: bool,
    /// Whether the skill is offered by the app rather than added by the
    /// person. Never written to the file.
    #[serde(skip)]
    pub builtin: bool,
}

impl Default for Skill {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            description: String::new(),
            instruction: String::new(),
            enabled: true,
            builtin: false,
        }
    }
}

impl Skill {
    pub fn normalize(&mut self) {
        self.name = trimmed(&self.name, NAME_LIMIT);
        self.description = trimmed(&self.description, DESCRIPTION_LIMIT);
        self.instruction = trimmed(&self.instruction, INSTRUCTION_LIMIT);
        self.id = self.id.trim().to_string();
        if self.id.is_empty() && !self.name.is_empty() {
            self.id = format!("skill:{}", self.name);
        }
    }
}

/// A bundle of roles and skills for one field of work. Importing one adds
/// it whole; removing it takes its roles and skills with it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DomainPack {
    pub id: String,
    pub name: String,
    pub description: String,
    pub roles: Vec<Role>,
    pub skills: Vec<Skill>,
    /// Whether the pack ships with the app. Never written to the file.
    #[serde(skip)]
    pub builtin: bool,
}

impl DomainPack {
    pub fn normalize(&mut self) {
        self.name = trimmed(&self.name, NAME_LIMIT);
        self.description = trimmed(&self.description, DESCRIPTION_LIMIT);
        self.id = self.id.trim().to_string();
        if self.id.is_empty() && !self.name.is_empty() {
            self.id = format!("pack:{}", self.name);
        }
        for role in &mut self.roles {
            role.normalize();
        }
        self.roles.retain(|role| !role.name.is_empty());
        dedupe(&mut self.roles, |role| role.id.clone());
        self.roles.truncate(ROLES_LIMIT);
        for skill in &mut self.skills {
            skill.normalize();
        }
        self.skills.retain(|skill| !skill.name.is_empty());
        dedupe(&mut self.skills, |skill| skill.id.clone());
        self.skills.truncate(SKILLS_LIMIT);
    }
}

/// One file the import and export buttons exchange: a role, a skill or a
/// whole pack, told apart by `kind`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Bundle {
    Role(Role),
    Skill(Skill),
    Pack(DomainPack),
}

impl Bundle {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

/// The two roles the app ships with.
pub fn builtin_roles() -> Vec<Role> {
    vec![
        Role {
            id: "builtin:ops-engineer".to_string(),
            name: t!("assistant.builtin.role.ops.name").to_string(),
            description: t!("assistant.builtin.role.ops.description").to_string(),
            avatar: "🛠".to_string(),
            instruction: "Act as an operations engineer. Before changing anything, \
                establish the current state with read-only commands, name the impact and \
                how to roll back, and prefer the least invasive step. Never run a \
                destructive command without an explicit instruction from the user. \
                Report what you checked and what you left alone."
                .to_string(),
            builtin: true,
        },
        Role {
            id: "builtin:system-tuner".to_string(),
            name: t!("assistant.builtin.role.tuner.name").to_string(),
            description: t!("assistant.builtin.role.tuner.description").to_string(),
            avatar: "⚙".to_string(),
            instruction: "Act as a system performance engineer. Measure before changing \
                anything: name the metric, its current value and the target. Prefer \
                changes that can be reverted, and after each one measure again and \
                compare. Do not tune by guessing."
                .to_string(),
            builtin: true,
        },
    ]
}

/// The skills the app offers. They are suggestions until added, so a fresh
/// install sends none of them.
pub fn builtin_skills() -> Vec<Skill> {
    vec![
        Skill {
            id: "builtin:logs".to_string(),
            name: t!("assistant.builtin.skill.logs.name").to_string(),
            description: t!("assistant.builtin.skill.logs.description").to_string(),
            instruction: "When diagnosing from logs, first locate the service's log file \
                and the time range, then filter by level and time before reading. Quote \
                the decisive lines with their timestamps instead of summarizing the whole \
                file."
                .to_string(),
            enabled: false,
            builtin: true,
        },
        Skill {
            id: "builtin:resources".to_string(),
            name: t!("assistant.builtin.skill.resources.name").to_string(),
            description: t!("assistant.builtin.skill.resources.description").to_string(),
            instruction: "When checking resource use, read CPU, memory, disk usage and the \
                busiest processes in one pass (for example with vmstat, free, df and ps), \
                state the interval, and point out what is abnormal instead of dumping raw \
                output."
                .to_string(),
            enabled: false,
            builtin: true,
        },
    ]
}

/// The pack the app ships with: the two built-in roles and two skills.
pub fn builtin_packs() -> Vec<DomainPack> {
    vec![DomainPack {
        id: "builtin:linux-ops".to_string(),
        name: t!("assistant.builtin.pack.name").to_string(),
        description: t!("assistant.builtin.pack.description").to_string(),
        roles: builtin_roles(),
        skills: builtin_skills(),
        builtin: true,
    }]
}

/// Everything a conversation may use: built-in roles first, then the ones
/// people wrote and the ones packs brought. Skills come only from settings
/// and packs, since a built-in skill does nothing until it is added.
pub struct Library {
    pub roles: Vec<Role>,
    pub skills: Vec<Skill>,
}

pub fn library(settings: &AssistantSettings) -> Library {
    let mut roles = builtin_roles();
    for role in &settings.roles {
        push_unique(&mut roles, role.clone());
    }
    for pack in &settings.packs {
        for role in &pack.roles {
            push_unique(&mut roles, role.clone());
        }
    }
    let mut skills = settings.skills.clone();
    for pack in &settings.packs {
        for skill in &pack.skills {
            push_unique_skill(&mut skills, skill.clone());
        }
    }
    Library { roles, skills }
}

/// The role `id` names, if the library has it.
pub fn role<'a>(library: &'a Library, id: &str) -> Option<&'a Role> {
    library.roles.iter().find(|role| role.id == id)
}

/// The ids of the roles `text` mentions as `@name`, longest name first so
/// `@运维` does not swallow `@运维工程师`. Unknown mentions stay text.
pub fn mentioned_roles(text: &str, roles: &[Role]) -> Vec<String> {
    let mut named: Vec<&Role> = roles
        .iter()
        .filter(|role| !role.name.is_empty() && text.contains(&format!("@{}", role.name)))
        .collect();
    named.sort_by_key(|role| std::cmp::Reverse(role.name.chars().count()));
    let mut ids: Vec<String> = Vec::new();
    for role in named {
        if !ids.contains(&role.id) {
            ids.push(role.id.clone());
        }
    }
    ids
}

/// What the system prompt says about the loaded roles and the enabled
/// skills, in English like the rest of the prompt. Empty when neither is
/// in play.
pub fn instructions(library: &Library, loaded: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let roles: Vec<&Role> = loaded.iter().filter_map(|id| role(library, id)).collect();
    if !roles.is_empty() {
        parts.push(
            "Roles loaded in this conversation. With one role, answer as that role. \
             With several, follow all of them; where their guidance conflicts, prefer \
             the more specific one and say which you followed. Roles shape expertise \
             and style, not permission: the command policy and the tools still decide \
             what may run."
                .to_string(),
        );
        for role in roles {
            parts.push(format!(
                "- {} ({}): {}",
                role.name, role.description, role.instruction
            ));
        }
    }
    for skill in library.skills.iter().filter(|skill| skill.enabled) {
        parts.push(format!("Skill {}: {}", skill.name, skill.instruction));
    }
    parts.join("\n")
}

fn push_unique(roles: &mut Vec<Role>, role: Role) {
    if role.name.is_empty() || roles.iter().any(|other| other.id == role.id) {
        return;
    }
    roles.push(role);
}

fn push_unique_skill(skills: &mut Vec<Skill>, skill: Skill) {
    if skill.name.is_empty() || skills.iter().any(|other| other.id == skill.id) {
        return;
    }
    skills.push(skill);
}

fn dedupe<T>(items: &mut Vec<T>, id: impl Fn(&T) -> String) {
    let mut seen: Vec<String> = Vec::new();
    items.retain(|item| {
        let id = id(item);
        if seen.contains(&id) {
            return false;
        }
        seen.push(id);
        true
    });
}

fn trimmed(value: &str, limit: usize) -> String {
    value.trim().chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(
        roles: Vec<Role>,
        skills: Vec<Skill>,
        packs: Vec<DomainPack>,
    ) -> AssistantSettings {
        AssistantSettings {
            roles,
            skills,
            packs,
            ..AssistantSettings::default()
        }
    }

    #[test]
    fn the_library_starts_with_the_built_in_roles() {
        let library = library(&AssistantSettings::default());
        assert!(
            library
                .roles
                .iter()
                .any(|role| role.id == "builtin:ops-engineer")
        );
        assert!(
            library
                .roles
                .iter()
                .any(|role| role.id == "builtin:system-tuner")
        );
        assert!(
            library.skills.is_empty(),
            "built-in skills wait until they are added"
        );
    }

    #[test]
    fn a_pack_brings_its_roles_and_skills() {
        let mut pack = DomainPack {
            name: "on call".to_string(),
            roles: vec![Role {
                name: "值班".to_string(),
                instruction: "Stay concise.".to_string(),
                ..Role::default()
            }],
            skills: vec![Skill {
                name: "看盘".to_string(),
                instruction: "Read the dashboard first.".to_string(),
                ..Skill::default()
            }],
            ..DomainPack::default()
        };
        pack.normalize();
        let library = library(&settings_with(Vec::new(), Vec::new(), vec![pack]));
        assert!(library.roles.iter().any(|role| role.name == "值班"));
        assert!(library.skills.iter().any(|skill| skill.name == "看盘"));
        let instruction = instructions(&library, &["role:值班".to_string()]);
        assert!(instruction.contains("Roles loaded"));
        assert!(instruction.contains("Stay concise."));
        assert!(instruction.contains("Skill 看盘"));
    }

    #[test]
    fn mentions_pick_the_longest_name_and_skip_strangers() {
        let mut roles = builtin_roles();
        roles.push(Role {
            id: "role:ops-short".to_string(),
            name: "运维".to_string(),
            instruction: "Short.".to_string(),
            ..Role::default()
        });
        let ids = mentioned_roles("@运维工程师 看下磁盘，@运维 也看，@陌生人", &roles);
        assert_eq!(
            ids.first().map(String::as_str),
            Some("builtin:ops-engineer")
        );
        assert!(ids.contains(&"role:ops-short".to_string()));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn a_handwritten_role_gets_an_id_and_a_file_round_trips() {
        let mut role = Role {
            name: " 值班 ".to_string(),
            description: " 处理告警 ".to_string(),
            instruction: " 先看监控 ".to_string(),
            ..Role::default()
        };
        role.normalize();
        assert_eq!(role.id, "role:值班");
        assert_eq!(role.name, "值班");
        let bundle = Bundle::Role(role.clone());
        let parsed = Bundle::parse(&bundle.to_json()).expect("the file it wrote");
        assert_eq!(parsed, bundle);
    }

    #[test]
    fn a_pack_file_round_trips_and_cleans_its_parts() {
        let json = r#"{
            "kind": "pack",
            "name": "db",
            "roles": [{"name": "dba", "instruction": "Be careful."}],
            "skills": [{"name": "slow query", "instruction": "Look at the plan."}]
        }"#;
        let Bundle::Pack(mut pack) = Bundle::parse(json).expect("a pack") else {
            panic!("not a pack");
        };
        pack.normalize();
        assert_eq!(pack.id, "pack:db");
        assert_eq!(pack.roles[0].id, "role:dba");
        assert!(
            pack.skills[0].enabled,
            "skills are on unless the file says no"
        );
    }
}
