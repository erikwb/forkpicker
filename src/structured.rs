//! Native JSON Schema transport for the three supported classification CLIs.
use crate::{llm, review, write_json};
use anyhow::{ensure, Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Codex,
    Claude,
    Grok,
}
impl Agent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Grok => "grok",
        }
    }
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "grok" => Ok(Self::Grok),
            _ => anyhow::bail!(
                "classification requires native schema support: choose codex, claude, or grok"
            ),
        }
    }
}

pub fn invocation(
    agent: &Agent,
    profile: &review::AgentProfile,
    input: &Path,
    output: &Path,
    schema_path: &Path,
    schema: &Value,
) -> Result<Vec<String>> {
    let mut profile = profile.clone();
    // Grok's plain-output default conflicts with its schema envelope.
    if matches!(agent, Agent::Grok) {
        let mut i = 0;
        while i < profile.command.len() {
            if profile.command[i] == "--output-format" {
                ensure!(i + 1 < profile.command.len(), "missing Grok output format");
                profile.command.drain(i..i + 2);
            } else if profile.command[i].starts_with("--output-format=") {
                profile.command.remove(i);
            } else {
                i += 1;
            }
        }
        profile
            .command
            .extend(["--output-format".into(), "json".into()]);
    }
    let mut argv = review::invocation(&profile, input, output)?;
    match agent {
        Agent::Codex => argv.extend([
            "--output-schema".into(),
            schema_path.to_string_lossy().into_owned(),
        ]),
        Agent::Claude | Agent::Grok => {
            argv.extend(["--json-schema".into(), serde_json::to_string(schema)?])
        }
    }
    Ok(argv)
}

pub fn decode(agent: &Agent, text: &str) -> Result<(Value, Option<Value>)> {
    let value: Value = serde_json::from_str(text).context("agent did not return JSON")?;
    if matches!(agent, Agent::Codex) {
        return Ok((value, None));
    }
    ensure!(
        value["is_error"] != true && value.get("error").is_none_or(Value::is_null),
        "agent returned an error envelope"
    );
    let usage = Some(
        json!({"usage":value.get("usage"),"model_usage":value.get("modelUsage"),"reported_cost_usd":value.get("total_cost_usd")}),
    );
    let field = if matches!(agent, Agent::Grok) {
        "structuredOutput"
    } else {
        "structured_output"
    };
    if let Some(v) = value.get(field).filter(|v| !v.is_null()) {
        return Ok((v.clone(), usage));
    }
    anyhow::bail!(
        "native structured output missing (envelope fields: {:?})",
        value.as_object().map(|o| o.keys().collect::<Vec<_>>())
    )
}

