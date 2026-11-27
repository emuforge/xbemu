# xb-testkit

Test support shared by the workspace: it finds the game images a developer
has linked into the titles stash (`titles/README.md`).

- **Layer:** L0, no dependencies. Other crates use it as a dev-dependency
  only.
- **Invariant:** a title that is not in the stash is never an error. The
  lookup prints a note naming the missing link and the test passes.
- **`unsafe`:** forbidden.
- **Tests:** `cargo test -p xb-testkit`
