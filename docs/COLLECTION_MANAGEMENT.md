# Collection management supplement

- `DELETE /api/library/v1/collection/:id` removes only the authenticated account's membership and outfit-candidate relationship. It is repeatable and can remove stale entries after model permissions are revoked.
- This endpoint does not delete the original model, downloaded files, another account's membership, or current equipment. The client uses the existing model deletion endpoint for its owner's own submissions.
- No schema or synchronization protocol changes are required. Existing authentication and UUID validation apply.
- Checkpoint (2026-10-20): `cargo test collection -- --nocapture` passed all four collection HTTP tests, including cross-account isolation, repeated deletion, inaccessible models, invalid IDs, original-model retention and persistence after restart.

## Current equipment visibility (2026-10-20)

- `POST /api/library/v1/models/:id/equip` accepts optional `show_equipment` for the wearer's current source-bound appearance, independently of catalog visibility. It shares the existing equipment CAS, replay fingerprint, policy checks and observer invalidation. Omitted fields preserve old public-visible/private-opt-in behavior and old request fingerprints.
- Public equipment reuses `library_wardrobe_settings`; private equipment retains uploader-only opt-in. No schema migration is required. Session responses add `show_equipment`; the legacy private field remains compatible.
- Hiding a public equipped alias removes it from both profile protocols and denies alias downloads, while canonical catalog reads and downloads retain their original permissions. This setting cannot bypass grants, bans or source checks.
- Validation: `cargo test equipment -- --nocapture` (13 passed), `cargo test subscriptions -- --nocapture` (3 passed). New public visibility HTTP regression failed against the original API before implementation and passed after the change. Deployment and real multiplayer UI checks remain pending.
