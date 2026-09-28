# Lightning reference peers

Host-only reference tools for testing a Lightning implementation against
independent peers. This repository contains the Rust LDK harness, pinned Cargo
lockfiles, and the builder for stock Core Lightning and LDK. It is not an app
dependency.

Extracted from [Winnow Lightning](https://github.com/posix4e/winnow-lightning)
commit `fc7a3dc34c1f498a894df12dcaf62020896e8c4a`, specifically
`Tools/LightningReference` and `scripts/ci-lightning-references`. The Rust source
and lockfiles are unchanged by the extraction. The original MIT license is
preserved in [LICENSE](LICENSE).

## References

- Core Lightning v26.06.8: `6f741afc395c66d200429ea477d29df4d974748d`.
- LDK: `0a2b003e7e1602df8933b15b7563ba8b6e198391`.
- Rust toolchain: 1.89.0.

The builder checks out the exact upstream revisions without protocol patches.
It runs LDK's async-payment tests with both standard time and its controlled
test clock, then builds the harness using the checked-in lockfiles and
`cargo --locked`. The harness uses public LDK APIs for codec/signature checks,
onion messages, and disposable regtest nodes with persistent channel state.

## Build on macOS

```sh
brew install autoconf automake libtool gnu-sed gettext libsodium lowdown pkgconf openssl@3 make sqlite uv
rustup toolchain install 1.89.0 --profile minimal
RUSTUP_TOOLCHAIN=1.89.0 scripts/build-references /tmp/lightning-references
```

Use a dedicated build directory. Outputs retain the layout consumed by Winnow:

- `cln/`: stock Core Lightning checkout and binaries.
- `ldk/`: pinned LDK checkout.
- `harness/target/debug/winnow-lightning-reference`: Rust host executable.
- `reference-manifest.json`: upstream revisions, this repository's commit, and
  compiler version.
- `ldk-async-*.log`: upstream test results.

The harness accepts newline-delimited JSON commands on stdin. For example:

```sh
printf '%s\n' '{"command":"vectors"}' | /tmp/lightning-references/harness/target/debug/winnow-lightning-reference
```

Its deterministic fixture keys are public test data and only suitable for
disposable regtest networks.

## Winnow integration

Winnow pins an exact commit of this repository in `scripts/ci-lightning-references`
and fetches it into the reference build directory. Its Swift fixtures, Python
scenario drivers, Bitcoin Core validation, app tests, and simulator recordings
remain in Winnow. No Rust is linked into its application.

Reference self-tests and codec checks do not establish payment settlement or
general standards compliance. Winnow's separate interoperability scenarios
exercise the Swift engine against these peers over real connections, including
async hold/release, offline clients, process restarts, and timeout recovery.

The host waits for Init and usability of every existing channel to a returning
peer before replaying its saved onion-message batch. A peer without channels
can receive the batch after Init. The batch remains on disk; one queued attempt
is assigned per connection event, and repeated event-loop checks do not resend
it. An unrelated unusable channel can delay replay because an opaque saved
notification does not identify its payment's target channel. This conservative
fixture policy is not a general-purpose liquidity-provider scheduler. The
builder also runs the host's unfunded readiness and queue tests.
