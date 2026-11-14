use super::*;
use crate::service::psf::{self, Purpose};
use base64::{engine::general_purpose::STANDARD, Engine};

#[tokio::test]
async fn psf_key_route_requires_session_manual_key_and_authenticated_source_without_exposing_master(
) {
    let server = Server::start().await;
    let master: [u8; 32] = rand::random();
    let source = psf::tests::fixture(&master);
    let path = "/api/library/v1/psf/authorize";
    let request = || {
        server
            .request(Method::POST, path, 1)
            .header("content-type", "application/octet-stream")
            .header("x-omnimorph-psf-purpose", "PUBLICATION")
            .body(source.clone())
    };
    assert_eq!(
        server
            .client
            .post(format!("{}{path}", server.base))
            .body(source.clone())
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let missing = request().send().await.unwrap();
    assert_eq!(missing.status(), 503);
    assert_eq!(
        missing.json::<Value>().await.unwrap()["code"],
        "PSF_KEY_UNAVAILABLE"
    );
    // Missing PSF provisioning must not disable existing Figura/YSM service routes.
    assert_eq!(
        server
            .request(Method::GET, "/api/library/v1/capabilities", 1)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let keys = server._directory.path().join("keys");
    std::fs::create_dir(&keys).unwrap();
    std::fs::write(keys.join("application.bin"), master).unwrap();
    let response = request().send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let text = response.text().await.unwrap();
    assert!(!text.contains(&STANDARD.encode(master)));
    let result: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(result["content_sha256"], psf::digest(&source));
    assert_eq!(result["purpose"], "PUBLICATION");
    assert_eq!(
        result["package_key"],
        STANDARD.encode(psf::authenticate(&source, &master).unwrap())
    );
    let binding = psf::digest(b"Bearer synthetic-token-1\0\0figura");
    server
        .state
        .psf_keys
        .verify(
            result["grant"].as_str().unwrap(),
            Uuid::from_u128(1),
            &binding,
            &psf::digest(&source),
            Purpose::Publication,
            crate::storage::now_seconds(),
        )
        .unwrap();
    assert!(server
        .state
        .psf_keys
        .verify(
            result["grant"].as_str().unwrap(),
            Uuid::from_u128(2),
            &binding,
            &psf::digest(&source),
            Purpose::Publication,
            crate::storage::now_seconds()
        )
        .is_err());
    let mut corrupted = source.clone();
    *corrupted.last_mut().unwrap() ^= 1;
    assert_eq!(
        server
            .request(Method::POST, path, 1)
            .header("content-type", "application/octet-stream")
            .header("x-omnimorph-psf-purpose", "PUBLICATION")
            .body(corrupted)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        server
            .request(Method::POST, path, 1)
            .header("content-type", "application/octet-stream")
            .header("x-omnimorph-psf-purpose", "CONSUMPTION")
            .body(source)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}

#[tokio::test]
async fn psf_private_consumption_resolves_stored_binding_and_rechecks_owner_conflict_and_revoke() {
    let server = Server::start().await;
    let owner = Uuid::from_u128(1);
    let master: [u8; 32] = rand::random();
    let encrypted = crate::service::psf::tests::fixture(&master);
    let bytes = crate::service::package::tests::psf_distribution(&encrypted, None);
    let hash = crate::storage::sha256(&bytes);
    let path = server.state.storage.blob_path(&hash).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    // This fixture installs an already stored immutable source. It is not an upload/publication test.
    let descriptor = server
        .state
        .storage
        .commit_avatar(
            owner,
            &library::random_id(),
            &hash,
            crate::protocol::v1::AvatarDescriptor {
                owner: owner.to_string(),
                asset_id: "private_psf".into(),
                model_type: 4,
                distribution_format: 3,
                raw_sha256: hash.clone(),
                raw_length: bytes.len() as u64,
                content_sha256: crate::storage::sha256(&encrypted),
                schema_sha256: vec![1; 32],
                required_capabilities: vec![7],
                ..Default::default()
            },
            64,
        )
        .unwrap();
    let binding = json!({
        "reference": {"source":"LOCAL", "local_id":library::random_id(),
            "content_hash":crate::service::psf::digest(&encrypted), "format":"PSF"},
        "asset_id":descriptor.asset_id, "asset_revision":descriptor.revision,
        "raw_sha256":faster_hex::hex_string(&hash), "resource_kind":"AVATAR", "accessory_metadata":null
    });
    let response = server
        .request(Method::PUT, "/api/library/v1/outfits/resources/local", 1)
        .json(&binding)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let mut request = binding.clone();
    request
        .as_object_mut()
        .unwrap()
        .remove("accessory_metadata");
    let endpoint = "/api/library/v1/outfits/resources/local/psf-key";
    assert_eq!(
        server
            .request(Method::POST, endpoint, 1)
            .json(&request)
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    std::fs::create_dir(server._directory.path().join("keys")).unwrap();
    std::fs::write(
        server._directory.path().join("keys/application.bin"),
        master,
    )
    .unwrap();
    let response = server
        .request(Method::POST, endpoint, 1)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert_eq!(response.headers()["cache-control"], "no-store");
    let grant = response.json::<Value>().await.unwrap();
    assert_eq!(grant["purpose"], "CONSUMPTION");
    assert_ne!(grant["package_key"], STANDARD.encode(master));
    assert_eq!(
        server
            .request(Method::POST, endpoint, 2)
            .json(&request)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let mut wrong = request.clone();
    wrong["asset_revision"] = json!(descriptor.revision + 1);
    assert_eq!(
        server
            .request(Method::POST, endpoint, 1)
            .json(&wrong)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    server
        .state
        .storage
        .delete(
            owner,
            &crate::protocol::v1::Delete {
                operation_id: library::random_id(),
                asset_id: descriptor.asset_id,
                expected_revision: 1,
            },
        )
        .unwrap();
    assert_eq!(
        server
            .request(Method::POST, endpoint, 1)
            .json(&request)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

async fn prepare_grant(server: &Server, source: &[u8], purpose: &str) -> Value {
    let response = server
        .request(Method::POST, "/api/library/v1/psf/authorize", 1)
        .header("content-type", "application/octet-stream")
        .header("x-omnimorph-psf-purpose", purpose)
        .body(source.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    response.json().await.unwrap()
}

#[tokio::test]
async fn psf_publication_requires_exact_current_grant_at_reserve_and_commit_and_blocks_raw_bypass()
{
    use crate::protocol::v1::CommitUpload;
    use crate::service::resource;
    let server = Server::start().await;
    let owner = Uuid::from_u128(1);
    let master: [u8; 32] = rand::random();
    std::fs::create_dir(server._directory.path().join("keys")).unwrap();
    std::fs::write(
        server._directory.path().join("keys/application.bin"),
        master,
    )
    .unwrap();
    let encrypted = crate::service::psf::tests::fixture(&master);
    let bytes = crate::service::package::tests::psf_distribution(&encrypted, None);
    let mut input = outfit_uploads::input(&bytes);
    let crate::service::outfit::ResourceRef::Local {
        content_hash,
        format,
        ..
    } = &mut input.reference
    else {
        panic!("local fixture required");
    };
    *format = "PSF".into();
    *content_hash = psf::digest(&encrypted);
    input.content_sha256 = Some(psf::digest(&encrypted));
    input.schema_sha256 = Some("1".repeat(64));
    let grant = prepare_grant(&server, &encrypted, "PUBLICATION").await;
    let token = grant["grant"].as_str().unwrap();
    let fitting = prepare_grant(&server, &encrypted, "FITTING").await;
    let reserve_path = "/api/library/v1/outfits/resources/uploads";
    for proof in [None, Some(fitting["grant"].as_str().unwrap())] {
        let request = server.request(Method::POST, reserve_path, 1).json(&input);
        let request = if let Some(proof) = proof {
            request.header("x-omnimorph-psf-grant", proof)
        } else {
            request
        };
        assert_eq!(request.send().await.unwrap().status(), 403);
    }
    assert_eq!(
        server
            .request(Method::POST, reserve_path, 2)
            .header("x-omnimorph-psf-grant", token)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        server
            .request(Method::POST, reserve_path, 1)
            .header("x-omnimorph-psf-grant", token)
            .header("x-omnimorph-epoch", "changed")
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let raw_request = input.request(owner).unwrap();
    assert!(matches!(
        resource::grant_upload(&server.state, owner, &raw_request).await,
        Err(ErrorCode::Forbidden)
    ));
    let response = server
        .request(Method::POST, reserve_path, 1)
        .header("x-omnimorph-psf-grant", token)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let reserved = response.json::<Value>().await.unwrap();
    assert_eq!(
        server
            .request(Method::PUT, reserved["relative_path"].as_str().unwrap(), 1)
            .header("content-type", "application/octet-stream")
            .body(bytes.clone())
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let commit_path = format!("{reserve_path}/{}/commit", input.operation_id);
    assert_eq!(
        server
            .request(Method::POST, &commit_path, 1)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let commit = CommitUpload {
        operation_id: input.operation_id.to_string(),
        raw_sha256: crate::storage::sha256(&bytes),
        raw_length: bytes.len() as u64,
    };
    assert!(matches!(
        resource::commit_upload(&server.state, owner, &commit).await,
        Err(ErrorCode::Forbidden)
    ));
    let session = psf::digest(b"Bearer synthetic-token-1\0\0figura");
    let expired = server
        .state
        .psf_keys
        .authorize(
            &encrypted,
            owner,
            &session,
            Purpose::Publication,
            crate::storage::now_seconds().saturating_sub(121),
        )
        .unwrap();
    assert_eq!(
        server
            .request(Method::POST, &commit_path, 1)
            .header("x-omnimorph-psf-grant", &expired.grant)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let response = server
        .request(Method::POST, &commit_path, 1)
        .header("x-omnimorph-psf-grant", token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let binding = response.json::<Value>().await.unwrap();
    assert_eq!(binding["reference"]["format"], "PSF");
    assert_eq!(
        server
            .request(Method::POST, &commit_path, 1)
            .header("x-omnimorph-psf-grant", token)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let mut consumption = binding.clone();
    consumption
        .as_object_mut()
        .unwrap()
        .remove("accessory_metadata");
    let response = server
        .request(
            Method::POST,
            "/api/library/v1/outfits/resources/local/psf-key",
            1,
        )
        .json(&consumption)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert_eq!(
        response.json::<Value>().await.unwrap()["purpose"],
        "CONSUMPTION"
    );
    let body_query = json!({"reference": binding["reference"], "resource_kind":"AVATAR", "raw_sha256":binding["raw_sha256"]});
    let response = server
        .request(
            Method::POST,
            "/api/library/v1/outfits/resources/local/body",
            1,
        )
        .json(&body_query)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), bytes);
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap().revision,
        0
    );
    let mut configuration = crate::service::outfit::OutfitConfig::default();
    configuration.base = Some(serde_json::from_value(binding["reference"].clone()).unwrap());
    configuration.show_equipment = true;
    let response = server.request(Method::PUT, "/api/library/v1/outfits/current", 1)
        .header("x-omnimorph-origin", &server.base)
        .json(&json!({"operation_id":library::random_id(), "expected_revision":0, "configuration":&configuration}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let snapshot_path = format!("/api/library/v1/outfits/players/{owner}");
    let response = server
        .request(Method::GET, &snapshot_path, 2)
        .header("x-omnimorph-origin", &server.base)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let snapshot = response.json::<Value>().await.unwrap();
    let resource = &snapshot["resources"][0];
    assert_eq!(resource["distribution_format"], 3);
    let key_path = format!(
        "{snapshot_path}/resources/{}/psf-key?revision=1",
        resource["id"].as_str().unwrap()
    );
    let response = server
        .request(Method::POST, &key_path, 2)
        .header("x-omnimorph-origin", &server.base)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        response.json::<Value>().await.unwrap()["purpose"],
        "CONSUMPTION"
    );
    configuration.revision = 1;
    configuration.show_equipment = false;
    let response = server.request(Method::PUT, "/api/library/v1/outfits/current", 1)
        .header("x-omnimorph-origin", &server.base)
        .json(&json!({"operation_id":library::random_id(), "expected_revision":1, "configuration":&configuration}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    for path in [
        key_path.clone(),
        key_path.replace("revision=1", "revision=2"),
    ] {
        assert!(
            !server
                .request(Method::POST, &path, 2)
                .header("x-omnimorph-origin", &server.base)
                .send()
                .await
                .unwrap()
                .status()
                .is_success(),
            "hidden equipment must not issue observer keys"
        );
    }
}

#[tokio::test]
async fn psf_catalog_publication_rechecks_grant_on_commit_and_preserves_native_descriptor() {
    let server = Server::start().await;
    let master: [u8; 32] = rand::random();
    std::fs::create_dir(server._directory.path().join("keys")).unwrap();
    std::fs::write(
        server._directory.path().join("keys/application.bin"),
        master,
    )
    .unwrap();
    let encrypted = crate::service::psf::tests::fixture(&master);
    let bytes = crate::service::package::tests::psf_distribution(&encrypted, None);
    let grant = prepare_grant(&server, &encrypted, "PUBLICATION").await;
    let token = grant["grant"].as_str().unwrap();
    let mut input = upload_input(&bytes, "PUBLIC");
    input["metadata"]["model_type"] = json!("PSF");
    input["metadata"]["internal_name"] = json!("fixture");
    input["metadata"]["authors"] = json!([]);
    input["source_filename"] = json!("fixture.PSF");
    input["metadata_source"] = json!("psf_manifest");
    input["content_sha256"] = json!(psf::digest(&encrypted));
    input["schema_sha256"] = json!("1".repeat(64));
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/uploads", 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let response = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .header("x-omnimorph-psf-grant", token)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let reserved = response.json::<Value>().await.unwrap();
    assert_eq!(
        server
            .request(Method::PUT, reserved["relative_path"].as_str().unwrap(), 1)
            .header("content-type", "application/octet-stream")
            .body(bytes.clone())
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let path = format!(
        "/api/library/v1/uploads/{}/commit",
        input["operation_id"].as_str().unwrap()
    );
    let commit = json!({"operation_id":library::random_id(), "expected_revision":1});
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&commit)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let response = server
        .request(Method::POST, &path, 1)
        .header("x-omnimorph-psf-grant", token)
        .json(&commit)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let id = reserved["model_id"].as_str().unwrap();
    let response = server
        .request(Method::GET, &format!("/api/library/v1/models/{id}"), 1)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let model = response.json::<Value>().await.unwrap();
    assert_eq!(model["model_type"], "PSF");
    let response = server
        .request(
            Method::POST,
            &format!("/api/library/v1/models/{id}/transfers"),
            2,
        )
        .json(
            &json!({"operation_id":library::random_id(), "model_revision":1, "purpose":"preview"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let transfer = response.json::<Value>().await.unwrap();
    let key_path = format!(
        "/api/library/v1/transfers/{}/psf-key",
        transfer["transfer_id"].as_str().unwrap()
    );
    let response = server
        .request(Method::POST, &key_path, 2)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        response.json::<Value>().await.unwrap()["content_sha256"],
        psf::digest(&encrypted)
    );
    assert!(!server
        .request(Method::POST, &key_path, 3)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let response = server.request(Method::PATCH, &format!("/api/library/v1/models/{id}"), 1)
        .json(&json!({"operation_id":library::random_id(), "expected_revision":model["policy_revision"], "visibility":"PRIVATE"}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert!(
        !server
            .request(Method::POST, &key_path, 2)
            .send()
            .await
            .unwrap()
            .status()
            .is_success(),
        "a transfer issued while public cannot keep granting keys after policy revocation"
    );
}

#[tokio::test]
async fn psf_java_real_source_publication_consumption_and_native_avatar() {
    let (Some(reference), Some(_)) = (
        std::env::var_os("OMNIMORPH_PSF_REFERENCE"),
        std::env::var_os("OMNIMORPH_CLIENT_CLASSPATH_FILE"),
    ) else {
        println!("SKIPPED PSF Java/Rust: reference and client classpath required");
        return;
    };
    let reference = std::path::PathBuf::from(reference);
    // Read the manually supplied reference key in place; never copy it into test artifacts.
    let keys = Arc::new(psf::Keys::reference(reference.join("application.bin")));
    let server = Server::start_with_psf(false, Some(keys)).await;
    // The supplied skin is 4.6 MB. Exercise the administrator-configured quota, not
    // the example's 100 KB legacy wardrobe default; production defaults stay unchanged.
    server
        .state
        .config
        .write()
        .await
        .limitations
        .max_avatar_size = 64 * 1024;
    crate::api::protobuf::tests::java_client::execute(
        &[
            "top.aihmc.omnimorph.model.ysm.YsmMaterialNativeSmokeTest".into(),
            "--psf-cloud".into(),
            server.base.clone(),
            reference.to_string_lossy().into_owned(),
            server.state.storage.instance_id().to_owned(),
        ],
        server._directory.path(),
    )
    .await;
}

#[tokio::test]
async fn psf_legacy_raw_upload_cannot_bypass_publication_admission() {
    let server = Server::start().await;
    let owner = Uuid::from_bytes(rand::random());
    server
        .state
        .user_manager
        .insert(
            owner,
            "psf-legacy-admission".into(),
            Userinfo {
                uuid: owner,
                nickname: "PSF legacy admission".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let source = psf::tests::fixture(&rand::random());
    let bytes = crate::service::package::tests::psf_distribution(&source, None);
    let response = server
        .client
        .put(format!("{}/api/avatar", server.base))
        .header("token", "psf-legacy-admission")
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(server
        .state
        .storage
        .equipped(owner)
        .unwrap()
        .primary
        .is_none());
}
