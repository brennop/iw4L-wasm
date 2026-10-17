# Presentation publications

`render_fx::PresentedFireFx` owns receipts for one world and replay timeline.
A receipt identifies a fire cause and product, with pellet/segment identity for
tracer, impact, glass and marks. Positions, normals, surface and target are
result data. Authority replaces that data; conflicting authority is refused.

| product | prediction | authority |
|---|---|---|
| muzzle/brass | create when available | retain success or retry refusal |
| tracer/transient impact | create when available | retain success or retry using authoritative result |
| marks | defer | schedule mark emission once |
| glass | defer | evaluate authoritative segment once, including zero hits |

`execute` admits scope/verdict/budget before invoking creation. It records
success after creation returns. Dependency and allocation refusals remain
retryable; intentional tracer interval skips are terminal. Marks report
Scheduled, separately from Created: a scheduled effect can later encounter a
receiver/material refusal in the mark subsystem.

Impact effects carry Transient or Marks emission policy through runners,
emitted children, impacts, deaths and trails. Transient effects cannot emit
decals; Marks effects simulate child motion without drawing particles,
models/lights/trails or playing sounds.
Expiration runs once per presentation frame, including idle frames. Receipts and
tracer shot decisions retain a 5000 ms horizon; the receipt bound is 8192
products. Saturation refuses admission rather than evicting recent successes.
World/timeline replacement or clock rewind resets the scope; i32 wrap remains
legal. Late delivery after expiry may create a fresh product. `fx_dump` reports
scope/time, product budget refusals, receipt size/capacity and sweep costs.

`asset_material::UiImagePublication` owns material mappings, preview policy,
zone payloads and archive indices. Preparation returns private products;
shell/match installation selects the active owner. A late shell completion
cannot replace an installed match. HUD handle/RGBA caches and asynchronous
class/loading previews adopt publication identity. Menu overlays replace their
own maps and caches when the menu resource changes.

Installation revisions cover relevant file metadata, following game symlinks.
Unix revisions also include inode/change time. A new load checks the revision;
there is no automatic file watcher. Archive indices retain opened readers, so
atomic file replacement preserves old readers for referenced publications.
In-place corruption is refused through archive length/CRC validation.

`EditorWeaponCatalog` exposes map-independent selection and class admission;
it cannot be installed as `PreparedWeapons`. Match admission additionally
checks prepared FPV assemblies and map-selected hands. Appearance selection
keeps NativeReady, BaseByPolicy and DeclaredUnavailable separately for view and
world models. Render consumers retain refusal reasons and do not replace a
declared unavailable model with the base weapon.
