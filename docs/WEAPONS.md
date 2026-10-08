# Weapon configuration products

`asset_game::WeaponCatalog` captures source rows. `WeaponBuild` links dependencies
and publishes immutable effective rows, semantic policies and consumer projections.
`weapon_catalog/{capture_merge,catalog_linking,model_linking,publication}` own
those operations; `configuration/{iw4,iw5,t5,t6}` own source selection rules.

`WeaponFamilies` normalizes selections and checks attachments and host limits.
Resolution and UI toggles return a private registry-issued `WeaponHandle` plus
read-only canonical selection. Console/UI use this resolver. Publication iteration
includes the last valid row. Unarmed, unknown and unsupported remain distinct.

`WeaponRegistry::bind` rejects foreign revisions before row access. Exact clones
retain identity; republishing changes it. Raw projection getters are crate-private;
consumers use `BoundWeapon`. `PreparedWeapons` admits wire IDs only after checking
the installed snapshot epoch or original event generation. Local revisions never
enter wire IDs/content digests. Queued effects retain their producing generation.

Combat, equipment, penetration, FPV, HUD, world and event projections use one
effective row. Combat binds host rules and location damage before simulation.
The host burst cooldown remains 200 ms across sources. Presentation facts carry
camera, alternate, dual, shield, overlay and event policies without reinterpreting
capture classifications. Loadout labels/archive hints are prepared by asset_game.

`BoundWeapon::preparation` retains source keys and per-component targets/refusals.
T6 converted models/clips/cues and named donors retain separate source references
and storage locators. Linking requires declared references and actual catalogs;
missing required donors refuse dependent capabilities. Native material preparation
records explicit donor fields where needed. Compatibility does not relabel source identity.

`FpvWeaponTable::bind` checks registry, mesh, clip, material, image and atlas owners
before accepting handles and alternate links. Retained compositions validate model
and hide identity; track mappings/rigs retain actual clips and meshes. Foreign clips
or rig poses refuse before writes. Optional bones/clips and preparation budget remain.

Dropped items and remote kits share demanded `ItemComposition` topology and hide
layout, retaining registry/world owners. Required model/pose failures are cached;
optional attachments and camo-to-base fallback preserve policy. Replacement clears
successes/refusals. Kits retain body/world catalogs; `models()` accepts no catalogs.
Prepared DObj identity controls reuse. Optional heads and separate shield policy remain.

`PlayerAnimationBinding` retains the character kit/rig, paired tree/script and clips.
Multiplayer body-track compatibility validates leaves/tracks before advancement;
native T6 player profiles refuse. Persistent trees reuse only the same binding.
Body/tree/script/clip replacement resets animation and preserves corpse occupation;
unchanged bindings preserve blend, phase and rate.

CPU publication/binding does not imply GPU/media readiness. Existing match, material,
FPV and audio milestones keep their own admission and lifetime boundaries.
