---
name: skill-installer
description: Install Onigio skills into the personal skills folder (~/.onigio/skills) from a known skills repo (openai/skills, anthropics/skills) or any GitHub repo path. Use when a user asks to list installable skills, install a skill by name, or install a skill from another repo (including private repos).
metadata:
  short-description: Install skills from openai/skills, anthropics/skills or any GitHub repo
---

# Skill Installer

Installs skills for Onigio. A skill is a folder with a `SKILL.md`; any repo that keeps skills that way works. Known sources (pass `--source <name>` to list them):

| `--source` | Repo and path | Notes |
|---|---|---|
| `openai` (default) | https://github.com/openai/skills/tree/main/skills/.curated | curated skills |
| `openai-experimental` | https://github.com/openai/skills/tree/main/skills/.experimental | only when the repo has it; a 404 means it is gone |
| `anthropic` | https://github.com/anthropics/skills/tree/main/skills | some skills (e.g. docx, pdf, pptx, xlsx) carry their own non-open license: point the user at the skill's LICENSE before installing |

Any other GitHub repo or path the user gives works the same way (`--repo owner/repo`, `--url …`).

Use the helper scripts based on the task:
- List skills when the user asks what is available, or uses this skill without saying what to do. Default listing is `openai`; use `--source anthropic` (or `--repo`/`--path`) when they name another source.
- Install by name from a known source when the user gives a skill name: use that source's repo and path (e.g. `--repo openai/skills --path skills/.curated/<name>`).
- Install from another repo when the user gives a GitHub repo/path (including private repos).

## Communication

When listing skills, output approximately as follows, labelling the source:
"""
Skills from {repo}:
1. skill-1
2. skill-2 (already installed)
3. ...
Which ones would you like installed?
"""

After installing a skill, tell the user it will be available on their next turn (in the Onigio app: next message; `/skills` lists it and can switch it off).

## Scripts

All of these scripts use the network. In a sandboxed session request escalation to run them; in the Onigio app's Linux VM there is no sandbox, run them directly.

- `scripts/list-skills.py` (prints skills list with installed annotations; default source `openai`)
- `scripts/list-skills.py --format json`
- `scripts/list-skills.py --source anthropic`
- `scripts/list-skills.py --repo <owner>/<repo> [--path <dir>]` (any repo; `--path` defaults to `skills`)
- `scripts/install-skill-from-github.py --repo <owner>/<repo> --path <path/to/skill> [<path/to/skill> ...]`
- `scripts/install-skill-from-github.py --url https://github.com/<owner>/<repo>/tree/<ref>/<path>`
- Example (Anthropic skill): `scripts/install-skill-from-github.py --repo anthropics/skills --path skills/<skill-name>`

## Behavior and Options

- Defaults to direct download for public GitHub repos.
- If download fails with auth/permission errors, falls back to git sparse checkout.
- Aborts if the destination skill directory already exists.
- Installs into `$CODEX_HOME/skills/<skill-name>`, i.e. `~/.onigio/skills/<skill-name>`: the Onigio agent sets `CODEX_HOME` to its home for every command it runs; run by hand without it, the scripts use `$ONIGIO_HOME`, then `~/.onigio` (never another agent's `~/.codex`). `~` is the home of the machine the agent runs on: in the Onigio app on Windows that is the Linux VM's `/root`. Always install there unless the user asks for one project only.
- Multiple `--path` values install multiple skills in one run, each named from the path basename unless `--name` is supplied.
- Options: `--ref <ref>` (default `main`), `--dest <path>` (e.g. `<project>/.agents/skills` for one project only), `--method auto|download|git`.

## Notes

- Listings are fetched through the GitHub API. If a listing is unavailable (network, rate limit, missing path), explain the error and exit.
- Private GitHub repos can be accessed via existing git credentials or optional `GITHUB_TOKEN`/`GH_TOKEN` for download.
- Git fallback tries HTTPS first, then SSH.
- The bundled system skills (in `$CODEX_HOME/skills/.system`) are preinstalled and refreshed with the agent, so do not install those; if the user asks, explain this. If they insist, install under another `--name`.
- Installed annotations come from `~/.onigio/skills` (and `~/.agents/skills`, which the agent also reads).
