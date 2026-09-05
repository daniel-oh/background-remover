# Security

## Reporting

Please report vulnerabilities privately through GitHub's
[security advisories](https://github.com/daniel-oh/background-remover/security/advisories/new)
rather than a public issue. You will get an acknowledgement within a few
days and a fix or a plan as quickly as the problem warrants.

## What is in scope

- Anything that lets a request read or write outside its own bytes, crash
  the process, or hold resources past the documented limits (12 MiB body,
  one inference at a time, a queue of four, 75 s per request).
- Decoder issues in the image path (JPEG via libjpeg-turbo, PNG and WebP via
  the `image` crate).
- The model checksum being bypassable.

## Design notes

The service is meant to run on a private network behind an application that
authenticates and rate-limits its users. It runs as an unprivileged user in
a distroless image with a read-only root and no capabilities, stores
nothing, and logs only method, status, byte counts and timing.

Dependencies are watched by Dependabot and `cargo audit` runs in CI.
