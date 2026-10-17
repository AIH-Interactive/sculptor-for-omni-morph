# OmniMorph client identity and Alpha handoff

Predecessor checkpoints: backend
`847b36704a17e9d6ef07a2026c397d6ac941651d`, client
`6fe492951489658300eb0ddd2e8d201e1ae71436`.
This checkpoint changes backend documentation only. Its production code and wire
contract remain the namespace checkpoint's tested code.

The client display name is OmniMorph, mod ID/resource namespace/artifact base is
omni-morph, Java root is top.aihmc.omnimorph. The canonical version is
1.0.0-alpha.1, with 1.0 Alpha in Help and Minecraft/commit build metadata in
development artifacts. Figura Lua API, Figura Cloud, original Blockbench Figura
format IDs, model-type labels, Lua APIs and upstream technical class names remain.

Help groups Documentation (OmniMorph Docs, Figura Lua Manual, YSM / Molang Guide)
and Project & Community (OmniMorph GitHub, Credits, Figura Upstream). The GitHub
button/Mod Menu/links command target https://github.com/AIH-Interactive.
Credits targets the exact omni-morph/main/CREDITS link; Upstream remains
https://github.com/FiguraMC/Figura. New-version downloads target the project's
releases. Old donation/community links are removed from those project surfaces.
Metadata retains upstream authors/contributors and declares PolyForm
Noncommercial 1.0.0. The supplied mod_icon.png was resized by script while
preserving the required metadata and UI atlas geometries/transparency.

When no new config exists, the client reads figura.json and saves omni-morph.json
without overwriting the original. Existing new config takes precedence. Existing
local Figura data remains selected when no new data directory exists; explicit
paths/new directories take precedence. No user model files are moved or deleted.
Upstream public JSON resource IDs/language keys are remapped on client mounting,
preserving technical names and bundled project Help/identity/icon assets. This
does not rewrite avatar packages or change backend raw transfer bytes/hashes.

The Blockbench format detector no longer derives its technical format ID from
the installed mod ID; both parser paths retain the original Figura format rules.
The language corpus audit now uses dynamic counts instead of its old 40/5 fixture
assertion; every detected model's dictionaries are still checked, with both
plain/encrypted classes and real translated primitive keys required.

Final foreground gates passed:

- :common:test, including wire/adapter/config/endpoint and new identity smoke
  gates; :common:ysmMaterialNativeSmokeTest and writeCloudIntegrationClasspath.
- The identity gate uses actual ConfigManager/FiguraMod data paths, old/new
  configuration precedence, retained local model markers, packaged language
  dictionaries and actual public-resource remapping with preserved technical names.
- ysmLanguageSmokeTest: 43 YSM models, 8 encrypted, 55 dictionaries, 2160 primitive
  keys; shared Figura/YSM consumers, locale fallback and isolation remain passing.
- ysmLocalCorpusSmokeTest: dynamically 5 Figura compiled, 35 ordinary YSM
  assembled and 8 encrypted assembled/rehydrated; payload, sync and crypto gates
  passed. Existing embedded-texture fallback diagnostics remain outside scope.
- :fabric:build --console=plain passed. The actual Alpha JAR was inspected for
  mod ID/name/version/license, one GitHub link, renamed entry point/Mixin/access
  widener, new Java/resource namespaces, actual icon atlas sizes and bilingual
  Help labels. No old root package or stale assets/figura entries were present.
- cargo test --locked --offline -- --nocapture with all B8 classpath/old/new
  executable inputs: 20 tests and 12 production Java protocol/model pairs passed,
  including original backend process compatibility, crash/restart/rollback and
  route-toggle gates. cargo build --release --locked --offline passed.
- Scoped diff checks passed; original README backup hash remains unchanged.

The final client checkpoint is committed/pushed after these gates, then its JAR
is rebuilt to embed that SHA. The client release recorder writes paired source
SHAs, actual sizes/version/mod ID/schema/artifact hashes to
build/reports/cloud/final-release.json. Archived B8 artifacts remain separate.

Current validated release target remains Minecraft 26.1.2 Fabric. Other loader
directories only receive identity metadata here; the requested future ports are
not claimed complete. Public deployment and real game UI/render/audio/two-client
observations remain SKIPPED/MANUAL_BY_USER; no asset or CPU test substitutes for
them. Source startup/configuration instructions are in DEPLOYMENT.md: cargo run
--locked reads Config.toml or RUST_CONFIG and performs any needed compilation
automatically, without a separate manual executable-build step.
