# PSF key provisioning

Administrators manually place the 32-byte application key at:

```text
<directory containing sculptor.exe or sculptor>/keys/application.bin
```

The executable location determines this path, even when the working directory is
elsewhere. The repository tracks only `keys/.gitkeep`; real keys must never be
committed or included in release/CI artifacts. Grant read access only to the
service account and required administrators. Do not expose this directory through
a static-file route. Build and packaging must prepare an empty directory only.

A missing/unreadable key reports `PSF_KEY_UNAVAILABLE`; an invalid length reports
`PSF_KEY_LENGTH`. Existing Figura/YSM routes continue running. No embedded or
repository-key fallback is provided. Replacing a key is a manual administrator
operation and does not re-encrypt existing packages.

The new authenticated preparation endpoint is `POST /api/library/v1/psf/authorize`,
with the original encrypted package as `application/octet-stream` and
`x-omnimorph-psf-purpose: PUBLICATION` or `FITTING`. Use the existing player session
headers. It validates the complete package and issues a 120-second grant bound to
the account, session, content SHA256, PSF version and requested purpose. The master
key is never returned. Production traffic must use the existing HTTPS endpoint
with normal certificate validation; responses are `Cache-Control: no-store`.

Preparation alone does not publish or equip a model. PSF catalog and private
upload reservations and commits require a fresh `x-omnimorph-psf-grant` with
PUBLICATION purpose for the exact original encrypted source. Final publication
authenticates the source and rechecks the account, session and grant expiry.
FITTING and CONSUMPTION grants cannot authorize publication. Legacy/raw upload
paths reject PSF rather than bypassing this check.

Consumption resolves an already admitted stored resource through the existing
private-source, catalog-transfer or outfit-observer policy. The respective
`/psf-key` endpoints return a CONSUMPTION grant; arbitrary source uploads cannot
request that purpose. Cached bodies remain encrypted and require fresh admission.
The library advertises PSF and appends protocol capability 7, model type 4 and
distribution format 3, preserving all existing enum numbers. SQLite schema 13
migrates the existing type constraints while retaining prior data and indexes.

Existing upload quotas still apply. An administrator must configure a wardrobe
`max_avatar_size` large enough for the intended skin before private publication;
the example 100 KB default does not fit multi-megabyte PSF skins. No quota is
automatically raised by key provisioning.
Signing keys are independent random server secrets, not the application key that
local-fitting users may possess; restarting the server invalidates outstanding
grants. The client can ultimately inspect its own authorized plaintext, so this
scheme does not promise unextractable DRM.

Tests use generated temporary keys. Optional real-reference tests read an existing
user-provided directory through `OMNIMORPH_PSF_REFERENCE`; they do not copy real
keys into the backend installation or repository.

## M4 validation (2026-11-14)

Full Rust tests with reference authentication and Java cross-language gates:
218 passed, 0 failed, 1 existing ignored worker-saturation test. Optional artifact-
directory and old-process gates report their actual skipped status in the log.
The real Java PSF gate covers catalog/private publication, consumption on cache
hits, native Avatar loading, Apply, observer distribution and catalog revocation.
The release build passed. Game/manual visual acceptance was SKIPPED by the user;
no production deployment was performed.
