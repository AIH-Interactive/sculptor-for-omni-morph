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
