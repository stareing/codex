# Network approvals / sandbox notes

This file is for the fallback CLI mode only. Read it when the user explicitly asks to use `scripts/image_gen.py` / CLI / API / model controls, or after the user explicitly confirms that a transparent-output request should use the `gpt-image-1.5` true-transparency fallback path.

This guidance is intentionally isolated from `SKILL.md` because it can vary by environment and may become stale. Prefer the defaults in your environment when in doubt.

## Why am I asked to approve image generation calls?
The fallback CLI uses the OpenAI Image API, so it needs outbound network access. In many agent setups, network access is disabled by default and/or the approval policy requires confirmation before networked commands run.

## In the Onigio app
- Local sessions on Windows run in the app's Linux VM with full access and network on: no approval or sandbox setting is needed there, just run the CLI.
- Cloud sessions (the website's cloud workspace) have no outbound internet, so the CLI cannot reach the OpenAI API there; tell the user to run it from a computer session instead.
- Remote-controlled computers follow that computer's own approval and sandbox settings, as below.

## Important note about approvals vs network
- `--ask-for-approval never` suppresses approval prompts.
- It does **not** by itself enable network access.
- In `workspace-write`, network access still depends on your agent configuration (for example `[sandbox_workspace_write] network_access = true`).

## How do I reduce repeated approval prompts?
If you trust the repo and want fewer prompts, use a configuration or profile that both:
- enables network for the sandbox mode you plan to use
- sets an approval policy that matches your risk tolerance

Example `~/.onigio/config.toml` (`$CODEX_HOME/config.toml`) pattern:

```toml
approval_policy = "on-request"
sandbox_mode = "workspace-write"

[sandbox_workspace_write]
network_access = true
```

If you want quieter automation after network is enabled, you can choose a stricter approval policy, but do that intentionally and with care.

## Safety note
Enabling network and reducing approvals lowers friction, but increases risk if you run untrusted code or work in an untrusted repository.
