# Changelog fragments

One file per work item or PR, named `<item>.md` (for example `G1.1.md`), with
one or two lines describing the user-visible change. Run
`scripts/release/changelog.sh <version>` in the release PR to fold the
fragments into `CHANGELOG.md` and delete the consumed files. This README is
retained, so parallel feature PRs never conflict on the changelog.
