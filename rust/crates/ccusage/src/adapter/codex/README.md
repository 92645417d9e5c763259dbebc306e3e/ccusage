# Codex Source

Data source:

```text
${CODEX_HOME:-~/.codex}/sessions/
${CODEX_HOME:-~/.codex}/archived_sessions/
```

When both directories contain the same relative JSONL path for one Codex home,
the active `sessions/` copy wins.

Relevant JSONL event:

- `type === "event_msg"`
- `payload.type === "token_count"`
- `payload.info.total_token_usage` is cumulative.
- `payload.info.last_token_usage` is the current turn delta.
- If only cumulative totals exist, subtract prior totals to recover deltas.

Relevant speed-setting event in Codex CLI 0.144.0 and later:

- `type === "event_msg"`
- `payload.type === "thread_settings_applied"`
- `payload.thread_settings.service_tier === "priority"` (or legacy `"fast"`) selects Fast.
- `payload.thread_settings.service_tier === "default"` selects Standard. Codex Desktop spells the same tier `"standard"`; both appear in the same CLI version, so this is a value mapping and not a version split.
- Token usage inherits the latest recognized setting in the rollout. Missing or unsupported settings remain unclassified so report policy can apply its documented fallback.

Token mapping:

- `input_tokens` - total input tokens.
- `cached_input_tokens` - cached prompt tokens.
- `output_tokens` - completion tokens, including reasoning cost.
- `reasoning_output_tokens` - informational breakdown; already included in output billing.
- `total_tokens` - provided directly or recomputed as input plus output for legacy entries.

Pricing uses model metadata from `turn_context`. Early sessions without metadata fall back to `gpt-5`, mark `isFallbackModel === true`, and expose fallback rows as approximate in aggregate JSON.
