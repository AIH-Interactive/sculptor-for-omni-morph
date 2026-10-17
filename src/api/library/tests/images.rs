use super::*;

fn png() -> Vec<u8> {
    let image = image::DynamicImage::new_rgba8(2, 2);
    let mut output = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

#[tokio::test]
async fn cover_signature_size_permissions_and_clear_are_checked_without_fallback_image_copies() {
    let server = Server::start().await;
    let (id, _) = server.upload_model("PUBLIC").await;
    let path = format!("/api/library/v1/models/{id}/image");
    let missing = server.request(Method::GET, &path, 2).send().await.unwrap();
    assert_eq!(missing.status(), 404);
    assert_eq!(missing.json::<Value>().await.unwrap()["code"], "NO_IMAGE");
    let image = png();
    let operation = library::random_id();
    for _ in 0..2 {
        assert_eq!(
            server
                .request(Method::PUT, &path, 1)
                .header("content-type", "image/png")
                .header("x-omnimorph-operation", &operation)
                .header("x-omnimorph-expected-revision", "2")
                .body(image.clone())
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    let response = server.request(Method::GET, &path, 2).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), image);
    let bad = server
        .request(Method::PUT, &path, 1)
        .header("content-type", "image/png")
        .header("x-omnimorph-operation", library::random_id())
        .header("x-omnimorph-expected-revision", "3")
        .body(b"<html>not an image</html>".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 415);
    let response = server
        .request(Method::PUT, &path, 1)
        .header("content-type", "image/png")
        .header("x-omnimorph-operation", library::random_id())
        .header("x-omnimorph-expected-revision", "3")
        .body(vec![0; 2 * 1024 * 1024 + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    assert_eq!(
        server
            .request(Method::PUT, &path, 2)
            .header("content-type", "image/png")
            .header("x-omnimorph-operation", library::random_id())
            .header("x-omnimorph-expected-revision", "3")
            .body(image)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let clear = json!({"operation_id": library::random_id(), "expected_revision": 3});
    assert_eq!(
        server
            .request(Method::DELETE, &path, 1)
            .json(&clear)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        server
            .request(Method::GET, &path, 2)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}
