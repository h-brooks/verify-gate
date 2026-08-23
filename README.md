# verify-gate

[![CI](https://github.com/h-brooks/verify-gate/actions/workflows/ci.yml/badge.svg)](https://github.com/h-brooks/verify-gate/actions/workflows/ci.yml)

A Claude Code `Stop` hook that refuses to let an agent end its turn after
editing files without running anything to check the edit. It reads the
harness's own transcript, so it works regardless of whether the agent
cooperates or remembers to verify.

## Install

Build the binary and put it on `PATH`:

```sh
cargo build --release
cp target/release/verify-gate /usr/local/bin/verify-gate
```

Then run `verify-gate init` inside a project to write `.verify-gate.toml`
and print the hooks snippet below.

Add this to `.claude/settings.json` (project or user level):

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          { "type": "command", "command": "verify-gate hook" }
        ]
      }
    ]
  }
}
```

## Commands

- `verify-gate hook`: reads the Stop-hook JSON payload from stdin, applies
  the rule below, and (only when it blocks) prints
  `{"decision":"block","reason":"..."}` to stdout. Always exits `0`; the
  block signal is the JSON, not the exit code, per the Stop hook contract.
  `--dry-run`, or the `VERIFY_GATE_DISABLE=1` environment variable, skips
  the rule and always allows (and says so on stderr).
- `verify-gate check <transcript.jsonl> [--format text|json]`: runs the
  same rule directly over a saved transcript file. This is the test seam,
  and it's also useful for replaying the rule over recorded transcripts in
  CI. Exits `1` if the rule would block, `0` otherwise.
- `verify-gate init`: writes `.verify-gate.toml` with the built-in
  defaults spelled out (never overwrites an existing file) and prints the
  settings.json snippet above.

## The rule

Scan the transcript (main-session records only; anything with
`isSidechain: true` belongs to a subagent and is ignored) for the last
qualifying **edit**: a call to a tool in `edit_tools`, or a `Bash` command
matching `edit_command_patterns`, excluding any target path that matches
`ignore_paths`.

- No qualifying edit anywhere → **allow**.
- Fewer than `min_edits` qualifying edits have piled up since the last
  verification → **allow** (lets a single trivial edit through without
  nagging; raise `min_edits` to require more before the gate engages).
- No **verification** call (a tool matching `verify_tools`, or a `Bash`
  command matching `verify_patterns`) with a result that actually came back
  happened after that edit → **block**, naming every file edited since the
  last verification. A verification tool call with no matching result (e.g.
  the user interrupted it) does not count.
- Any verification after the edit came back with `is_error: true` →
  **block**, quoting the first 200 characters of the most recent failing
  result.
- Otherwise → **allow**.

`stop_hook_active: true` in the hook payload always allows, regardless of
the above: the agent is already mid-retry from a previous block, and
blocking again would loop forever.

## Config reference (`.verify-gate.toml`)

Looked for first in the hook's `cwd` (or the current directory for
`check`/`init`), then `~/.config/verify-gate/config.toml`, then built-in
defaults. Only the first file found is read; any field it omits falls back
to the default shown by `verify-gate init`. A config file that fails to
parse is reported on stderr and ignored (defaults are used instead) rather
than crashing the hook.

| Field | Default | Meaning |
|---|---|---|
| `edit_tools` | `Edit`, `Write`, `MultiEdit`, `NotebookEdit` | Tool names that always count as an edit. |
| `edit_command_patterns` | `sed -i`, `tee`, `> file` / `>> file`, `cat >`, `git apply`, `patch` | Regexes; a matching `Bash` command also counts as an edit. |
| `ignore_paths` | `**/*.md`, `**/.claude/**`, `**/memory/**`, `**/scratchpad/**` | Globs; edits to a matching path never require verification. |
| `verify_patterns` | `cargo test/build/...`, `npm/pnpm/yarn/bun test/run`, `pytest`, `go test`, `make`, `curl `, `playwright`, `tsc`, `eslint`, `ruff`, `mypy`, `screenshot`, ... | Regexes; a matching `Bash` command counts as verification. |
| `verify_tools` | `^mcp__(playwright\|claude-in-chrome)` | Regexes on tool name; a match counts as verification (browser-automation MCP tools). |
| `min_edits` | `1` | Qualifying edits that must have piled up since the last verification before the gate blocks. |

The glob matcher for `ignore_paths` is a small hand-rolled `*`/`**`
translator, not a full glob spec; see below.

## What it can't catch

- **Subagent (Task-delegated) edits.** Records with `isSidechain: true` are
  skipped entirely (see "The rule" above), so an edit made by a subagent
  (the normal way large or parallel edits get delegated) is invisible to the
  gate even if the main session never verifies it afterwards.
- **Any file path it can't find.** `Edit`/`Write`/`MultiEdit`/`NotebookEdit`
  are read from `file_path` / `path` / `notebook_path` in the tool input.
  For a `Bash` edit command, the edited file is guessed by picking the last
  path-looking token in the command. This is a heuristic and can point at
  the wrong argument or none at all (in which case the whole command is
  reported as the "file" and `ignore_paths` can't apply to it).
- **Verification of the wrong thing.** Running `cargo test` after editing
  an unrelated file still counts as verification: the rule only checks
  *ordering* (a verification call happened after the edit), not that the
  verification actually exercised the edited code.
- **Verification outside the transcript.** A human running the app
  manually, or a check performed by a different tool the transcript
  doesn't capture, won't be seen.
- **Non-standard glob syntax** in `ignore_paths` beyond `*` and `**`
  (no `?`, `[abc]`, brace expansion, etc.).
- **A hook timeout or crash mid-evaluation** on a pathological transcript
  is turned into "allow" by design (see Safety below), which means a bug in
  this tool fails open, not closed.

## Safety

`hook` never raises the harness's exit code and never lets an internal
error (bad JSON, a missing file, even a panic) propagate: every failure
path logs to stderr and allows. The intent is that a bug in verify-gate can
make it a no-op, never a hang or a crash of the agent's turn.

Transcript files are read line-by-line with a buffered reader and never
loaded whole into memory, since real transcripts can run past 60MB.
