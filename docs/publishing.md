# Publishing releases

Cycle is not on Flathub. It is published from **its own Flatpak repository** — which is just a
folder of files at a web address that `flatpak` knows how to download from. Users add that address
once, and `flatpak update` brings them new versions from then on, exactly as it would from Flathub.

| | |
|---|---|
| Source | `https://github.com/rorynuijens/Cycle` |
| Flatpak repo | `https://rorynuijens.github.io/Cycle/repo/` |
| Built by | `.github/workflows/release.yml` |

---

## How it works, in one paragraph

You put a **tag** on a commit — a label saying "this exact code is version 0.1.0". GitHub notices
the tag and runs the workflow: it compiles the app, adds the result to the Flatpak repository, and
pushes that repository to a branch called `gh-pages`, which GitHub Pages serves on the web. It also
creates a release page with a single downloadable `.flatpak` file attached. Each build is stamped
with a **signing key** so users' computers can check the files really came from you.

---

## One-time setup

Do this once. It takes about ten minutes.

### 1. Make the signing key

On your laptop:

```bash
# create the key — no passphrase, because the build server cannot type one
gpg --batch --passphrase '' --quick-gen-key \
    "Cycle Flatpak Repo <noreply@example.invalid>" default default never

# find its ID
KEYID=$(gpg --list-keys --with-colons "Cycle Flatpak Repo" | awk -F: '/^fpr/ {print $10; exit}')
echo "Your key ID: $KEYID"

# write out the two values you will paste into GitHub
echo "$KEYID" > ~/cycle-secret-key-id.txt
gpg --export-secret-keys "$KEYID" | base64 -w0 > ~/cycle-secret-private-key.txt

# and a backup of the key itself — keep this somewhere safe
gpg --export-secret-keys --armor "$KEYID" > ~/cycle-repo-signing-key.asc
```

**Back up `cycle-repo-signing-key.asc`.** If you lose it, every existing user has to remove and
re-add the remote before they can update again.

### 2. Add two secrets

Go to <https://github.com/rorynuijens/Cycle/settings/secrets/actions> and click *New repository
secret* twice. The names must match exactly.

| Name | Value |
|---|---|
| `FLATPAK_GPG_KEY_ID` | contents of `~/cycle-secret-key-id.txt` |
| `FLATPAK_GPG_PRIVATE_KEY` | contents of `~/cycle-secret-private-key.txt` (one very long line) |

Then delete `~/cycle-secret-private-key.txt` — it is your private key in plain text. Keep the
`.asc` backup.

No access token is needed; the workflow uses the one GitHub Actions provides to itself.

### 3. Turn on Pages — but only after the first release

The `gh-pages` branch does not exist until the first successful run creates it, so this step comes
*after* cutting the first tag, not before. Once the workflow has gone green:

<https://github.com/rorynuijens/Cycle/settings/pages> → Source: *Deploy from a branch* →
Branch `gh-pages`, folder `/ (root)` → Save.

Give it a couple of minutes, then `https://rorynuijens.github.io/Cycle/` should show the install
page.

---

## Cutting a release

The first one needs **no file edits at all**: `Cargo.toml` already says `0.1.0` and the metainfo
already has a matching entry, so this works as-is —

```bash
git tag v0.1.0
git push origin v0.1.0
```

Watch it at <https://github.com/rorynuijens/Cycle/actions>. Expect 15–30 minutes; it compiles
everything from scratch. Mistakes are caught in the first minute, before the build starts.

For every release after that:

1. Change `version` in `Cargo.toml` (e.g. to `0.2.0`) and run `cargo build` so `Cargo.lock` picks
   the number up.
2. Add an entry at the top of the `<releases>` list in
   `data/io.github.rorynuijens.Cycle.metainfo.xml`:
   ```xml
   <release version="0.2.0" date="2026-08-20">
     <description><p>What changed, in a sentence or two.</p></description>
   </release>
   ```
   GNOME Software shows this text verbatim. The workflow refuses to publish without it.
3. Commit, then tag and push:
   ```bash
   git tag v0.2.0
   git push origin main v0.2.0
   ```

The tag, `Cargo.toml` and the metainfo must all agree on the version, or the workflow stops.

**You do not need to regenerate `build-aux/cargo-sources.json` for a version bump.** That file
lists your *dependencies*, and bumping your own version does not change them. Regenerate it only
when you add or update a crate — the flatpak build is offline and fails on a stale list:

```bash
python3 flatpak-cargo-generator.py Cargo.lock -o build-aux/cargo-sources.json
```

(from [flatpak-builder-tools](https://github.com/flatpak/flatpak-builder-tools))

---

## Checking it worked, as a user would

Your current copy was installed from a local folder, so remove it first. **This does not delete
your rides** — they live in `~/.var/app/io.github.rorynuijens.Cycle` and stay put.

```bash
flatpak uninstall io.github.rorynuijens.Cycle
flatpak remote-add --if-not-exists cycle https://rorynuijens.github.io/Cycle/cycle.flatpakrepo
flatpak install cycle io.github.rorynuijens.Cycle
```

---

## When something goes wrong

Open the failed run under the *Actions* tab. Step names say where it broke:

| Step | Meaning |
|---|---|
| *Resolve and verify the version* | the tag, `Cargo.toml` and the metainfo disagree |
| *Import the repo signing key* | a secret is missing, misnamed, or pasted wrong — it must be the base64 line |
| *Build and commit to the repo* | a real compile failure, or a stale `cargo-sources.json` |
| *Publish to gh-pages* | the workflow could not push; check Actions has write permission |

A tag is not permanent. To scrap one and try again:

```bash
git tag -d v0.1.0                    # delete locally
git push origin :refs/tags/v0.1.0    # delete on GitHub
```

Fix the problem and tag again with the same number.

---

## Notes and limits

- **Repo size.** GitHub Pages allows roughly 1 GB per site and 100 GB of bandwidth a month.
  `--prune-depth=3` keeps the last three releases installable and drops the rest, which holds this
  to a few hundred MB. Static deltas are deliberately *not* generated — they trade repository size
  for download size, and size is the scarcer resource here.
- **`gh-pages` history is disposable.** Each release force-pushes a fresh root commit. The Flatpak
  repository carries its own history internally; git history on that branch would only accumulate
  binary weight.
- **Flathub.** Not used. Nothing here conflicts with a Flathub submission if that ever changes —
  the manifest is the same one Flathub would build.
