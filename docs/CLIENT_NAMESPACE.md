# Client Java namespace migration

Paired predecessor checkpoints: backend
`efe4a9523fbd7b5ca53e49509dcf32bfcf2bd2bb`, client
`eaf7201c6510454f1694ac29a6616d41ca7620ef` (B8).

The authorized client root package is now top.aihmc.omnimorph. Its production,
test and platform source paths, imports, reflective names, Mixin configuration,
Fabric entry point and Gradle Java test mains were updated. Figura technical
class/API names remain. This checkpoint does not yet change mod ID, visible
branding, version or icons; those are the next user-requested checkpoint.

The canonical and vendored schema change only the java_package option to
top.aihmc.omnimorph.backend2.protocol.generated. The Protobuf package, field
numbers, messages, wire format and golden vectors are unchanged. Source digest:
6fff18b25ac2f6b88d1530add0dbc30546136506f878568f86bf713388a9e741.
Both release manifests match. Historical B0-B8 records keep their original digest.
The backend Java launchers invoke the renamed integration class.

Final gates: :common:test (wire/adapter/config/endpoint security),
:common:ysmMaterialNativeSmokeTest, :common:writeCloudIntegrationClasspath and
:fabric:build --console=plain PASS. The actual JAR contains zero classes under
org/figuramc/figura and 1270 under top/aihmc/omnimorph. Generated Java bindings
also use the new directory. No generated source is committed.

cargo build --release --locked --offline PASS. cargo test --locked --offline
-- --nocapture with all three B8 Java/rollback/delivery environment inputs:
20 tests and all 12 Java protocol-pair/model cases PASS, including the new Java
adapter against the actual baseline backend process. Migration/restart/rollback
and main route toggles remain passing. Paired frozen-release verification and
scoped diff checks PASS. Original README backup SHA-256 remains
7971ba229264fb3d6cce9923a944b48946a71d38b887a2fc6cdea7bc7d906105.

Real-game/manual/public deployment states remain SKIPPED/MANUAL_BY_USER.
