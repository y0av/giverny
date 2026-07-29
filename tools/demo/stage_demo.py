#!/usr/bin/env python3
"""Stage a fully mock Giverny environment for README screenshots.

Nothing here touches the real ~/.config/giverny or ~/.claude: the app is
launched with HOME pointed at a scratch tree, so its config, state and
account discovery all resolve inside it.
"""
import json
import os
import shutil
import stat
import time
from pathlib import Path

DEMO = Path("/tmp/giverny-demo")
NOW_MS = int(time.time() * 1000)


def iso_in(hours):
    return time.strftime(
        "%Y-%m-%dT%H:%M:%S+00:00", time.gmtime(time.time() + hours * 3600)
    )


def usage(session, weekly, model_name, model_pct, severity="normal"):
    return {
        "fetchedAtMs": NOW_MS,
        "accountUuid": "demo-uuid",
        "utilization": {
            "limits": [
                {"kind": "session", "group": "session", "percent": session,
                 "severity": "normal", "resets_at": iso_in(3.2), "scope": None,
                 "is_active": True},
                {"kind": "weekly_all", "group": "weekly", "percent": weekly,
                 "severity": severity, "resets_at": iso_in(74), "scope": None,
                 "is_active": False},
                {"kind": "weekly_scoped", "group": "weekly", "percent": model_pct,
                 "severity": severity, "resets_at": iso_in(74),
                 "scope": {"model": {"id": None, "display_name": model_name},
                           "surface": None},
                 "is_active": False},
            ]
        },
    }


def profile(dir_path, email, session, weekly, model_pct, severity="normal"):
    dir_path.mkdir(parents=True, exist_ok=True)
    (dir_path / "projects").mkdir(exist_ok=True)
    (dir_path / "sessions").mkdir(exist_ok=True)
    payload = {
        "oauthAccount": {"emailAddress": email, "accountUuid": f"uuid-{email}"},
        "cachedUsageUtilization": usage(session, weekly, "Fable", model_pct, severity),
    }
    # The default profile keeps its identity file beside the dir, others inside.
    target = (
        dir_path.with_suffix(".json")
        if dir_path.name == ".claude" and dir_path.parent == DEMO
        else dir_path / ".claude.json"
    )
    target.write_text(json.dumps(payload, indent=1))


def repo(path, branch):
    """A directory that reads as a git repo on the given branch."""
    path.mkdir(parents=True, exist_ok=True)
    git = path / ".git"
    git.mkdir(exist_ok=True)
    (git / "HEAD").write_text(f"ref: refs/heads/{branch}\n")


def main():
    if DEMO.exists():
        shutil.rmtree(DEMO)
    DEMO.mkdir(parents=True)

    # --- mock accounts -----------------------------------------------------
    profile(DEMO / ".claude", "ada@example.com", 34, 41, 28)
    profile(DEMO / "envs/lab/claude", "lab@example.com", 8, 12, 9)
    profile(DEMO / "envs/night/claude", "night@example.com", 0, 92, 88, "critical")

    # --- mock projects -----------------------------------------------------
    repo(DEMO / "work/orbital-api", "feat/rate-limits")
    repo(DEMO / "work/atlas-web", "main")
    repo(DEMO / "work/ingest", "fix/backpressure")
    repo(DEMO / "oss/serde-yaml-ng", "main")
    repo(DEMO / "infra/terraform", "prod")

    # --- workspace state ---------------------------------------------------
    state = DEMO / ".config/giverny/state"
    state.mkdir(parents=True)
    (DEMO / ".config/giverny/config.toml").write_text(
        '[font]\nfamily = ""\nsize = 13.0\n\n[theme]\nname = "monet-dark"\n\n'
        "[update]\ncheck = false\n"
    )

    def tab(i, cat, title, cwd, session=None):
        return {
            "id": i,
            "category": cat,
            "custom_title": title,
            "auto_title": title,
            "cwd": str(cwd),
            "claude_session": session,
            "claude_config_dir": None,
        }

    workspace = {
        "next_id": 40,
        "categories": [
            {"id": 1, "name": "work", "color_index": 0, "collapsed": False,
             "profile_dir": None},
            {"id": 2, "name": "open source", "color_index": 1, "collapsed": False,
             "profile_dir": None},
            {"id": 3, "name": "infra", "color_index": 3, "collapsed": False,
             "profile_dir": None},
        ],
        "tabs": [
            tab(10, 1, "orbital-api", DEMO / "work/orbital-api", "aaaaaaaa-1111-2222-3333-444444444444"),
            tab(11, 1, "atlas-web", DEMO / "work/atlas-web", "bbbbbbbb-1111-2222-3333-444444444444"),
            tab(12, 1, "ingest", DEMO / "work/ingest"),
            tab(20, 2, "serde-yaml-ng", DEMO / "oss/serde-yaml-ng", "cccccccc-1111-2222-3333-444444444444"),
            tab(30, 3, "terraform", DEMO / "infra/terraform"),
        ],
        "active": 10,
    }
    (state / "tabs.json").write_text(
        json.dumps(
            {"version": 1, "boot_id": "", "clean_shutdown": True,
             "workspace": workspace, "font_size": 13.0},
            indent=1,
        )
    )

    # --- per-tab screen content -------------------------------------------
    # Every tab runs this as its shell; it prints the scene for its tab id
    # and then idles, so the window shows composed content without anyone
    # driving the UI.
    scenes = DEMO / "scenes"
    scenes.mkdir()
    shell = DEMO / "demo-shell.sh"
    shell.write_text(
        "#!/bin/sh\n"
        'f="$HOME/scenes/${GIVERNY_TAB_ID:-none}.ans"\n'
        '[ -f "$f" ] && cat "$f"\n'
        "exec sleep 100000\n"
    )
    shell.chmod(shell.stat().st_mode | stat.S_IEXEC)
    print(DEMO)


if __name__ == "__main__":
    main()