pub fn run(
    agent: &Agent,
    profile: &review::AgentProfile,
    pack: &Value,
    timeout: Duration,
) -> Result<(Value, Option<Value>, u64)> {
    let temp = tempfile::tempdir()?;
    // Grok interprets *.json prompt files as its typed ACP input protocol,
    // rather than literal text. Our JSON context is prompt text for every CLI.
    let input = temp.path().join("context.txt");
    let output = temp.path().join("result.json");
    let schema_path = temp.path().join("response.schema.json");
    let schema = pack
        .get("response_schema")
        .context("classification schema missing")?;
    write_json(&schema_path, schema)?;
    let bytes = serde_json::to_vec(pack)?;
    crate::write_atomic(&input, &bytes)?;
    let argv = invocation(agent, profile, &input, &output, &schema_path, schema)?;
    let start = Instant::now();
    let uses_output = profile.command.iter().any(|s| s.contains("{output}"));
    let text = llm::run_process(
        &argv,
        &bytes,
        timeout,
        Some(temp.path()),
        &profile.environment,
        uses_output.then_some(output.as_path()),
    )?;
    let (value, usage) = decode(agent, &text)?;
    Ok((value, usage, start.elapsed().as_millis() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_schema_arguments_preserve_json_without_shell_interpretation() {
        let schema = json!({"type":"object","description":"literal $(id) `whoami` and spaces"});
        for agent in [Agent::Codex, Agent::Claude, Agent::Grok] {
            let mut profile = review::Config::default().agents[agent.name()].clone();
            // Avoid requiring installed agent executables on the test machine.
            profile.command[0] = "true".into();
            let argv = invocation(
                &agent,
                &profile,
                Path::new("input file"),
                Path::new("output file"),
                Path::new("schema file"),
                &schema,
            )
            .unwrap();
            if matches!(agent, Agent::Codex) {
                assert_eq!(&argv[argv.len() - 2..], ["--output-schema", "schema file"]);
            } else {
                assert_eq!(argv[argv.len() - 2], "--json-schema");
                assert_eq!(
                    serde_json::from_str::<Value>(argv.last().unwrap()).unwrap(),
                    schema
                );
            }
            if matches!(agent, Agent::Grok) {
                assert!(!argv.iter().any(|a| a == "plain"));
                assert_eq!(argv.iter().filter(|a| *a == "--output-format").count(), 1);
            }
        }
        assert!(Agent::parse("muse").is_err());
        assert!(Agent::parse("opencode").is_err());
    }

    #[test]
    fn structured_envelopes_are_required_and_errors_are_not_answers() {
        let answer = json!({"schema_version":1});
        assert_eq!(
            decode(&Agent::Codex, &answer.to_string()).unwrap().0,
            answer
        );
        for agent in [Agent::Claude, Agent::Grok] {
            let field = if matches!(agent, Agent::Grok) {
                "structuredOutput"
            } else {
                "structured_output"
            };
            let mut envelope = json!({"is_error":false,"usage":{"input_tokens":5}});
            envelope[field] = answer.clone();
            assert_eq!(decode(&agent, &envelope.to_string()).unwrap().0, answer);
            envelope["is_error"] = json!(true);
            assert!(decode(&agent, &envelope.to_string()).is_err());
            assert!(decode(&agent, &answer.to_string()).is_err());
        }
        assert!(decode(&Agent::Claude, r#"{"result":"looks valid"}"#).is_err());
        assert!(decode(&Agent::Codex, "```json\n{}\n```").is_err());
    }

    #[test]
    fn prompt_files_are_literal_text_and_cli_schema_envelopes_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cli.py");
        std::fs::write(
            &script,
            r#"import json,pathlib,sys
p=pathlib.Path(sys.argv[1])
assert p.suffix=='.txt', 'Grok treats .json as typed ACP input'
context=json.loads(p.read_text())
assert json.load(sys.stdin)==context
if '--json-schema' in sys.argv:
    schema=json.loads(sys.argv[sys.argv.index('--json-schema')+1])
    assert schema==context['response_schema']
    field='structuredOutput' if sys.argv[2]=='grok' else 'structured_output'
    print(json.dumps({field:{'ok':True},'usage':{'input_tokens':10}}))
else:
    print(json.dumps({'ok':True}))
"#,
        )
        .unwrap();
        for agent in [Agent::Claude, Agent::Grok] {
            let mut profile = review::Config::default().agents[agent.name()].clone();
            profile.command = vec![
                "python3".into(),
                script.to_string_lossy().into_owned(),
                "{input}".into(),
                agent.name().into(),
            ];
            let pack = json!({"response_schema":{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}});
            let (answer, usage, _) = run(&agent, &profile, &pack, Duration::from_secs(5)).unwrap();
            assert_eq!(answer, json!({"ok":true}));
            assert_eq!(usage.unwrap()["usage"]["input_tokens"], 10);

            profile.output = review::OutputFormat::Json;
            let request = review::Request {
                agent: agent.name(),
                profile: &profile,
                pack: &pack,
            };
            assert_eq!(request.run_json(Duration::from_secs(5)).unwrap().0, answer);
        }
    }
}
