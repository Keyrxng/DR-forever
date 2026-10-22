# Provenance and contribution rules

## What this project is

`drf-server` is an independent Rust implementation of the server-side behavior
needed to interoperate with the Dungeon Runners client. It is intended for
preservation, research, and use with a client and game data that the operator
already owns.

The server is not a copy of the original server or decompiled client source.
Some content and protocol algorithms were adapted from the open-source
[`dr_pyserver`](https://github.com/Styx3/dr_pyserver) project, which is licensed
under GPLv3; those portions remain under GPLv3 and are attributed in
[`NOTICE.md`](NOTICE.md).

## Sources used

The implementation is based on:

- the observable protocol behavior of a user-supplied Dungeon Runners client;
- the client's shipped configuration and package format;
- functional protocol facts such as message framing, field widths, opcodes,
  and content identifiers;
- original test fixtures and expected outputs maintained by this project;
- the GPLv3-licensed `dr_pyserver` project, for specific maze, spawn, and
  creature-manipulator behavior.

The project does not require network captures to build or run.

In code comments, "the reference" means either observable client behavior or,
where explicitly named, the GPLv3 `dr_pyserver` implementation. It does not
refer to a closed-source server.

## What must not be contributed

Do not submit any of the following:

- game clients, installers, executables, DLLs, or libraries;
- `game.pkg`, `game.pki`, extracted game data, or asset bundles;
- artwork, audio, models, textures, fonts, or other creative content;
- packet captures or decoded traffic from a live or third-party server;
- decompiled or disassembled proprietary code;
- code from another server project whose license is incompatible with GPLv3;
  adapted code must be GPLv3-compatible, attributed, and clearly marked;
- credentials, keys, tokens, or personal data.

If a change depends on game data to test, keep the data outside the repository
and use the `DRF_GAME_PKI` and `DRF_GAME_PKG` environment variables.

## Licensing of contributions

By submitting a contribution, you agree that it is your original work, or that
you have the right to submit it under this project's GNU General Public License
v3.0 or later, and that it does not contain third-party material incompatible
with that license. Do not submit code you cannot license under these terms.

The project does not require a contributor license agreement. Contributions are
accepted under the same GPLv3-or-later terms as the rest of the repository.

## Names and marks

References to Dungeon Runners classes, zones, items, and other game terms are
used for interoperability and identification only. They do not imply any
affiliation with or endorsement by the rights holders. See
[`NOTICE.md`](NOTICE.md).
