# Developer reference

The rustdoc of every crate in the workspace, private items included, is
published beside these pages:

**[Open the developer reference](reference/index.html)**

It is built from `main` by the same workflow as this site. To build it
yourself: `tools/build-docs.sh`, or `cargo doc --workspace --no-deps
--document-private-items` for the rustdoc alone.

`xb_core` is the way in: it is the one API a front end uses. Each crate's
`README.md` in the repository says what the crate owns and its invariants.
