"""Where skills are installed and which repos the helper scripts know by name.

The branded agent (`onigio`) exports `CODEX_HOME=~/.onigio` to every tool it runs, so inside a session the
home is always `$CODEX_HOME`. When a script is run by hand without it, `$ONIGIO_HOME`, then `~/.onigio`,
are used, so it never writes into another agent's `~/.codex`.
"""

from __future__ import annotations

import os

# name -> (owner/repo, path inside the repo that holds one folder per skill)
SOURCES: dict[str, tuple[str, str]] = {
    "openai": ("openai/skills", "skills/.curated"),
    "openai-experimental": ("openai/skills", "skills/.experimental"),
    "anthropic": ("anthropics/skills", "skills"),
}
DEFAULT_SOURCE = "openai"


def agent_home() -> str:
    for var in ("CODEX_HOME", "ONIGIO_HOME"):
        value = os.environ.get(var)
        if value:
            return value
    return os.path.expanduser("~/.onigio")


def install_root() -> str:
    """The default destination: `<agent home>/skills` (`~/.onigio/skills`), Onigio's personal skills folder."""
    return os.path.join(agent_home(), "skills")


def installed_roots() -> list[str]:
    """Every folder the agent loads personal skills from: `~/.onigio/skills`, and `~/.agents/skills`, which the
    upstream loader also reads (upstream e24058b7a8); Onigio documents and installs into the first one only."""
    return [install_root(), os.path.join(os.path.expanduser("~"), ".agents", "skills")]


def installed_skills() -> set[str]:
    names: set[str] = set()
    for root in installed_roots():
        if not os.path.isdir(root):
            continue
        for name in os.listdir(root):
            if not name.startswith(".") and os.path.isdir(os.path.join(root, name)):
                names.add(name)
    return names
