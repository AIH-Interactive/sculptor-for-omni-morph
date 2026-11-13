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

Preparation alone does not publish or equip a model. Final cloud publication and
existing-resource consumption integration belong to M4. M1 rejects arbitrary
source requests for consumption. Fitting grants cannot authorize publication.
Signing keys are independent random server secrets, not the application key that
local-fitting users may possess; restarting the server invalidates outstanding
grants. The client can ultimately inspect its own authorized plaintext, so this
scheme does not promise unextractable DRM.

Tests use generated temporary keys. Optional real-reference tests read an existing
user-provided directory through `OMNIMORPH_PSF_REFERENCE`; they do not copy real
keys into the backend installation or repository.
