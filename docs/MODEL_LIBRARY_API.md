# Model library v1 contract (M0)

Frozen on 2026-10-18, with explicit user amendments recorded below. The client plan `docs/MODEL_LIBRARY_IMPLEMENTATION_PLAN.md`
sections 1, 3–7 is normative; this document resolves its implementation choices.
This extends Sculptor, never the separate `spm-cloud` service. No protocol v1
field numbers, release hashes, application versions, or wardrobe UI change.

M3 implementation update: source equipment, observer policy enforcement and
legacy wardrobe hash restrictions are implemented. The local schema is v3 and
`capabilities.enabled` is true after the automated distribution checks below.
Client library service and UI remain M4–M6 work; real-game acceptance is
MANUAL/PENDING. Frozen Protobuf v1 field numbers remain unchanged.

## Identity and request envelope

Every business route, including capabilities, requires a verified player.

M4 client integration adds `equipment_revision` to the existing `GET session`
JSON response, alongside uuid/name/admin. It is the authenticated player's
current equipment revision, read on the bounded worker pool, so both Figura
and Protobuf clients can submit library equip CAS without guessing from a
legacy profile. No target UUID parameter, new route, migration or frozen
Protobuf field is introduced. The API test verifies 0 before equipment, 1
after equipment, another player's independent 0 and conflict on the stale
revision. M4 validation passed: cargo build, the full 76-test suite and the
separate worker-saturation test. Actual game/deployment acceptance remains
MANUAL/PENDING.

M5 catalog integration completes the planned `sort=name` whitelist option:
the server orders display names with SQLite NOCASE, then model_id, before
applying pagination. The API test reads three one-item pages, checks stable
case-insensitive ties and total/has_next, and rejects arbitrary sort strings.
Validation: cargo build, 77 regular tests and the separately run worker
saturation test passed. No storage schema or permission semantics changed.

Protobuf requests use `Authorization: Bearer <session>` and
`x-omnimorph-epoch: <u64>`, validated by `api::protobuf::file_scope`.
Figura requests use `x-omnimorph-auth-mode: figura` and
`Authorization: Bearer <existing-figura-token>`; resolve through the existing
token registry, never a submitted UUID. Protobuf mode is the default; an epoch
on a Figura request or an unknown mode is rejected. No credentials in URLs.
Administrator requests additionally use `x-omnimorph-admin: <access-token>`;
this cannot replace the player session. Client endpoints remain same-origin,
HTTPS/trust constrained, and reject redirects carrying credentials.

All mutations carry a UUID `operation_id` (DELETE uses a JSON body).
Per actor and operation, persist the canonical request fingerprint and result:
same request returns the prior result, changed request returns 409.
`expected_revision` is the model's policy revision, not a resource revision.
Image/content replacement also advances model revision. The server decides
uploader, IDs, timestamps, hashes, moderation, counters, and allowed actions;
unknown identity/moderation fields in ordinary DTOs are rejected.

## Policy applied to every entry point

An administrator below means a configured, UUID-bound player with an active
password-authenticated admin session, explicitly accessing admin context.
Ordinary scopes never expose blocked records, even to their uploader.

| State | Ordinary metadata/list/image | Complete preview/download/equip |
| --- | --- | --- |
| ACTIVE PUBLIC, no authorization | All authenticated players | All authenticated players |
| ACTIVE PUBLIC, authorization | All authenticated players | Uploader, granted UUID, administrator |
| ACTIVE PRIVATE, either authorization flag | Uploader only | Uploader only; administrator in admin context |
| BLOCKED (forced PRIVATE) | Nobody in ordinary scopes | Administrator in admin context only |
| DELETED | Administrator audit/management context only | Nobody; no new transfer/equip |

Private always overrides grants. Admin management may inspect active private
metadata and packages. A later explicit user amendment allows a player to opt
into showing their own current private equipment to observers, default false.
This is a source-equipment render permission only: library list, details, covers,
download and grants remain private. Closing that option or changing equipment
revokes it; blocked/deleted identities still prohibit observer distribution.
M3 implements this option and its two-protocol invalidation. PUBLIC
authorized distribution evaluates the observer's grant, not just the wearer.
Metadata visibility and full-use permission are separate decisions. A visible
but locked model can be favorited, and its permitted public cover can be read.

