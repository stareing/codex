#!/usr/bin/env python3
"""List skills from a GitHub repo path."""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error

from github_utils import github_api_contents_url, github_request
from skill_home import DEFAULT_SOURCE, SOURCES, installed_skills

DEFAULT_REF = "main"


class ListError(Exception):
    pass


class Args(argparse.Namespace):
    source: str
    repo: str | None
    path: str | None
    ref: str
    format: str


def _request(url: str) -> bytes:
    return github_request(url, "onigio-skill-list")


def _repo_and_path(args: Args) -> tuple[str, str]:
    """`--repo`/`--path` win over `--source`; a `--repo` without `--path` lists `skills`."""
    source_repo, source_path = SOURCES[args.source]
    if args.repo:
        return args.repo, args.path or "skills"
    return source_repo, args.path or source_path


def _list_skills(repo: str, path: str, ref: str) -> list[str]:
    api_url = github_api_contents_url(repo, path, ref)
    try:
        payload = _request(api_url)
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            raise ListError(
                f"Skills path not found: https://github.com/{repo}/tree/{ref}/{path}"
            ) from exc
        raise ListError(f"Failed to fetch skills: HTTP {exc.code}") from exc
    data = json.loads(payload.decode("utf-8"))
    if not isinstance(data, list):
        raise ListError("Unexpected skills listing response.")
    skills = [item["name"] for item in data if item.get("type") == "dir"]
    return sorted(skills)


def _parse_args(argv: list[str]) -> Args:
    parser = argparse.ArgumentParser(description="List skills.")
    parser.add_argument(
        "--source",
        choices=sorted(SOURCES),
        default=DEFAULT_SOURCE,
        help="A known skills repo (default: openai = openai/skills skills/.curated)",
    )
    parser.add_argument("--repo", help="Any GitHub owner/repo instead of --source")
    parser.add_argument(
        "--path",
        help="Repo path to list (default: the source's, or skills with --repo)",
    )
    parser.add_argument("--ref", default=DEFAULT_REF)
    parser.add_argument(
        "--format",
        choices=["text", "json"],
        default="text",
        help="Output format",
    )
    return parser.parse_args(argv, namespace=Args())


def main(argv: list[str]) -> int:
    args = _parse_args(argv)
    try:
        repo, path = _repo_and_path(args)
        skills = _list_skills(repo, path, args.ref)
        installed = installed_skills()
        if args.format == "json":
            payload = [
                {"name": name, "installed": name in installed} for name in skills
            ]
            print(json.dumps(payload))
        else:
            for idx, name in enumerate(skills, start=1):
                suffix = " (already installed)" if name in installed else ""
                print(f"{idx}. {name}{suffix}")
        return 0
    except ListError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
