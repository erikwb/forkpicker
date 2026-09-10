# Agent configuration

Forkpicker invokes installed coding-agent CLIs using their existing authentication and billing configuration. It makes no direct model API calls. A subscription login stays a subscription login; an API-configured CLI still uses its API account. Forkpicker does not automatically retry with another model or provider.

## Supported agents and defaults

```sh
forkpicker agents
forkpicker run bluez/bluez --agent claude --model sonnet --effort medium
```

`agents` shows installed executables, supported operations, and effective settings. `run`, discovery, and fork classification use **Codex, Claude Code, or Grok**, whose adapters support native JSON Schema. Individual reviews and demand triage also support Muse, OpenCode, Pi, and custom commands.

| Adapter | Classification/triage model | Effort |
| --- | --- | --- |
| Codex | `gpt-5.6-luna` | `low` |
| Claude | `haiku` | CLI default; no effort flag |
| Grok | `grok-4.6` | `low` |
| Muse, triage only | CLI-selected | `low` |
| OpenCode, Pi, custom triage wrappers | CLI-selected | CLI default |

Full `review` inherits the selected CLI's model and effort. `--model` and `--effort` override the run. Classification precedence, independently for each setting, is: command line → `classification.<agent>` → `agents.<agent>` preference → bundled default → CLI setting. `inherit` suppresses that classification argument. Changing the model drops bundled effort unless effort was explicitly configured. Replacing command/argument templates disables bundled defaults for the wrapper.

Model availability follows the user's account. Requested settings appear in saved results; an inherited setting or provider alias does not establish the actual resolved model, token count, or dollar cost.

## Configuration file

```sh
forkpicker agents --write-config ~/.config/forkpicker/config.json
```

This writes editable built-in profiles and refuses to overwrite an existing file. Use `$XDG_CONFIG_HOME/forkpicker/config.json` when XDG_CONFIG_HOME is set, or global `--config PATH`. Unmentioned built-ins remain available. For example:

```json
{
  "default_agent": "claude",
  "classification": {
    "claude": {"model": "sonnet", "effort": "low"},
    "codex": {"model": "inherit", "effort": "inherit"}
  }
}
```

Classification overrides do not replace full-review settings. Omitted/null classification fields retain lower-precedence defaults; empty strings are rejected. `run --no-llm` does not read agent configuration.

A custom review wrapper can instead define a named profile:

```json
{
  "default_agent": "my-reviewer",
  "agents": {
    "my-reviewer": {
      "command": ["/absolute/path/to/wrapper", "--prompt-file", "{input}"],
      "model": null,
      "effort": null,
      "model_args": ["--model", "{model}"],
      "effort_args": ["--effort", "{effort}"],
      "environment": {},
      "output": "json"
    }
  }
}
```

A named profile replaces the entire profile of the same name and must include its command. Commands are argv arrays, never shell strings. `{input}` and `{output}` expand to private temporary paths; a command using `{output}` has its result read there, otherwise from stdout. The prompt also arrives on stdin. Supported output decoders are `json`, `claude_json`, and `opencode_json`; send diagnostics to stderr. Output streams/files are each bounded to 1 MiB, and timeouts terminate the child process group on Unix.

Built-in adapters run in a temporary evidence directory and restrict agent tools where supported. They do not check out fork code or load its instruction files. Custom commands and user-level CLI configuration can have additional capabilities; Forkpicker does not provide an OS sandbox for arbitrary wrappers.

## Prompts and schemas

The prompts live beside the implementation: [mapping](../src/prompts/map.md), [inspection](../src/prompts/inspect.md), [classification](../src/prompts/classify.md), [related-patch retrieval](../src/prompts/related.md), and [code review](../src/prompts/review.md). Issue matching uses [screening](../src/prompts/issue-screen.md), [confirmation](../src/prompts/issue-confirm.md), and [inline matching](../src/prompts/inline-issues.md).

Classification sends a request-specific JSON Schema through Codex's `--output-schema` or Claude/Grok's `--json-schema`. Use `classify --dry-run --plan-dir DIR` to export exact requests and schemas. Candidate and evidence IDs are enumerated; every supplied candidate must be accounted for once, group claims must cite their members, and dependency claims need both endpoints without cycles. Issue connections require supplied project evidence. Useful/excluded judgments require reasons and cannot override maintainer decisions.

For a custom review, run `context REPORT FEATURE_ID --review` and return its `response_example` shape. The review covers quality, style, security, and correctness, with findings tied to a supplied commit and changed path. Cleanliness and upstream-style judgments need evidence; style comparisons must cite actual upstream excerpts. Empty findings do not prove correctness. Source context comes from pinned Git blobs, including available root convention files, within the request's byte budget.

The separate explanation contract from `context REPORT FEATURE_ID` also supplies a complete response example. `annotate` validates its feature identity, exact upstream SHA, nonempty claims, and citations before attaching it. Unknown fields and invented evidence are rejected. Model output cannot alter raw measurements or report tests as executed. Citation validation establishes traceability, not the truth of the claim.

## Review reuse

Individual reviews are saved immediately and cached by exact input, upstream SHA, prompt/schema, agent profile, and executable identity. Repeating a command resumes matching work. Changes to patches, source context, model, effort, or adapter settings request a new review. Changing issue votes alone does not invalidate a static code review.

Use `--force` to repeat reviews after changing inherited CLI configuration, which Forkpicker cannot inspect. Explicit model/effort settings make the requested configuration reproducible. `review --all` uses visible candidates in author-recency order; `--min-priority` explicitly opts into heuristic filtering. A failure stops the batch and preserves completed results. Discovery and one-command runs have their own fresh/continuation rules in [usage](usage.md#failures-and-continuation).