Uploader/admin can patch, delete, edit images, and manage grants on visible
records; only admin can block/unblock or maintain blocked records. Grants and
favorites survive visibility changes. Removing one's own invisible favorite
succeeds idempotently without returning model metadata. Ordinary deleted or
invisible lookups return 404, visible but forbidden actions return 403.

Block atomically forces PRIVATE, retains authorization intent, adds verified
hash bans, advances policy revision, and audits the change. Unblock restores
ACTIVE but retains PRIVATE. Delete is logical and does not release hash bans.
Even after deletion, an explicit admin unblock can release that model's bans;
another active ban on the same identity still applies. Old transfers, aliases,
subscriptions, caches and resource URLs must re-evaluate the same policy.

Wardrobe upload/local use remains available. A verified banned identity gets
SELF_ONLY distribution for its actual owner, across accounts/assets/protocols.
Observers receive unavailable/removal projections; self receives no forced
local unequip. Library blocked files cannot be newly downloaded by uploaders.
Pending resources are inaccessible to legacy commit/equip/download/delete RPCs.

## Routes and JSON

All paths below have prefix `/api/library/v1`. Equip remains M3; the other
routes are implemented in M2.
`owner` means authenticated uploader; `use` means the complete-use policy above.

| Method/path | Request / result | Policy |
| --- | --- | --- |
| GET capabilities | Limits and supported formats/categories | Player |
| GET session | Verified UUID/name, current admin validity | Player |
| GET models | scope, q, category, tags, type, authorization, status, sort, page, page_size; paged summaries | Visibility; admin scope requires admin |
| GET models/{id} | ModelDetail | Visibility |
| PATCH models/{id} | operation_id, expected_revision, optional display_name/description/category_id/tags/visibility/requires_authorization | Owner/admin |
| DELETE models/{id} | operation_id, expected_revision | Owner/admin, logical delete |
| PUT/DELETE models/{id}/favorite | operation_id | Visible for PUT; own reference for DELETE |
| POST uploads | operation_id, metadata, distribution descriptor | Player, upload budget |
| POST uploads/{operation}/commit | operation_id, model_id, expected_revision | Pending owner, validated files |
| DELETE uploads/{operation} | operation_id | Pending owner; committed returns conflict |
| PUT/DELETE models/{id}/image | Image bytes with operation/revision headers, or JSON deletion | Owner/admin |
| GET models/{id}/image | Validated custom image, 404 NO_IMAGE if absent | Visibility |
| POST models/{id}/transfers | operation_id, purpose=preview/download, model_revision | Use |
| GET transfers/{id} | Package bytes, digest/length headers | Same actor, purpose, revision, unexpired and current policy |
| POST transfers/{id}/complete | operation_id, raw_sha256 | Same actor, completed byte stream, download receipt |
| POST models/{id}/equip | operation_id, model_revision, expected_equipment_revision, show_private_equipment=false | Use, creates source-bound self alias; optional private observer rendering |
| GET models/{id}/grants | page/page_size; UUID/name/time entries | Owner/admin |
| PUT/DELETE models/{id}/grants/{uuid} | operation_id, expected_revision | Owner/admin |
| GET players/resolve | name, model_id; verified directory candidates | Owner/admin of model, rate limited |
| POST admin/login | operation_id, player_name, password, remember | Configured verified UUID + password |
| POST admin/refresh | operation_id, refresh_token | Same player/origin/instance/config; single-use rotation |
| POST admin/logout | operation_id | Own admin family, idempotent |
| PUT/DELETE admin/models/{id}/block | operation_id, expected_revision, reason | Admin; atomic policy/ban update |

Image PUT headers: `x-omnimorph-operation` (UUID),
`x-omnimorph-expected-revision` (decimal policy revision). Other mutations use
JSON. Upload reserve returns an owner-bound relative upload path and deadline.
Reuse the existing streaming upload implementation; the library adapter can
provide a thin Figura-authenticated route when a scoped Protobuf grant is not
available. Reserve installs pending source bindings before issuing any grant.
Commit validates immutable blob first, then publishes resource metadata, model,
bindings and operation result in one SQLite transaction. Orphan blobs are
delayed garbage, never downloadable pending resources.

