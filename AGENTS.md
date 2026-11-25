# Working on bt

- Every feature has a GitHub issue `#<N>`. Work happens on branch `feature/gh-<N>`.
- Commits follow [Conventional Commits](https://www.conventionalcommits.org/) and end with a `Refs: #<N>` trailer.
- The PR body contains `Closes #<N>`.
- `make check` (fmt, clippy, tests, shellcheck, actionlint, SPDX headers) must pass before committing.
- New files start with the same two SPDX header lines as `src/main.rs`, in their own comment syntax. Docs, `LICENSE`, `.gitignore`, the lockfile and images have none.
- Domain vocabulary lives in `GLOSSARY.md`; design decisions in `docs/adr/`.
