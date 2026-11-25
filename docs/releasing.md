# Releasing

Every push to `main` opens or updates a release PR (`chore: release vX.Y.Z`). Squash-merge it to release: CI tags the version, drafts a GitHub release, uploads the archives and makes it public.

`RELEASE_PLZ_TOKEN` is a fine-grained PAT (Contents and Pull requests: read and write). It is shared with `lulivi/mw`: after regenerating it, update the secret in both repos.

## If a release fails

Re-run the failed jobs: `gh run rerun <run id> --failed`.

The exception is a `release` job that failed after creating the tag, because a re-run skips a version that is already tagged. First delete the tag, and the draft release if there is one:

```sh
gh release delete vX.Y.Z --yes      # only if the draft exists
git push origin :refs/tags/vX.Y.Z
```

Never do this once the release is public.
