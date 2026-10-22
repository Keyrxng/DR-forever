# NOTICE

Copyright (C) 2026 keyrxng.

## Unofficial interoperability project

`drf-server` is an independent, unofficial server implementation for the
discontinued game **Dungeon Runners**. It is not affiliated with, endorsed by,
sponsored by, or supported by NCsoft, NetDevil, or any successor or rights
holder.

"Dungeon Runners", associated logos, character names, item names, zone names,
and related marks are the property of their respective owners. Their use here
is solely for identification and interoperability.

## No game assets or proprietary source code

This repository does **not** contain or distribute:

- the Dungeon Runners client, executable, libraries, or installer;
- `game.pkg`, `game.pki`, or any other game data;
- artwork, audio, models, textures, fonts, or other game content;
- network captures from a live or third-party server;
- decompiled, disassembled, or otherwise reconstructed proprietary source code;
- code from a server project with a license incompatible with GPLv3.

Protocol field layouts, opcodes, and constants in this repository are
functional interoperability facts re-derived from the behavior of a
user-supplied client. They are not intended to reproduce or replace the
original game's creative content.

## Third-party attribution

Portions of the maze generation, dungeon spawn placement, and creature
manipulator logic are adapted from [`dr_pyserver`](https://github.com/Styx3/dr_pyserver),
Copyright (C) Styx3 and contributors, licensed under GPLv3. Those portions are
distributed here under GPLv3 as part of the combined work. `dr_pyserver` is an
independent Python reimplementation and is not the original Dungeon Runners
server.

You are responsible for obtaining the client and game data lawfully and for
complying with the terms that apply to your copy. Do not commit client files,
game data, captures, or extracted assets to this repository.

## License scope

The GNU General Public License v3.0 or later in [`LICENSE`](LICENSE) applies to
the source code and documentation in this repository. It does not grant any
license or right to:

- Dungeon Runners or any associated intellectual property;
- game clients, game data, or extracted assets;
- third-party libraries, tools, or content;
- trademarks, service marks, or trade dress.

See [`PROVENANCE.md`](PROVENANCE.md) for the project's source and contribution
rules.
