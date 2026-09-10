//! Lightweight classification settings, separate from full static code reviews.
use crate::review::{AgentProfile, Config};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Omitted/null leaves lower-precedence settings in effect; "inherit" omits CLI argv.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

pub fn classification(agent: &str) -> Settings {
    let (model, effort) = match agent {
        "claude" => (Some("haiku"), None),
        "codex" => (Some("gpt-5.6-luna"), Some("low")),
        "grok" => (Some("grok-4.6"), Some("low")),
        // Retain Muse's account-selected model; its CLI supports low reasoning.
        "muse" => (None, Some("low")),
        // OpenCode/pi have multiple providers. Do not switch billing routes or
        // assume that every model supports the same reasoning-variant names.
        _ => (None, None),
    };
    Settings {
        model: model.map(str::to_owned),
        effort: effort.map(str::to_owned),
    }
}

pub fn resolve_classification(
    config: &Config,
    agent: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<AgentProfile> {
    let mut profile = config
        .agents
        .get(agent)
        .context("unknown classification agent profile")?
        .clone();
    // A replaced adapter command may target a different provider or accept no
    // model flags. Do not attach named-adapter defaults to arbitrary wrappers.
    let builtins = Config::default();
    let mut defaults = if builtins.agents.get(agent).is_some_and(|p| {
        p.command == profile.command
            && p.model_args == profile.model_args
            && p.effort_args == profile.effort_args
    }) {
        classification(agent)
    } else {
        Settings::default()
    };
    let configured = config.classification.get(agent);
    let explicit_model = model
        .or_else(|| configured.and_then(|c| c.model.as_deref()))
        .or(profile.model.as_deref());
    // A user-selected model can have a different effort vocabulary. Do not
    // carry a bundled effort into it unless it is the same bundled model.
    if explicit_model.is_some() && explicit_model != defaults.model.as_deref() {
        defaults.effort = None;
    }
    let effective_model = explicit_model.or(defaults.model.as_deref());
    let effective_effort = effort
        .or_else(|| configured.and_then(|c| c.effort.as_deref()))
        .or(profile.effort.as_deref())
        .or(defaults.effort.as_deref());
    let setting = |value: Option<&str>| -> Result<Option<String>> {
        if let Some(v) = value {
            ensure!(
                !v.trim().is_empty(),
                "model and effort settings must not be empty; use inherit for CLI defaults"
            );
        }
        Ok(value.filter(|v| *v != "inherit").map(str::to_owned))
    };
    let model = setting(effective_model)?;
    let effort = setting(effective_effort)?;
    profile.model = model;
    profile.effort = effort;
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classification_defaults_do_not_change_review_profiles_or_provider_agnostic_routes() {
        let config = Config::default();
        for (agent, model, effort) in [
            ("claude", Some("haiku"), None),
            ("codex", Some("gpt-5.6-luna"), Some("low")),
            ("grok", Some("grok-4.6"), Some("low")),
            ("muse", None, Some("low")),
            ("opencode", None, None),
            ("pi", None, None),
        ] {
            let p = resolve_classification(&config, agent, None, None).unwrap();
            assert_eq!(p.model.as_deref(), model);
            assert_eq!(p.effort.as_deref(), effort);
            assert!(config.agents[agent].model.is_none());
            assert!(config.agents[agent].effort.is_none());
        }
    }
    #[test]
    fn user_preferences_win_and_inherit_removes_arguments() {
        let mut config = Config::default();
        config.agents.get_mut("codex").unwrap().model = Some("existing-model".into());
        let p = resolve_classification(&config, "codex", None, None).unwrap();
        assert_eq!(p.model.as_deref(), Some("existing-model"));
        assert!(p.effort.is_none());
        config.classification.insert(
            "codex".into(),
            Settings {
                model: Some("configured-model".into()),
                effort: Some("medium".into()),
            },
        );
        let p = resolve_classification(&config, "codex", None, None).unwrap();
        assert_eq!(p.model.as_deref(), Some("configured-model"));
        assert_eq!(p.effort.as_deref(), Some("medium"));
        let p =
            resolve_classification(&config, "codex", Some("command-model"), Some("high")).unwrap();
        assert_eq!(p.model.as_deref(), Some("command-model"));
        assert_eq!(p.effort.as_deref(), Some("high"));
        let inherited =
            resolve_classification(&config, "codex", Some("inherit"), Some("inherit")).unwrap();
        let argv = crate::review::invocation(
            &inherited,
            std::path::Path::new("input"),
            std::path::Path::new("output"),
        )
        .unwrap();
        assert!(!argv
            .iter()
            .any(|a| a == "--model" || a.contains("model_reasoning_effort")));
        assert!(resolve_classification(&config, "codex", Some(" "), None).is_err());
        config.agents.get_mut("codex").unwrap().command = vec!["custom-wrapper".into()];
        config.agents.get_mut("codex").unwrap().model = None;
        config.classification.clear();
        let p = resolve_classification(&config, "codex", None, None).unwrap();
        assert!(p.model.is_none() && p.effort.is_none());
    }
}
