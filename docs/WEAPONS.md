# Weapon configuration products

Capture belongs to `asset_game::WeaponCatalog`; `WeaponBuild` owns preparation
and dependency linking. `WeaponBuild::publish` freezes the registry and compiles
combat, equipment, penetration, FPV, HUD, world and event projections from each effective row.
Every publication receives a fresh revision; an exact registry clone retains its revision. IDs and selections belong to that registry revision. Replacing content requires
rebuilding its registry and all tables retaining those IDs.

`WeaponFamilies` exposes offered families, descriptions and attachment options.
Selection resolution normalizes attachments and checks offers, family
compatibility and host loadout limits. The internal configuration compiler
selects an authored row or an IW5 prepared row and applies registry admission.
Success returns that ID and canonical selection; missing content, missing
bindings, unsupported mechanisms and match restrictions remain distinct
refusals. Options test each toggle through the same resolver.

Family preparation applies overrides once. Simulation, FPV, world models,
HUD and events use the same effective registry row and existing prepared
assemblies. Catalog capture rows, the broad capture body/accessor and native IW5 override helpers are private to asset_game.

Combat projection publication resolves the melee weapon, charge animation and
melee classification. `combat_facts_of` binds the match location-damage table
and explicit `WeaponHostRules`, validates the result and returns simulation
facts or a typed refusal. The default host burst cooldown is 200 ms for every
source family. Unknown IDs, unpublished rows and unsupported configurations
refuse; session logs the reason and disables that combat row.

Equipment and penetration projections use the shared `weapon_iw4` products.
Source-specific rotation and ballistic-blade identification are compiled before
session publishes its tables. FPV and camera consumers use `WeaponFpvFacts`,
which contains placement/camera data and prepared alternate/dual-animation
classification rather than the capture body. Existing FPV assemblies retain
model and skeleton validation; motion-tracker presentation shares one query.

`WeaponHudFacts` publishes ammo/reticle/overlay data and primary, alternate and guided-overlay rules. `WeaponWorldFacts` publishes shield classification. `WeaponEventFacts` publishes impact, explosion and ignition data, projectile camera policy, ground-rest policy and fire-ping suppression. Killcam camera classification preserves the existing guidance-before-class-before-type priority. Consumers no longer read numeric capture classifications to decide these behaviors. Animation overlay convention and perk inheritance are also published with FPV facts.

Console completion names are compiled once with the catalog's family descriptions, admission and naming rules; console reads the resulting list. Registry lookup owns name normalization, including authored suffix handling. Presentation projections are metadata for an existing published row: they do not replace configuration admission, allow an unsupported selection or imply residency. An unknown ID or unpublished projection has no product.

Loadout catalog preparation compiles `CacItemPresentation` labels and archive-icon hints once per catalog. Source suffix/prefix rules, equipment table references, localization namespaces and image namespaces belong to asset_game preparation; display reads prepared labels and previews. Archive hints serve the explicit host bootstrap roster and do not establish source asset residency. Unknown keys without a prepared presentation remain visible as their identity. Replacing the retained registry requires rebuilding the loadout catalog; authored table replacement requires reapplying preview preparation.

Dropped-item DObj preparation follows item occupancy demand. It admits each encountered configuration, caches successful compositions and refusals within the weapon revision/world-catalog identity, and retains its registry owner. Revision or world replacement invalidates the cache; match installation/teardown clears it. No dropped-item composition is prepared by enumerating unused registry rows.

Remote-body kits follow current player/corpse demand after entity synchronization. Admitted configurations (or the explicit unarmed ID zero) share cached DObjs by body/world model identities and attachment topology; refusals are cached. The cache retains body/registry owners and clears on body, weapon revision, world identity or match replacement. Shield attachments retain their existing per-entity DObj path.

These CPU contracts do not imply GPU or media readiness. Material admission,
FPV layout, audio preparation and match generation readiness keep their own
validation and lifetime boundaries.
