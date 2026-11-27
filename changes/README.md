# Changelog fragments

Each user-visible change adds one fragment file here, named `<topic>.md` (for example `line-merge.md`), written as one or two bullet lines (`- area: what changed`) describing the change from a user's point of view. Do not cite internal work-item numbers. Keeping one file per change means parallel pull requests never conflict on the changelog.

At release time, run `scripts/release/changelog.sh <version>` in the release pull request. It folds the fragments into `CHANGELOG.md` and deletes the consumed files. This README is kept.
