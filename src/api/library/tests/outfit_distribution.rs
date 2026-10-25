use super::*;
use crate::service::{outfit::*, outfit_limits::OutfitLimits};

#[test]
fn asset_distribution_rollback_preserves_owner_sources_and_blocks_observer_bodies() {
    const CHILD: &str = "OMNIMORPH_DISTRIBUTION_ROLLBACK_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "api::library::tests::outfit_distribution::asset_distribution_rollback_preserves_owner_sources_and_blocks_observer_bodies", "--nocapture"])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    // The switch is process-wide. A dedicated child prevents interference with other test servers.
    std::env::set_var("OUTFIT_ASSET_DISTRIBUTION", "true");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let server = Server::start().await;
        let owner = Uuid::from_u128(1);
        let native = outfit_resources::upload_private(&server, "rollback-accessory").await;
        let reference = ResourceRef::Local {
            local_id: library::random_id().parse().unwrap(),
            content_hash: "c".repeat(64),
            format: "FIGURA".into(),
        };
        let binding = crate::storage::outfit_resources::LocalBinding {
            reference: reference.clone(), asset_id: native.asset_id.clone(), asset_revision: native.revision,
            raw_sha256: faster_hex::hex_string(&native.raw_sha256), resource_kind: ResourceKind::Accessory,
            accessory_metadata: Some(serde_json::from_value(json!({
                "schema_version": 1, "category": "General", "cosmetic_anchor": "head_anchor",
                "default_transform": {"position": [0,0,0], "rotation": [0,0,0], "scale": [1,1,1]},
                "supported_bases": ["VANILLA"], "has_bones": false, "skeleton_revision": "",
                "skeleton_hash": "", "display_bones": false, "edit_bones": false,
                "exported_bones": [], "validation_source": "CLIENT_EXTRACTED"
            })).unwrap()),
        };
        let limits = OutfitLimits::default();
        server.state.storage.outfit_bind_local(owner, &binding, &limits).unwrap();
        let mut configuration = OutfitConfig::default();
        configuration.show_equipment = true;
        configuration.display_ordinals.insert(reference.identity(), 1);
        let instance = Uuid::from_u128(100);
        configuration.instances.push(AccessoryInstance {
            instance_id: instance, resource: reference.clone(), display_ordinal: 1, enabled: true,
            anchor: AnchorRef::Standard { name: "head_anchor".into() }, transform: Transform::default(),
            bone_overrides: Vec::new(), controls: Default::default(), lua_state: Default::default(),
        });
        server.state.storage.outfit_apply(Actor { uuid: owner, admin_context: false, granted: false },
            &server.base, &crate::storage::outfit_apply::ApplyInput {
                operation_id: library::random_id().parse().unwrap(), expected_revision: 0, configuration,
            }, &limits).unwrap();
        let snapshot = server.state.storage.outfit_project(owner, Uuid::from_u128(2), &server.base, &limits).unwrap();
        assert_eq!(snapshot.resources.len(), 1);
        let path = snapshot.resources[0].relative_path.clone();
        let read = |viewer| server.request(Method::GET, &path, viewer)
            .header("x-omnimorph-origin", &server.base).send();
        assert_eq!(read(2).await.unwrap().status(), 200);
        std::env::set_var("OUTFIT_ASSET_DISTRIBUTION", "false");
        let hidden = server.state.storage.outfit_project(owner, Uuid::from_u128(2), &server.base, &limits).unwrap();
        assert!(hidden.resources.is_empty());
        assert!(hidden.configuration.unwrap().instances.is_empty());
        assert_eq!(read(2).await.unwrap().status(), 404);
        assert_eq!(read(1).await.unwrap().status(), 200);
        let capabilities = server.request(Method::GET, "/api/library/v1/capabilities", 1)
            .send().await.unwrap().json::<Value>().await.unwrap();
        assert_eq!(capabilities["outfits_v1"]["asset_distribution"], false);
        let state_path = format!("/api/library/v1/outfits/players/{owner}/instances/{instance}/state");
        assert_eq!(server.request(Method::GET, &state_path, 2)
            .header("x-omnimorph-origin", &server.base).send().await.unwrap().status(), 404);
        assert_eq!(server.request(Method::GET, &state_path, 1)
            .header("x-omnimorph-origin", &server.base).send().await.unwrap().status(), 200);
        assert_eq!(server.state.storage.outfit_resolve_local(owner, &reference, ResourceKind::Accessory, &limits).unwrap(), Some(binding.clone()));
        assert!(server.state.storage.outfit_bind_local(owner, &binding, &limits).is_err());
        assert_eq!(server.state.storage.avatar(owner, &native.asset_id, native.revision).unwrap(), native);
        std::env::set_var("OUTFIT_ASSET_DISTRIBUTION", "true");
        assert_eq!(read(2).await.unwrap().status(), 200);
    });
}
