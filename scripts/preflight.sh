#!/usr/bin/env bash
#
# Check everything that can fail a release, before the tag is pushed.
#
# Both gates in .github/workflows/release.yml only fail *after* a tag exists, and
# a failed run costs the whole release. This runs the same two checks locally,
# plus the ones the workflow does not do at all.
#
# Usage: scripts/preflight.sh v0.6.0
#
# Run it inside the Cycle toolbox — the host has no cargo.

set -euo pipefail

APP_ID=io.github.rorynuijens.Cycle
METAINFO="data/$APP_ID.metainfo.xml"
WORKFLOW=.github/workflows/release.yml
BROKEN_OBJECT=https://dl.flathub.org/repo/objects/06/0fdf65e0a1042c0db51bf9d009048f9f07e047b422e0e736c5d8ad35ac3a9c.filez

# Every command substitution below ends in `|| true`: under `set -e` an
# assignment from a failing command kills the script, and a missing version
# string is exactly the case this is meant to report on rather than die on.
failures=0
warnings=0

fail() {
    echo "FAIL: $*"
    failures=$((failures + 1))
}

warn() {
    echo "warning: $*"
    warnings=$((warnings + 1))
}

if [ $# -ne 1 ]; then
    echo "Usage: $0 vX.Y.Z" >&2
    exit 2
fi

tag="$1"
version="${tag#v}"
echo "Checking a release of $version (tag $tag)"
echo

# ── The two gates the release workflow actually runs ──────────────────────────
#
# Replicated byte for byte from release.yml:68-81 so they cannot drift.

cargo_version=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2 || true)
if [ "$cargo_version" != "$version" ]; then
    fail "tag says $version but Cargo.toml says $cargo_version (release.yml gate 1)"
else
    echo "Cargo.toml is $cargo_version"
fi

if ! grep -q "<release version=\"$version\"" "$METAINFO"; then
    fail "$METAINFO has no <release version=\"$version\"> entry (release.yml gate 2)"
else
    echo "$METAINFO has a <release> entry for $version"
fi

# ── The checks the workflow does not do ───────────────────────────────────────

# meson.build is declarative and nothing gates it, so it drifts silently. The
# About dialog reads CARGO_PKG_VERSION, but keeping the two in step is the point.
meson_version=$(grep -m1 "^  version: " meson.build | cut -d"'" -f2 || true)
if [ "$meson_version" != "$version" ]; then
    fail "meson.build says $meson_version, not $version (nothing in CI checks this)"
else
    echo "meson.build is $meson_version"
fi

# The release list is the changelog GNOME Software shows, and the gate above
# ignores the date entirely. Dating an entry to the day it was drafted rather
# than the day it ships has already had to be corrected by hand once (d83f4e3).
release_date=$(grep -m1 "<release version=\"$version\"" "$METAINFO" |
    sed -n 's/.*date="\([0-9-]*\)".*/\1/p' || true)
today=$(date +%Y-%m-%d)
if [ -z "$release_date" ]; then
    fail "the <release> entry for $version has no date attribute"
elif [ "$release_date" != "$today" ]; then
    warn "the <release> entry is dated $release_date, and today is $today"
else
    echo "the <release> entry is dated today"
fi

# The flatpak build runs with CARGO_NET_OFFLINE=true and fails on a stale vendor
# list — 15 to 30 minutes in. Compare the sets, never the mtimes: Cargo.lock is
# routinely newer than the JSON purely from the local package's own version bump,
# which never appears in the vendored list.
if python3 - "$version" <<'PY'
import json, re, sys

version = sys.argv[1]
lock = open("Cargo.lock").read()
packages = re.findall(r'\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"\n(.*?)(?=\n\[\[|\Z)',
                      lock, re.S)
want = {f"{n}-{v}" for n, v, rest in packages if 'source = "registry' in rest}

sources = json.load(open("build-aux/cargo-sources.json"))
have = {a["dest"].split("/")[-1] for a in sources if a.get("type") == "archive"}

missing = sorted(want - have)
stale = sorted(have - want)
if missing or stale:
    for m in missing:
        print(f"  missing from cargo-sources.json: {m}")
    for s in stale:
        print(f"  in cargo-sources.json but not the lock: {s}")
    sys.exit(1)
print(f"cargo-sources.json matches Cargo.lock ({len(want)} crates)")
PY
then :; else
    fail "build-aux/cargo-sources.json is out of step with Cargo.lock — regenerate it:
      python3 build-aux/flatpak-cargo-generator.py Cargo.lock -o build-aux/cargo-sources.json"
fi

# Free, and nothing anywhere validates the metainfo schema. Not --pedantic: the
# component ID has a capital C to match the app ID and the repo, and that is not
# going to change.
if command -v appstreamcli > /dev/null; then
    if appstreamcli validate --no-net "$METAINFO" > /dev/null 2>&1; then
        echo "$METAINFO validates"
    else
        fail "appstreamcli rejects $METAINFO:"
        appstreamcli validate --no-net "$METAINFO" || true
    fi
else
    warn "appstreamcli not found — skipping metainfo validation"
fi

# git does not track empty directories, so the workflow recreates the six OSTree
# needs after restoring gh-pages. Dropping one reintroduces a failure that cannot
# happen on a first release and always happens on the second (b3ecd53).
for dir in refs/heads refs/mirrors refs/remotes state tmp/cache extensions; do
    if ! grep -q "repo/$dir" "$WORKFLOW"; then
        fail "$WORKFLOW no longer recreates repo/$dir — the second release will fail on it"
    fi
done
echo "the workflow still recreates the six OSTree directories"

if git rev-parse -q --verify "refs/tags/$tag" > /dev/null; then
    fail "$tag already exists locally — 'git tag -d $tag' first if you mean to move it"
fi

if [ -n "$(git status --porcelain)" ]; then
    fail "the working tree is dirty — a tag would ship something other than what was reviewed"
fi

if [ -n "$(git log --branches --not --remotes --oneline)" ]; then
    warn "there are unpushed commits; push main before tagging"
fi

# The 0.5.0 release died here. The pin in c36547d + 4ba939e is the way round it,
# and 4ba939e is the half that actually works.
if curl -sfI --max-time 20 "$BROKEN_OBJECT" > /dev/null 2>&1; then
    echo "the Flathub object that broke 0.5.0 is being served again"
else
    warn "Flathub is still missing the object that broke the 0.5.0 release.
      The build dependencies must stay pinned, or this release will fail the
      same way. See docs and the c36547d + 4ba939e pair."
fi

# CI runs on main and pull requests, never on tags, so without this a tag ships
# having never been through fmt, clippy or the tests.
echo
echo "Running the checks CI would run on main, which it does not run on tags"
if ! cargo fmt --all --check; then
    fail "cargo fmt --all --check reports changes"
fi
if ! cargo clippy --all-targets -- -D warnings; then
    fail "cargo clippy -- -D warnings does not pass"
fi
if ! cargo test --all; then
    fail "cargo test --all does not pass"
fi

echo
if [ "$failures" -gt 0 ]; then
    echo "$failures blocking problem(s), $warnings warning(s). Do not tag."
    exit 1
fi
echo "Ready to tag $tag. $warnings warning(s)."
