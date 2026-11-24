# Working on bt

- Every feature has a GitHub issue `#<N>`. Work happens on branch `feature/gh-<N>`.
- Commits follow [Conventional Commits](https://www.conventionalcommits.org/) and end with a `Refs: #<N>` trailer.
- The PR body contains `Closes #<N>`.
- `make check` (fmt, clippy, tests, shellcheck, actionlint) must pass before committing.
- Domain vocabulary lives in `GLOSSARY.md`; design decisions in `docs/adr/`.
