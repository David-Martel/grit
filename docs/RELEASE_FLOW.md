# Branching & Release Flow

grit uses a two-branch model, mirroring the flow used across the rtk-ai org
(e.g. `rtk-ai/icm`).

```
 contributor PRs                          maintainer release
       │                                         │
       ▼                                         ▼
   ┌────────┐   "Next Release" PR (auto)   ┌──────────┐
   │ develop │ ───────────────────────────▶│  master  │
   └────────┘                              └──────────┘
       ▲         back-merge PR (auto)            │
       └───────────────────────────────────────-┘
                                          release-please tag + GitHub release
```

- **`develop`** — integration branch. All feature/fix PRs target `develop`.
- **`master`** — stable release branch. Only the maintainer-cut
  `develop -> master` PR lands here; release-please then tags and publishes.

## Workflows

| File | Trigger | What it does |
|------|---------|--------------|
| `ci.yml` | push / PR to `develop` or `master` | build, test (macOS/Linux/Windows), `cargo fmt --check`, `clippy -D warnings` |
| `next-release.yml` | PR merged into `develop` | maintains a persistent `develop -> master` **"Next Release"** PR, classifying each merged PR into Feats / Fix / Other |
| `pr-target-check.yml` | PR opened/edited | flags contributor PRs that target `master` instead of `develop` (label `wrong-base` + comment) |
| `release-please.yml` | push to `master` | release-please PR → tag `vX.Y.Z` → build assets → update `latest` tag → **back-merge `master` into `develop`** |

## One-time repository setup

These cannot be configured from the workflow files and must be set once by a
maintainer:

1. **Default branch / base** — set the default PR base branch to `develop`
   (Settings → General). The default branch can stay `master` for releases, but
   new PRs should open against `develop`.
2. **Branch protection** — protect both `master` and `develop`: require the
   `CI` checks to pass; restrict direct pushes to `master`.
3. **Labels** — create the labels `next-release`, `wrong-base`, and `automated`
   (Settings → Labels), used by the workflows above.
4. **Allow Actions to create PRs** — Settings → Actions → General → "Allow
   GitHub Actions to create and approve pull requests" must be enabled, since
   `next-release.yml` and the back-merge job open PRs with `GITHUB_TOKEN`.

## Token note

The automation uses the built-in `GITHUB_TOKEN` (no stored secrets required).
One caveat: PRs created or edited by `GITHUB_TOKEN` do **not** themselves
trigger other workflows, so the "Next Release" and back-merge PRs won't kick off
CI automatically — a maintainer re-runs or pushes if needed.

To make those PRs trigger CI, install a GitHub App for the repo and replace
`GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}` with an
`actions/create-github-app-token@v3` step (secrets `APP_CLIENT_ID` /
`APP_PRIVATE_KEY`), exactly as `rtk-ai/icm` does. Never commit the App private
key — it lives only in repository secrets.