Example summary (synthetic identities, no credentials):

```json
{"model_id":"00000000-0000-0000-0000-000000000010","revision":1,"policy_revision":1,"display_name":"示例","internal_name":"内部名称","authors":["示例作者","Second"],"uploader":{"uuid":"00000000-0000-0000-0000-000000000011","name":"Example"},"model_type":"FIGURA","image_revision":0,"image_available":false,"downloads":0,"favorites":0,"favorited":false,"visibility":"PUBLIC","requires_authorization":true,"allowed_actions":["favorite"]}
```

ModelDetail adds description, category_id, tags, byte_length, updated_at,
resource_revision, metadata_source and metadata_validation. No package body,
private URLs, passwords or admin tokens in list/detail DTOs. Counts, lengths
and revisions use nonnegative signed-64-compatible integers. List result:
`{"items":[],"total":0,"page":1,"page_size":8,"has_next":false}`.
Download receipts count only download purpose after successful stream completion
and client digest confirmation; deduplicate by actor/model/revision per UTC
24-hour window. Preview and retries do not increase the count.

## Text and transport budgets

Limits refer to UTF-8 bytes after trimming and normalization; reject over-limit
input, do not silently truncate provenance. All text is literal, including
tooltips; no click events or interpreted Minecraft formatting from metadata.
Retain nonempty authors in order with exact normalized duplicates removed;
prefer authors array, then legacy author. Empty authors remain `[]`, localized
as unknown; do not store that localized fallback as an author.

| Field | Limit |
| --- | --- |
| display_name / internal_name | 96 / 96 bytes |
| authors | 4 entries, 48 bytes each |
| uploader name snapshot | 64 bytes |
| category_id | 32 ASCII bytes, configured allowlist |
| tags | 8 entries of 32 bytes |
| description | 4096 bytes (detail only) |
| q / moderation reason | 128 / 512 bytes |
| summary serialized JSON | 6144 bytes including worst-case escaped text |
| control request/response | 65536 bytes |
| page_size | fixed maximum 8, default 8; 1–8 accepted, >8 rejected |

8 worst-case summaries consume 49152 bytes, leaving over 16 KiB for envelope.
Summary byte bound is validated when writing, not by dropping items from a
page. Offsets always use the accepted page_size. This is the plan's explicit
64 KiB-driven alternative to the illustrative 48-item performance sample;
M7 still tests 10,000 rows at the actual supported maximum. Sort fields are
updated/newest/downloads/favorites with stable model_id tie-breaker. Public,
mine, favorites queries and their totals share identical visibility predicates.
Admin alone may filter moderation status. SQL uses parameters and sort allowlist.

Initial image contract: PNG/JPEG only, at most 2 MiB, dimensions 1–2048 each,
at most 4,194,304 pixels, validated decoding with bounded memory. No external
URLs/SVG/HTML. Missing/broken cover uses the packaged copy of root
`ingame_logo.png`; decode failure offers retry. No avatar icon or screenshot.
Server capabilities returns these limits and upload bytes from configuration;
local 64 MiB source / 32 MiB resource / 256 MiB expanded / 4096 entries remain
independent frozen parsing budgets. No guessed upload limit when offline.

Error example: `{"code":"CONFLICT","localization_key":"library.error.conflict","request_id":"opaque-id","retryable":false}`.
Stable codes/statuses: UNAUTHENTICATED/401, FORBIDDEN/403, NOT_FOUND/404,
NO_IMAGE/404, CONFLICT/409, TOO_LARGE/413, UNSUPPORTED_FORMAT/415,
RATE_LIMITED/429, BUSY/503, INVALID_REQUEST/400, INTERNAL/500.
Only rate/budget failures include safe limit/retry_after_seconds fields.
No token, password, local paths or hidden model metadata in errors/logs.

## Metadata and identity provenance

Input picker accepts ZIP/YSM only. Inspect contents: Figura avatar.json,
new YSM ysm.json metadata, legacy YSM
main.json/minecraft:geometry[0]/description/ysm_extra_info. Ambiguous packages
are rejected, not classified by extension. Encrypted YSM uses bounded full
YsmModelData parsing; lightweight list metadata cannot establish authors.
Client compiles the existing compressed NBT/distribution payload, never sends
source ZIP to an NBT-only endpoint. Display name changes do not overwrite source
name/authors. Figura server-readable metadata is SERVER_VERIFIED only after
the server compares the prepared package; encrypted YSM metadata is explicitly
CLIENT_EXTRACTED. Authors are provenance text, not proof of ownership.

