# Security policy

## Supported versions

This project is alpha software. Security fixes are applied to the current
`main` branch; there are no maintained release branches yet.

## Reporting a vulnerability

Do not open a public issue for a security problem.

Once the public repository is available, use its **private vulnerability
reporting** or **security advisory** feature. If that is not available, contact
the maintainers privately through the repository's listed security contact.

Please include:

- the affected version or commit;
- a description of the issue and its impact;
- reproduction steps or a minimal proof of concept;
- any suggested mitigation.

Do not include game assets, client binaries, credentials, or other users'
data in a report.

## Deployment scope

`drf-server` speaks the original client's protocol and is intended for
controlled, trusted environments. It is not hardened for arbitrary public
exposure. In particular:

- `--admin` exposes administrative commands; never enable it on an untrusted
  network.
- `--trust-client-sim` trusts client-reported simulation data; use it only with
  a client and hook you control.
- The server does not provide TLS. Run it behind a trusted network boundary or
  an appropriate tunnel.

## Secrets and logs

The server stores character data in a local JSON file. Do not commit the data
directory, logs, packet captures, or client configuration. See
[`PROVENANCE.md`](PROVENANCE.md).
