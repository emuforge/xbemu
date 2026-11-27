# Security

xbemu reads untrusted input: disc images, executables and savestates. A
crafted file that makes the emulator run code on the host, read or write
outside its data directory, or otherwise escape the guest is a security bug.

Report one privately through GitHub's "Report a vulnerability" on the
repository's Security tab
(<https://github.com/emuforge/xbemu/security/advisories/new>), not in a
public issue. Say which build (the pre-release's commit) and include the
smallest input that shows it, unless that input is commercial game data, in
which case describe it.

Only the latest pre-release from `main` is supported while the project is
pre-1.0. A crash or a hang on a damaged file with no memory-safety
consequence is an ordinary bug; open an issue.
