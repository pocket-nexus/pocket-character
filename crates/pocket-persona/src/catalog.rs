//! Read the immutable subset of Persona's `library.json` format.
//!
//! Keeping the same data contract lets the native renderer consume an
//! existing Persona asset library without copying paths into a second config.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionRole {
    Idle,
    Speaking,
    Custom,
}

#[derive(Clone, Debug)]
pub struct ActionSpec {
    pub name: String,
    pub description: String,
    pub trigger_scenario: String,
    pub role: ActionRole,
    pub clips: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Catalog {
    pub model_name: String,
    pub model_path: PathBuf,
    pub actions: Vec<ActionSpec>,
}

#[derive(Deserialize)]
struct LibraryFile {
    schema_version: u32,
    default_model_id: Option<String>,
    models: Vec<ModelRecord>,
    animations: Vec<AnimationRecord>,
}

#[derive(Deserialize)]
struct ModelRecord {
    id: String,
    model_name: String,
    asset_path: String,
}

#[derive(Deserialize)]
struct AnimationRecord {
    id: String,
    animation_name: String,
    animation_description: String,
    animation_trigger_scenario: String,
    animation_type: Option<String>,
    asset_paths: Vec<String>,
}

impl Catalog {
    pub fn from_path(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading Persona library {}", path.display()))?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        Self::from_slice(&bytes, base)
    }

    fn from_slice(bytes: &[u8], base: &Path) -> Result<Self> {
        let file: LibraryFile =
            serde_json::from_slice(bytes).context("parsing Persona library.json")?;
        if file.schema_version != 1 {
            bail!(
                "unsupported Persona library schema {}; expected 1",
                file.schema_version
            );
        }
        if file.models.is_empty() {
            bail!("Persona library has no configured model");
        }
        let mut model_ids = std::collections::HashSet::new();
        for model in &file.models {
            validate_id(&model.id, "model id")?;
            if !model_ids.insert(model.id.as_str()) {
                bail!("duplicate Persona model id '{}'", model.id);
            }
            if model.model_name.trim().is_empty() {
                bail!("Persona model name cannot be empty");
            }
        }
        let model = match file.default_model_id {
            Some(ref id) => file
                .models
                .iter()
                .find(|model| model.id == *id)
                .with_context(|| format!("default Persona model '{id}' does not exist"))?,
            None => &file.models[0],
        };
        let model_path = resolve_media(base, &model.asset_path, "vrm")?;

        let mut ids = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        let mut configured = Vec::with_capacity(file.animations.len());
        for animation in file.animations {
            validate_id(&animation.id, "animation id")?;
            if !ids.insert(animation.id.clone()) {
                bail!("duplicate Persona animation id '{}'", animation.id);
            }
            let name = animation.animation_name.trim().to_ascii_lowercase();
            validate_action_name(&name)?;
            if !names.insert(name.clone()) {
                bail!("duplicate Persona action '{name}'");
            }
            if animation.animation_description.trim().is_empty()
                || animation.animation_trigger_scenario.trim().is_empty()
            {
                bail!("Persona action '{name}' needs description and trigger scenario");
            }
            let role = match animation.animation_type.as_deref() {
                Some("IDLE") => ActionRole::Idle,
                Some("TALK") => ActionRole::Speaking,
                None | Some("GREETING" | "HAPPY" | "FINGER_GUN" | "DANCE") => ActionRole::Custom,
                Some(kind) => bail!("invalid Persona animation type '{kind}'"),
            };
            match animation.id.as_str() {
                "system-idle" if name != "idle" || role != ActionRole::Idle => {
                    bail!("system-idle must retain the idle name and IDLE type")
                }
                "system-speaking" if name != "speaking" || role != ActionRole::Speaking => {
                    bail!("system-speaking must retain the speaking name and TALK type")
                }
                "system-idle" | "system-speaking" => {}
                _ if matches!(role, ActionRole::Idle | ActionRole::Speaking) => {
                    bail!("idle and speaking roles belong to their permanent system slots")
                }
                _ => {}
            }
            let clips = animation
                .asset_paths
                .iter()
                .map(|asset| resolve_media(base, asset, "vrma"))
                .collect::<Result<Vec<_>>>()?;
            configured.push((
                animation.id,
                ActionSpec {
                    name,
                    description: animation.animation_description.trim().into(),
                    trigger_scenario: animation.animation_trigger_scenario.trim().into(),
                    role,
                    clips,
                },
            ));
        }

        let mut take_system = |id: &str, fallback: ActionSpec| -> Result<ActionSpec> {
            if let Some(index) = configured.iter().position(|(candidate, _)| candidate == id) {
                return Ok(configured.remove(index).1);
            }
            if names.contains(&fallback.name) {
                bail!(
                    "Persona action '{}' conflicts with the permanent {id} slot",
                    fallback.name
                );
            }
            Ok(fallback)
        };
        let idle = take_system(
            "system-idle",
            system_action("idle", ActionRole::Idle, "A calm resting motion."),
        )?;
        let speaking = take_system(
            "system-speaking",
            system_action(
                "speaking",
                ActionRole::Speaking,
                "Conversational body motion.",
            ),
        )?;
        let mut actions = Vec::with_capacity(configured.len() + 2);
        actions.push(idle);
        actions.push(speaking);
        actions.extend(configured.into_iter().map(|(_, action)| action));

        Ok(Self {
            model_name: model.model_name.clone(),
            model_path,
            actions,
        })
    }

