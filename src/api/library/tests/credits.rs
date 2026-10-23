use super::*;

#[tokio::test]
async fn native_java_accessory_packages_publish_through_rust_routes() {
    let Ok(directory) = std::env::var("OMNIMORPH_TEST_ACCESSORY_UPLOAD_DIRECTORY") else {
        println!("SKIPPED native accessory upload: set OMNIMORPH_TEST_ACCESSORY_UPLOAD_DIRECTORY");
        return;
    };
    let server = Server::start().await;
    for kind in ["figura", "encrypted"] {
        let source = std::path::Path::new(&directory).join(kind);
        let bytes = std::fs::read(source.join("body.nbt")).unwrap();
        let mut input: Value =
            serde_json::from_slice(&std::fs::read(source.join("reserve.json")).unwrap()).unwrap();
        input["operation_id"] = json!(library::random_id());
        let response = server
            .request(Method::POST, "/api/library/v1/uploads", 1)
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            200,
            "{kind}: {}",
            response.text().await.unwrap()
        );
        let reservation: Value = response.json().await.unwrap();
        let response = server
            .request(
                Method::PUT,
                reservation["relative_path"].as_str().unwrap(),
                1,
            )
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            204,
            "{kind}: {}",
            response.text().await.unwrap()
        );
        let commit = json!({"operation_id": library::random_id(), "expected_revision": 1});
        for _ in 0..2 {
            let response = server
                .request(
                    Method::POST,
                    &format!(
                        "/api/library/v1/uploads/{}/commit",
                        input["operation_id"].as_str().unwrap()
                    ),
                    1,
                )
                .json(&commit)
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                200,
                "{kind}: {}",
                response.text().await.unwrap()
            );
        }
        let id = reservation["model_id"].as_str().unwrap();
        let detail: Value = server
            .request(Method::GET, &format!("/api/library/v1/models/{id}"), 1)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(detail["resource_kind"], "ACCESSORY");
        let expected: crate::service::outfit::AccessoryMetadata =
            serde_json::from_value(input["metadata"]["accessory_metadata"].clone()).unwrap();
        assert_eq!(
            detail["accessory_metadata"],
            serde_json::to_value(expected).unwrap()
        );
        let collection: Value = server
            .request(Method::GET, "/api/library/v1/collection", 1)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(collection["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["model_id"] == id));
    }
}

#[tokio::test]
async fn multiline_author_credits_publish_without_losing_original_grouping() {
    use std::io::Write;
    let server = Server::start().await;
    // Figura stores array entries joined with newlines in the compiled NBT.
    let credits = "Mainmodel:\n  OvOla2\n\nScripts:\nEZAnims & EZMount - JimmyHelp\nOvOla2";
    let mut nbt = vec![10, 0, 0, 10, 0, 8];
    nbt.extend_from_slice(b"metadata");
    for (name, value) in [("name", "Internal"), ("authors", credits)] {
        nbt.push(8);
        nbt.extend_from_slice(&(name.len() as u16).to_be_bytes());
        nbt.extend_from_slice(name.as_bytes());
        nbt.extend_from_slice(&(value.len() as u16).to_be_bytes());
        nbt.extend_from_slice(value.as_bytes());
    }
    nbt.extend_from_slice(&[0, 0]);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&nbt).unwrap();
    let bytes = encoder.finish().unwrap();
    let mut input = upload_input(&bytes, "PUBLIC");
    input["metadata"]["authors"] = json!([credits]);
    let (bytes, original_input) = match std::env::var("OMNIMORPH_TEST_UPLOAD_DIRECTORY") {
        Ok(directory) => {
            let directory = std::path::Path::new(&directory);
            let bytes = std::fs::read(directory.join("package.nbt")).unwrap();
            let input: Value =
                serde_json::from_slice(&std::fs::read(directory.join("upload.json")).unwrap())
                    .unwrap();
            (bytes, input)
        }
        Err(_) => (bytes, input),
    };
    for forged in [true, false] {
        let mut input = original_input.clone();
        if forged {
            input["metadata"]["authors"] = json!(["Forged attribution"]);
        }
        input["operation_id"] = json!(library::random_id());
        let response = server
            .request(Method::POST, "/api/library/v1/uploads", 1)
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        let reservation = response.json::<Value>().await.unwrap();
        let response = server
            .request(
                Method::PUT,
                reservation["relative_path"].as_str().unwrap(),
                1,
            )
            .header("content-type", "application/octet-stream")
            .body(bytes.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        let response = server
            .request(
                Method::POST,
                &format!(
                    "/api/library/v1/uploads/{}/commit",
                    input["operation_id"].as_str().unwrap()
                ),
                1,
            )
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 1}))
            .send()
            .await
            .unwrap();
        if forged {
            assert_eq!(response.status(), 400, "{}", response.text().await.unwrap());
            continue;
        }
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        let detail = server
            .request(
                Method::GET,
                &format!(
                    "/api/library/v1/models/{}",
                    reservation["model_id"].as_str().unwrap()
                ),
                1,
            )
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(detail["authors"], input["metadata"]["authors"]);
    }
}