Hash kinds: RAW_SHA256 (exact distribution bytes), NBT_SHA256 (server-validated
uncompressed NBT bytes, independent of gzip wrapping),
YSM_CIPHERTEXT_SHA256 (validated embedded original ciphertext), and
LEGACY_FIGURA_BASE64_SHA256 (legacy Base64 text, mapped to decoded bytes before
comparison). All are lower-case 64-hex, with kind mandatory. An unencrypted
YSM client-declared content hash is not server-verified identity and cannot
alone control bans. No Rust decryption or decrypted encrypted-YSM disk cache.
Different modified contents are outside same-identity matching guarantees.

## Storage and administrator security decisions

Use independent library tables in existing SQLite, never cascading foreign keys
to manifest-rewritten owners/avatars/equipment. New schema user_version=2;
back up using SQLite backup API before migration, transactional DDL and version
change, reject newer unknown versions. Manifest schema remains 1. Shared save
hook commits manifest and library metadata together when a business operation
requires both; ordinary save preserves all library tables. Restore the entire
pre-upgrade database/blob backup to run an older binary; no lossy downgrade.

Password format: `pbkdf2-sha256$v1$600000$<32-lower-hex-salt>$<64-lower-hex-key>`.
Accept 600000–2000000 iterations, exactly 16 random salt bytes and 32 key bytes.
Unknown versions, wrong sizes/encoding, duplicates (UUID or case-folded login
name), nil UUID, or weak parameters invalidate configuration. Use ring PBKDF2
constant-time verification in a bounded blocking worker; passwords 1–1024 UTF-8
bytes, no password in process arguments/logging. Offline tool uses masked stdin.
Test-only fixed salts/passwords are labeled synthetic.

Admin access expires in 15 minutes, remembered refresh in 7 days; random
256-bit credentials are stored hashed on the server, bound to verified player,
server instance/origin and configuration fingerprint. Refresh is single-use;
reuse revokes its family, logout revokes family. Configuration already reloads
in utils/auxiliary.rs: recheck the current admin entry fingerprint on every
admin access/refresh, so deletion, name/password changes revoke existing tokens
without relying on a UI refresh. Restart/instance changes invalidate old grants.
No GUI for editing configured accounts. Remembered client refresh credentials
may be saved only with owner-only file permissions; otherwise remember across
restart is disabled with a localized explanation. Never persist passwords.

## Existing-entry inventory and required enforcement seams

| Existing path / symbol | M2/M3 enforcement obligation |
| --- | --- |
| main.rs legacy_api: /api/:uuid, /api/:uuid/avatar, /api/equip, /api/avatar | Filter profile/reads/equip; hash wardrobe uploads/deletes |
| /ws, figura/websocket/{handler,processor}.rs | Per-observer subscriptions, pings, availability/removal |
| /api/v1/{raw,sub/raw}, http2ws.rs | Same event policy as WS; no recovery bypass |
| /api/v1/avatar/:uuid, avatars.rs | Admin-token legacy upload/delete goes through same identity policy |
| /internal/:uuid/{temp,avatar,event}, upload_state | write_legacy/temp ownership, dynamic read and event filtering |
| /api/protobuf/v1/rpc ProfileRequest/DownloadRequest | Filter descriptors, grants, compatibility, pending data |
| RPC UploadRequest/CommitUpload/Equip/Delete | Pending binding, source alias, ban and revision checks |
| /api/protobuf/v1/resources/uploads/:operation | Streaming owner/grant/pending binding checks |
| /api/protobuf/v1/resources/:owner/:asset/:revision | Dynamic policy even for guessed historical URLs |
| /ws/protobuf/v1, subscription.rs, SnapshotRequest | Per-observer events, resume and snapshots |
| service/resource.rs ensure_legacy_import/download_legacy/write_legacy | Historical/temp identity mapping and SELF_ONLY |
| service/equipment.rs, event.rs, ysm.rs | Revocation invalidation, YSM event/snapshot filtering |
| storage/sqlite.rs::save | Preserve library rows and transact related source bindings |

