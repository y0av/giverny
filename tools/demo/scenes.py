#!/usr/bin/env python3
"""Terminal content for each demo tab — invented projects, no real data."""
from pathlib import Path

DEMO = Path("/tmp/giverny-demo")
S = "\x1b["
R = f"{S}0m"


def c(code, text):
    return f"{S}{code}m{text}{R}"


dim = lambda t: c("2", t)
teal = lambda t: c("38;2;95;163;163", t)
amber = lambda t: c("38;2;217;181;95", t)
green = lambda t: c("38;2;123;162;90", t)
lav = lambda t: c("38;2;154;134;184", t)
fg = lambda t: c("38;2;215;221;226", t)
poppy = lambda t: c("38;2;195;91;78", t)


def prompt(cwd, branch, cmd=""):
    return (
        f"{green('❯')} {teal(cwd)} {dim('on')} {lav(branch)}\n"
        f"{green('❯')} {fg(cmd)}\n"
    )


def claude_header():
    return (
        f"{dim('╭───────────────────────────────────────────────────────────────╮')}\n"
        f"{dim('│')} {amber('✻')} {fg('Claude Code')} {dim('v2.1.220')}                                    {dim('│')}\n"
        f"{dim('╰───────────────────────────────────────────────────────────────╯')}\n\n"
    )


def user(text):
    return f"{S}48;2;30;38;42m{S}38;2;215;221;226m❯ {text}{' ' * max(0, 66 - len(text))}{R}\n\n"


def bullet(text, mark="●", color=teal):
    return f"  {color(mark)} {fg(text)}\n"


def tool(name, detail):
    return f"  {dim('⎿')}  {lav(name)} {dim(detail)}\n"


SCENES = {
    # Active tab: a Claude session mid-work.
    "giverny-10": (
        prompt("~/work/orbital-api", "feat/rate-limits", "claude")
        + claude_header()
        + user("add a token-bucket limiter to the ingest endpoint")
        + bullet("Reading the current middleware stack.")
        + tool("Read", "src/http/middleware.rs (218 lines)")
        + tool("Grep", '"RateLimit" — 3 matches in 2 files')
        + bullet("The stack already threads a Clock trait, so the limiter can")
        + f"    {fg('borrow it instead of calling Instant::now directly.')}\n"
        + tool("Edit", "src/http/limiter.rs +94 −0")
        + tool("Edit", "src/http/middleware.rs +12 −3")
        + bullet("Tests: 41 passed, 0 failed.", "●", green)
        + "\n"
        + f"  {amber('✳')} {fg('Puttering…')} {dim('(esc to interrupt)')}\n"
    ),
    # Background tab that finished.
    "giverny-11": (
        prompt("~/work/atlas-web", "main", "claude")
        + claude_header()
        + user("why is the dashboard bundle 900kb?")
        + bullet("Two copies of date-fns are being pulled in — the chart")
        + f"    {fg('package pins 2.x while the app is on 3.x.')}\n"
        + tool("Read", "package-lock.json")
        + bullet("Deduping saves 310kb before compression.", "●", green)
        + f"\n{green('❯')} {dim('')}\n"
    ),
    # Tab where Claude is waiting on the user.
    "giverny-12": (
        prompt("~/work/ingest", "fix/backpressure", "claude")
        + claude_header()
        + user("drop the stale partitions and re-run the backfill")
        + bullet("This deletes 4 partitions (about 2.1M rows).")
        + "\n"
        + f"  {amber('╭─────────────────────────────────────────────────╮')}\n"
        + f"  {amber('│')} {fg('Run this command?')}                              {amber('│')}\n"
        + f"  {amber('│')}                                                 {amber('│')}\n"
        + f"  {amber('│')} {poppy('psql -c \"DROP TABLE events_2025_q{1,2,3,4}\"')}    {amber('│')}\n"
        + f"  {amber('│')}                                                 {amber('│')}\n"
        + f"  {amber('│')} {fg('❯ 1. Yes    2. Yes, and do not ask again')}        {amber('│')}\n"
        + f"  {amber('│')} {fg('  3. No, tell Claude what to do differently')}     {amber('│')}\n"
        + f"  {amber('╰─────────────────────────────────────────────────╯')}\n"
    ),
    "giverny-20": (
        prompt("~/oss/serde-yaml-ng", "main", "cargo test")
        + f"{dim('   Compiling serde-yaml-ng v0.10.0')}\n"
        + f"{dim('    Finished `test` profile in 4.21s')}\n\n"
        + f"{green('test result: ok.')} {fg('182 passed; 0 failed; 3 ignored')}\n\n"
        + f"{green('❯')} {dim('')}\n"
    ),
    "giverny-30": (
        prompt("~/infra/terraform", "prod", "terraform plan")
        + f"{fg('Terraform will perform the following actions:')}\n\n"
        + f"  {green('+')} {fg('module.edge.aws_cloudfront_distribution.cdn')}\n"
        + f"  {amber('~')} {fg('module.api.aws_ecs_service.api')}\n"
        + f"      {dim('desired_count: 4 → 6')}\n\n"
        + f"{fg('Plan:')} {green('1 to add')}, {amber('1 to change')}, {fg('0 to destroy.')}\n\n"
        + f"{green('❯')} {dim('')}\n"
    ),
}


def main():
    out = DEMO / "scenes"
    out.mkdir(parents=True, exist_ok=True)
    for name, body in SCENES.items():
        (out / f"{name}.ans").write_text(body)
    print(f"wrote {len(SCENES)} scenes")


if __name__ == "__main__":
    main()
