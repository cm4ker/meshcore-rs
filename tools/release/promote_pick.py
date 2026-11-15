#!/usr/bin/env python3
"""Which dev release promote.yml makes stable, if any.

A dev release becomes stable once it has run on the test nodes for SOAK_DAYS (7) days without a
rollback. The test nodes follow dev and take every release, so a release has run that long only
while no newer dev release came out: the one promoted is the newest dev release, once it is
SOAK_DAYS old, newer than the last stable one, and not held. A release is held by a line `hold` in
its notes (edit the release on GitHub): that is how a rollback, or anything else seen on the test
nodes, keeps it from stable. By hand (TAG) any dev release newer than the last stable one is
promoted, held or not; FORCE skips the wait.

Its stable name is the dev one without `-dev.N` (0.9.0-dev.3 -> 0.9.0), or VERSION. The release
number stays: the devices on dev run these bytes already and take nothing.

Prints `key=value` lines for $GITHUB_OUTPUT (tag, dev_version, version, counter, dev_versions:
the dev releases since the last stable, oldest first, whose changelog sections make the notes),
nothing when there is nothing to promote; why, on stderr.
"""

import datetime
import json
import os
import re
import subprocess
import sys

TAG_RE = re.compile(r"^v(?P<version>.+)-r(?P<counter>\d+)$")
HOLD_RE = re.compile(r"^\s*hold\s*$", re.IGNORECASE | re.MULTILINE)


def gh(*args):
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True).stdout


def list_releases():
    rows = json.loads(gh("release", "list", "--limit", "100", "--exclude-drafts", "--json", "tagName,isPrerelease,publishedAt"))
    out = []
    for r in rows:
        m = TAG_RE.match(r["tagName"])
        if m:
            out.append({
                "tag": r["tagName"],
                "version": m["version"],
                "counter": int(m["counter"]),
                "dev": r["isPrerelease"],
                "published": datetime.datetime.fromisoformat(r["publishedAt"].replace("Z", "+00:00")),
            })
    return out


def release_notes(tag):
    return json.loads(gh("release", "view", tag, "--json", "body"))["body"] or ""


def pick(releases, notes, now, soak_days, tag="", version="", force=False):
    """The release to promote and its stable name, or (None, why)."""
    stable = [r for r in releases if not r["dev"]]
    last_stable = max((r["counter"] for r in stable), default=0)
    dev = sorted((r for r in releases if r["dev"]), key=lambda r: r["counter"])
    soak = datetime.timedelta(days=soak_days)
    if tag:
        found = next((r for r in releases if r["tag"] == tag), None)
        if not found or not found["dev"]:
            return None, f"{tag} is not a dev release"
        held = False  # by hand: whoever runs it has looked
    else:
        if not dev:
            return None, "no dev release"
        found = dev[-1]
        held = bool(HOLD_RE.search(notes(found["tag"])))
    if found["counter"] <= last_stable:
        return None, f"{found['tag']}: stable release #{last_stable} is out already"
    age = now - found["published"]
    if age < soak and not force:
        due = (found["published"] + soak).strftime("%Y-%m-%d %H:%M UTC")
        return None, f"{found['tag']} has been out for {age.days} days: stable on {due}, unless a newer dev release comes out first"
    if held:
        return None, f"{found['tag']} is held (a line `hold` in its notes)"
    name = version or re.sub(r"-dev\.\d+$", "", found["version"])
    if name == found["version"]:
        return None, f"{found['tag']}: its name {name} has no -dev.N to drop; give the stable name (version)"
    if any(r["version"] == name for r in stable):
        return None, f"{name} is out already as a stable release: the dev releases after a stable one are named after the next version (e.g. {bump(name)}-dev.1)"
    since = [r["version"] for r in dev if last_stable < r["counter"] <= found["counter"]]
    return {"tag": found["tag"], "dev_version": found["version"], "version": name, "counter": found["counter"], "dev_versions": " ".join(since)}, ""


def bump(version):
    parts = version.split(".")
    if parts and parts[-1].isdigit():
        parts[-1] = str(int(parts[-1]) + 1)
    return ".".join(parts)


def main():
    env = os.environ
    chosen, why = pick(
        list_releases(),
        release_notes,
        datetime.datetime.now(datetime.timezone.utc),
        int(env.get("SOAK_DAYS", "7")),
        tag=env.get("TAG", "").strip(),
        version=env.get("VERSION", "").strip(),
        force=env.get("FORCE", "false") == "true",
    )
    if not chosen:
        print(f"nothing to promote: {why}", file=sys.stderr)
        # a run by hand that promotes nothing failed; the daily one just had nothing to do
        sys.exit(1 if env.get("TAG", "").strip() else 0)
    for k, v in chosen.items():
        print(f"{k}={v}")
    print(f"promoting {chosen['tag']} as {chosen['version']}", file=sys.stderr)


if __name__ == "__main__":
    main()