`/api/v1/user/*` remains existing account administration, not library admin
authority. Existing internal token/Host checks never become game credentials.
Legacy auth/assets/info routes carry no model library bodies. Auditing these
seams does not authorize unrelated authentication/UI refactors.

## Validation record

M0: static route/symbol mapping and JSON/fixture review only; no build required
by the implementation plan. See client fixture manifest for hashes and limits.
Minimum supported layout target is 320×180 logical GUI pixels (1280×720 at
scale 4): one-column grid, preview may use existing expanded preview action.
Smaller surfaces show a localized size hint, never negative widget geometry.
Real-window render checks at planned resolutions/scales and encrypted model
visual behavior are MANUAL/PENDING; synthetic parser vectors are not rendered
model evidence. These checks belong to M4–M7 and do not block M0.

## M1 deployment and verification

Before starting the new binary, stop the service and retain a complete backup
of metadata.sqlite3 (including committed WAL via SQLite backup), model blobs
and configuration. Schema 1 upgrades to 2 with a uniquely named
`metadata.pre-library-<random>.sqlite3` backup next to the database. The upgrade
transaction creates library tables and advances user_version together. A failed
DDL rolls back and leaves version 1 usable. Unknown versions above 2 fail startup
before upload cleanup. Old binaries reject version 2: rollback requires restoring
the complete old database/blob snapshot, not manually editing user_version.

Generate deployment passwords locally using `python tools/library_admin_hash.py`
in an interactive terminal. It reads and confirms masked input, uses a fresh
16-byte cryptographic salt and PBKDF2-HMAC-SHA256/600000, and prints only the
versioned hash for the deployer to install in private Config.toml. Do not paste
real credentials into tests, issues or command arguments. Python standard
library only; no new production dependencies. Interactive password entry and
real deployment credentials are MANUAL/PENDING, intentionally not generated by
automated validation.

The existing configuration watcher reconciles active sessions on a blocking
worker before publishing a valid reload. Removed/changed administrators are
durably revoked, so restoring an old config cannot reactivate their old tokens.
Access/refresh also compare current account fingerprints and player/origin/
instance bindings. M2 handlers must resolve the *current* configured account,
reject removed accounts and recheck it after password verification, before
issuing credentials. SQLite stores credential hashes only. Replay revocation
commits even though refresh returns an authentication error. No persistent
client remember-login file is introduced by M1.

M1 automated results (2026-10-18): `cargo test library` 12/12,
`cargo test storage` 12/12, and affected-startup/protocol baseline `cargo test`
37/37; all exit 0. Tests cover 48 policy combinations, observer distribution,
v1 backup/DDL rollback, manifest hook rollback/restart, concurrent unique keys,
operation conflicts, persistent bans across delete/unblock, independent PBKDF2
vector, invalid config, identity/expiry bindings, refresh replay, logout and
durable config-change revocation. Scoped rustfmt and `git diff --check` pass.
M1 has no GUI; deploying real administrator credentials is MANUAL/PENDING.

## M2 implementation and deployment checkpoint

The JSON extension is mounted at `/api/library/v1`. List/detail/edit/delete,
favorites, grants, player resolution, covers, upload reserve/body/commit/cancel,
preview/download transfers and administrator login/refresh/logout/block are
implemented. Equipment, observer invalidation and the complete legacy-route
policy audit remain M3 work. `capabilities.enabled` therefore stays false until
that closure is validated. Frozen Protobuf v1 messages are unchanged.

Every request requires the existing authenticated player. Protobuf mode uses
its existing bearer and epoch headers. Figura mode explicitly selects
`x-omnimorph-auth-mode: figura` and sends the existing player token as
`Authorization: Bearer ...`, without an epoch. Native `token` headers and
duplicate credential headers are rejected. The server supplies the uploader
UUID/name; client-supplied identity fields are rejected.

Administrator requests additionally bind `x-omnimorph-origin` to the request
Host and the existing server instance. Authenticated administrator operations
use `x-omnimorph-admin`. Reverse proxies must preserve the public Host and serve
the configured trusted HTTPS endpoint. A successful password login grants only
the configured UUID its short-lived administration session. Configuration reload
holds the publication lock through durable revocation; issuance rechecks the
current configuration under that same lock.

