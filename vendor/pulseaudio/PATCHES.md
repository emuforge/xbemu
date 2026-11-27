# Changes to the vendored files

`pulseaudio` 0.3.2 from crates.io (MIT, `LICENSE.md`;
<https://github.com/colinmarc/pulseaudio-rs>), the client `cpal`'s
PulseAudio host is built on. `src/`, `examples/` and the manifest as
published; `testfiles/` and `Cargo.lock` are left out (nothing built here
uses them). The workspace's `[patch.crates-io]` puts it in the published
crate's place.

1. **`src/client/reactor.rs`, `Reactor::recv`: a read of the socket lands
   in a buffer the reactor keeps.** The published crate grows its receive
   buffer by 1 MiB of zeroes before every `read` and cuts it back to what
   arrived. The thread wakes for a few bytes of the server's timing
   messages many times a second, so here the read lands in a 64 KiB buffer
   the reactor keeps (`read_chunk`) instead of a newly cleared one, and
   what came is appended; the loop it is in reads again until the socket
   has no more, as in the published crate. Marked `xbemu:` in the source.

Drop this directory and the patch entry when a published release reads
that way.