    pub fn action(&self, name: &str) -> Option<&ActionSpec> {
        self.actions.iter().find(|action| action.name == name)
    }

    #[cfg(test)]
    pub fn action_for_role(&self, role: ActionRole) -> Option<&ActionSpec> {
        self.actions.iter().find(|action| action.role == role)
    }

    pub fn playable_actions(&self) -> impl Iterator<Item = &ActionSpec> {
        self.actions
            .iter()
            .filter(|action| !action.clips.is_empty())
    }
}

fn resolve_media(base: &Path, raw: &str, extension: &str) -> Result<PathBuf> {
    let normalized = raw.replace('\\', "/");
    let relative = Path::new(&normalized);
    if raw.trim().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::RootDir))
    {
        bail!("Persona asset path must stay relative to library.json: {raw}");
    }
    if relative
        .extension()
        .and_then(|value| value.to_str())
        .is_none_or(|value| !value.eq_ignore_ascii_case(extension))
    {
        bail!("Persona asset path must end in .{extension}: {raw}");
    }
    Ok(base.join(relative))
}

fn system_action(name: &str, role: ActionRole, description: &str) -> ActionSpec {
    ActionSpec {
        name: name.into(),
        description: description.into(),
        trigger_scenario: "Used automatically by the voice state machine.".into(),
        role,
        clips: Vec::new(),
    }
}

fn validate_id(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && byte == b'-')
        })
    {
        bail!("invalid Persona {label}: {value}");
    }
    Ok(())
}

fn validate_action_name(value: &str) -> Result<()> {
    validate_id(value, "action name")?;
    if !value.as_bytes()[0].is_ascii_lowercase() || value.contains("--") || value.ends_with('-') {
        bail!("invalid Persona action name: {value}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIBRARY: &str = r#"{
      "schema_version": 1,
      "default_model_id": "model-a",
      "models": [{
        "id": "model-a",
        "model_name": "Model A",
        "asset_path": "models/model.vrm"
      }],
      "animations": [{
        "id": "system-idle",
        "animation_name": "idle",
        "animation_description": "A calm resting motion.",
        "animation_trigger_scenario": "While waiting.",
        "animation_type": "IDLE",
        "asset_paths": ["animations/idle.vrma"]
      }, {
        "id": "wave",
        "animation_name": "wave-hello",
        "animation_description": "A friendly wave.",
        "animation_trigger_scenario": "When greeting.",
        "animation_type": "GREETING",
        "asset_paths": []
      }]
    }"#;

    #[test]
    fn reads_persona_library_without_translation() {
        let catalog = Catalog::from_slice(LIBRARY.as_bytes(), Path::new("/tmp/library")).unwrap();
        assert_eq!(catalog.model_name, "Model A");
        assert_eq!(
            catalog.model_path,
            Path::new("/tmp/library/models/model.vrm")
        );
        assert_eq!(
            catalog.action_for_role(ActionRole::Idle).unwrap().name,
            "idle"
        );
        assert_eq!(catalog.action("wave-hello").unwrap().clips.len(), 0);
        assert_eq!(catalog.playable_actions().count(), 1);
    }

    #[test]
    fn asset_paths_cannot_escape_the_library() {
        let bad = LIBRARY.replace("models/model.vrm", "../secret.vrm");
        let error = Catalog::from_slice(bad.as_bytes(), Path::new("/tmp")).unwrap_err();
        assert!(error.to_string().contains("stay relative"));
    }

    #[test]
    fn action_names_keep_persona_mcp_shape() {
        let bad = LIBRARY.replace("wave-hello", "Wave Hello");
        let error = Catalog::from_slice(bad.as_bytes(), Path::new("/tmp")).unwrap_err();
        assert!(error.to_string().contains("action name"));
    }

    #[test]
    fn packaged_ids_and_system_roles_follow_persona_invariants() {
        let duplicate = LIBRARY.replace(r#""id": "wave""#, r#""id": "system-idle""#);
        let error = Catalog::from_slice(duplicate.as_bytes(), Path::new("/tmp")).unwrap_err();
        assert!(error.to_string().contains("duplicate Persona animation id"));

        let extra_talk = LIBRARY.replace(
            r#""animation_type": "GREETING""#,
            r#""animation_type": "TALK""#,
        );
        let error = Catalog::from_slice(extra_talk.as_bytes(), Path::new("/tmp")).unwrap_err();
        assert!(error.to_string().contains("permanent system slots"));
    }
}