Mutations carry UUID `operation_id`; versioned changes also carry
`expected_revision`. Retrying the same operation and body returns its result;
changing the body conflicts. Login/refresh retries reconstruct credentials using
a process-local keyed derivation, while SQLite stores only credential hashes
and nonsensitive operation metadata. Restart invalidates the process binding:
start a fresh login operation. Reusing a consumed refresh token with a different
operation revokes its family. Password admission is bounded to five attempts per
60 seconds per player/instance, in addition to the two concurrent KDF limit.

`[model_library]` configures `max_upload_bytes` (default 64 MiB, allowed 1 byte
through 1 GiB) and category identifiers (default `general`). The library upload
limit is independent of the wardrobe limit; existing `canUpload` still applies.
Clients read capabilities before starting an upload. Upload bodies use
`application/octet-stream`; control requests/responses have a 64 KiB budget.
Source names accept ZIP/YSM only; actual distribution bytes and model type are
validated before atomic publication. Pending records stay out of public lists
and old resource routes. Interrupted bodies can retry the same grant; after a
server restart, repeat reserve to obtain a new in-memory grant for the same
draft. Cancel never publishes a partial model.

For unencrypted packages, the server checks parsed internal name/authors and
records verified raw/NBT identities. Encrypted YSM retains ciphertext and marks
metadata `CLIENT_EXTRACTED`; it does not claim that client metadata is verified
plaintext. An authorization-required model automatically grants its uploader;
that uploader grant cannot be revoked. Visibility and authorization remain
independent, editable fields. Client-side model `free` metadata supplies only
the upload form's initial authorization selection (M4).

Covers accept decoded PNG/JPEG only, at most 2 MiB, dimensions at most
2048×2048, with a 32 MiB decoder allocation budget. Missing custom covers return
404 `NO_IMAGE`; the client supplies the fixed placeholder. Images use immutable
blob storage, without persisting placeholder copies.

Transfers expire after 120 seconds and bind player, purpose, model revision and
policy revision. Each stream rechecks player and source access at chunk
boundaries. At most two streams per player and 32 globally are admitted. SQLite
work uses an eight-worker bounded pool and returns 503 when saturated. Formal
download counting requires the full server stream plus a matching client hash
acknowledgment; preview never counts. Counts deduplicate by player/model/revision
per UTC day. Receiving headers or requesting a file alone does not count.

Errors use the frozen JSON envelope, including localization key, request ID and
retryability. Status codes include 400/401/403/404/409/413/415/429/503. Responses
are `no-store`. Logs contain route templates, status, elapsed time and control
body byte count; credentials, private search strings and model bodies are not
logged. Player rate accounting charges each dispatch once, not every chunk.

M2 automated evidence (2026-10-18): `cargo test library` 34 passed; the isolated
`cargo test worker_saturation_returns_503_and_recovers_after_permits_are_released
-- --ignored --test-threads=1` passed. Full `cargo test` passed 59 tests with that
one intentional isolated test excluded; together all 60 registered tests ran.
`cargo build` also succeeded (exit 0); six unused-code warnings concern existing
foundation/test entry points and distribution hooks awaiting M3. Scoped rustfmt
and `git diff --check` passed. No production credentials or deployed data were
used for destructive test cases.
Real temporary Axum servers exercise disconnects, restart, commit races,
atomic rollback, encrypted identity, covers, policy/expiry changes, player
revocation, administrator configuration removal/restoration, token retries,
refresh-family revocation, login limits and stable error responses. Synthetic
encrypted vectors prove parser behavior only. Real deployments, password entry
and Minecraft GUI/rendering remain `MANUAL/PENDING` for the planned later stages.

## M3 implementation and deployment checkpoint

The source-equipment route and distribution closure are implemented and
capabilities are enabled. `POST models/{id}/equip` accepts
the documented operation/model/equipment revisions and defaults
`show_private_equipment` to false. Its response contains `model_id`,
`model_revision`, `equipment_revision`, `owner`, `asset_id`, `asset_revision`
and `show_private_equipment`. The alias reuses immutable package bytes and
persists its source binding, setting and equipment selection atomically.
Operation replay does not equip again; conflicting equipment revisions fail.

