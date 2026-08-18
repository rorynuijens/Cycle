# Publishing releases

Cycle is distributed from its own Flatpak repository, served as static files
from the `gh-pages` branch of this repo. Users add the remote once and get
updates from `flatpak update` from then on, exactly as they would from Flathub.

`.github/workflows/release.yml` does the work: tag `vX.Y.Z`, and it builds the
flatpak, commits it into the OSTree repo, republishes `gh-pages`, and attaches a
single-file `.flatpak` bundle to the GitHub release.

---

## One-time setup

### 1. Create a repository signing key

The key signs the OSTree commits and the repo summary. It is *not* a personal
identity key — it identifies the repository, so generate a dedicated one and
leave it without a passphrase (CI cannot type one).

```bash
gpg --batch --quick-gen-key "Cycle Flatpak Repo <noreply@example.invalid>" \
    default default never
gpg --list-keys --with-colons "Cycle Flatpak Repo" | awk -F: '/^fpr/ {print $10; exit}'
```

That fingerprint is the key ID used below. Back the secret key up somewhere
durable — losing it means every existing user has to remove and re-add the
remote:

```bash
gpg --export-secret-keys --armor <KEYID> > ~/cycle-repo-signing-key.asc
```

### 2. Add the two repository secrets

Settings → Secrets and variables → Actions:

| Secret | Value |
|---|---|
| `FLATPAK_GPG_KEY_ID` | the fingerprint from step 1 |
| `FLATPAK_GPG_PRIVATE_KEY` | `gpg --export-secret-keys <KEYID> \| base64 -w0` |

Base64, not armor — it survives the secret store without newline mangling.

### 3. Enable GitHub Pages

Settings → Pages → Source: **Deploy from a branch**, branch `gh-pages`, folder
`/ (root)`.

The branch does not exist until the first release runs, so do this immediately
after the first successful workflow run.

---

## Cutting a release

1. Bump `version` in `Cargo.toml`, then run a build so `Cargo.lock` picks it up.
2. Add a `<release version="X.Y.Z" date="...">` entry with real notes to
   `data/io.github.rorynuijens.Cycle.metainfo.xml`. GNOME Software shows these
   verbatim; the workflow refuses to publish without one.
3. If `Cargo.lock` changed at all, regenerate the vendored source list — the
   flatpak build is offline and fails on a stale one:
   ```bash
   python3 flatpak-cargo-generator.py Cargo.lock -o build-aux/cargo-sources.json
   ```
   (from [flatpak-builder-tools](https://github.com/flatpak/flatpak-builder-tools))
4. Commit, then tag and push:
   ```bash
   git tag v0.2.0 && git push origin main v0.2.0
   ```

`workflow_dispatch` with an explicit version re-runs the same pipeline if a
release needs rebuilding.

---

## What users run

```bash
flatpak remote-add --if-not-exists cycle https://rorynuijens.github.io/Cycle/cycle.flatpakrepo
flatpak install cycle io.github.rorynuijens.Cycle
```

Or, without automatic updates, the bundle attached to any release:

```bash
flatpak install ./cycle-0.2.0.flatpak
```

---

## Notes and limits

- **Repo size.** GitHub Pages allows roughly 1 GB per site and 100 GB of
  bandwidth a month. `build-update-repo --prune-depth=3` keeps the last three
  releases installable and drops the rest, which holds the repo to a few
  hundred MB at most. Static deltas are *not* generated — they trade repo size
  for download size, and size is the scarcer resource here.
- **`gh-pages` history is disposable.** Each release force-pushes a fresh root
  commit. The OSTree repo carries its own history, so git history on that branch
  would only accumulate binary weight.
- **Reachability.** GitHub itself 404s from some networks (including the
  developer's). `github.io` is a separate domain and may behave differently —
  verify `flatpak remote-add` actually works from the network you care about
  before pointing anyone at it. If it does not, the same repository layout
  publishes unchanged to Codeberg Pages; only the URL in `cycle.flatpakrepo`
  changes.
- **Flathub.** Not used. If that ever changes, nothing here conflicts with a
  Flathub submission — the manifest is the same one Flathub would build.