The local schema advances to v3 with `library_equipment_settings`, keyed by the
source-bound equipment reference. Existing bindings without a settings row are
closed by default. Upgrading v1/v2 creates a consistent pre-upgrade backup and
commits DDL/version together. Rollback requires restoring that snapshot; do not
lower user_version on a database containing the new table.
It also includes `library_verified_blobs`, indexed by immutable raw identity and
server-verified NBT/ciphertext/legacy identities. New resource commits record
these in the publication transaction. Historical blobs are boundedly scanned
once before observer access; client-declared content hashes are never accepted
as proof. Matching a blocked NBT identity denies repacked wardrobe copies
despite different compressed hashes, accounts or asset names.

Protobuf and Figura profile projections evaluate the observer independently,
including the Figura compatibility slot. A private render opt-in admits only
the uploader's currently equipped alias; catalog access stays private. Closing
the option or changing equipment denies subsequent alias reads. Revoking the
wearer's use permission also denies alias distribution even if an observer
still has a grant. The wearer's existing local selection is retained.

Explicit user amendment: BLOCKED models forced private cannot be opted into
external rendering. Existing true settings do not bypass moderation, and the
equipment transaction rejects attempts to enable them, including an uploader
acting with administrator context. The client control must be disabled in M6.
Blocked hash SELF_ONLY distribution remains mandatory across wardrobe copies.

Eight equipment/stream tests pass, including double-protocol profiles, private
intent changes, restart/replay, concurrent CAS, SQL-trigger rollback and block/
grant revocation, stale temporary slots and deterministic midstream denial.
Figura legacy downloads and Protobuf resource streams now share a 32-global /
two-per-player admission limit and recheck sessions/source access per 32 KiB
chunk, with bounded file reads. A real wardrobe-upload test covers repacking,
later moderation, post-ban uploads by another account, self access, guessed
Protobuf paths and an unaffected package. Full regression currently passes
75 tests (one saturation test runs in a separate batch). The legacy HTTP
baseline runs in a fresh child process because its process-global root cache
cannot share environment setup with parallel fixtures; no production behavior
was changed for test isolation.

Existing catalog copies now share an effective policy view for active verified
hash bans. Their persisted visibility stays PRIVATE across block/unblock, and
policy revisions advance together. A real admin API test verifies ordinary
list/count/detail hiding, admin BLOCKED discovery and transfer/equip rejection.

Both WebSocket adapters now recheck observer permissions before recovery and at
the send boundary. Active observers receive policy refresh/removal projections;
the wearer's local selection is retained. YSM state also matches the current
model and publisher generation; legacy fragments recheck at send checkpoints.
The legacy connection keeps its presented credential in connection memory and
revalidates it, rather than relying on an optional persisted profile field.
Three real WS tests cover private opt-in/removal, script ping filtering,
encrypted-source snapshot/resume/queued-state denial, the uploader/granted/
ungranted/configured-admin profile matrix and revocation of an observer grant.
Admin configuration does not grant observers the right to render model bytes.

The encrypted old-entry test changes the NBT wrapper while keeping the encrypted
source unchanged. It verifies lazy indexing of a historical internal upload,
post-ban v1 uploads and a self-only one-shot internal temporary upload. The
Windows staging flush uses a writable handle as required by FlushFileBuffers.
The existing internal Host/admin-token authority is unchanged by this milestone.

The client now exposes begin/current/publish guards for formal library equipment.
The request retires pending anonymous broadcasts and model preparation while
preserving the visible model on failure. Only a confirmed equip completion on
the current connection/selection reloads the source alias through profile and
download; it never uploads another `avatar` asset. Publication runs on the client
thread. The normal profile path refreshes Protobuf's equipment identity before
YSM publication. M4 owns the actual library HTTP client that uses this seam.

Final automated checks: cargo build, 75 regular Rust tests and one separate
worker-saturation test pass. Fabric compileJava, avatarCloudRefreshSmokeTest and
cloudAdapterSmokeTest pass. Logs are captured in build.log and copied to TEMP
as omnimorph-m3-{build,final-all,saturation,client}.log. Real deployment rollback
and visual/in-game acceptance remain MANUAL/PENDING, not equivalent to a PASS.
