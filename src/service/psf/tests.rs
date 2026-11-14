use super::*;

pub(crate) fn fixture(master: &[u8; 32]) -> Vec<u8> {
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;
    let resource = b"generated PSF test resource";
    let payload = serde_json::json!({
        "format": "PieceGrassSklnFiles", "version": 1,
        "entry": {"id":"fixture", "skin":"fixture", "category":"skin"},
        "models": {}, "textures": {},
        "files": {"resource.txt": {"size": resource.len(),
            "sha256": crate::service::psf::digest(resource), "data": STANDARD.encode(resource)}}
    });
    let plain = serde_json::to_vec(&payload).unwrap();
    let mut header = [0u8; 48];
    header[..8].copy_from_slice(b"PSF1PGSF");
    header[8..10].copy_from_slice(&1u16.to_le_bytes());
    header[10..12].copy_from_slice(&2u16.to_le_bytes());
    let random: [u8; 28] = rand::random();
    header[16..44].copy_from_slice(&random);
    header[44..48].copy_from_slice(&(plain.len() as u32).to_le_bytes());
    let extracted = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &header[16..32]), master);
    let derived = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, extracted.as_ref()),
        b"PieceGrassSklnFiles/1\x01",
    );
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    gzip.write_all(&plain).unwrap();
    let mut buffer = gzip.finish().unwrap();
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, derived.as_ref()).unwrap(),
    );
    key.seal_in_place_append_tag(
        aead::Nonce::try_assume_unique_for_key(&header[32..44]).unwrap(),
        aead::Aad::from(&header),
        &mut buffer,
    )
    .unwrap();
    [&header[..], &buffer].concat()
}

#[test]
fn psf_keys_require_manual_configuration_and_bind_exact_purpose_identity_and_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("sculptor.exe");
    let keys = Keys::beside_executable(&executable);
    let master: [u8; 32] = rand::random();
    let source = fixture(&master);
    let owner = Uuid::from_u128(1);
    assert!(matches!(
        keys.authorize(&source, owner, "session", Purpose::Publication, 100),
        Err(Error::KeyUnavailable)
    ));
    std::fs::create_dir(directory.path().join("keys")).unwrap();
    let key_file = directory.path().join("keys/application.bin");
    std::fs::write(&key_file, [0u8; 33]).unwrap();
    assert!(matches!(
        keys.authorize(&source, owner, "session", Purpose::Publication, 100),
        Err(Error::KeyLength)
    ));
    std::fs::write(&key_file, master).unwrap();
    let grant = keys
        .authorize(&source, owner, "session", Purpose::Publication, 100)
        .unwrap();
    assert_ne!(STANDARD.decode(&grant.package_key).unwrap(), master);
    let content = crate::service::psf::digest(&source);
    assert!(keys
        .verify(
            &grant.grant,
            owner,
            "session",
            &content,
            Purpose::Publication,
            101
        )
        .is_ok());
    for (account, session, hash, purpose, now) in [
        (
            Uuid::from_u128(2),
            "session",
            content.as_str(),
            Purpose::Publication,
            101,
        ),
        (
            owner,
            "different",
            content.as_str(),
            Purpose::Publication,
            101,
        ),
        (owner, "session", "wrong-hash", Purpose::Publication, 101),
        (owner, "session", content.as_str(), Purpose::Fitting, 101),
        (
            owner,
            "session",
            content.as_str(),
            Purpose::Publication,
            220,
        ),
    ] {
        assert_eq!(
            keys.verify(&grant.grant, account, session, hash, purpose, now),
            Err(Error::Forbidden)
        );
    }
    let mut corrupted = source.clone();
    *corrupted.last_mut().unwrap() ^= 1;
    assert!(matches!(
        keys.authorize(&corrupted, owner, "session", Purpose::Publication, 100),
        Err(Error::InvalidPackage)
    ));
    assert!(matches!(
        keys.authorize(&source[..48], owner, "session", Purpose::Publication, 100),
        Err(Error::InvalidPackage)
    ));
    assert!(matches!(
        keys.authorize(&source, owner, "session", Purpose::Consumption, 100),
        Err(Error::Forbidden)
    ));
    let consumption = keys
        .consume_authorized(&source, owner, "session", 100)
        .unwrap();
    assert!(keys
        .verify(
            &consumption.grant,
            owner,
            "session",
            &content,
            Purpose::Consumption,
            101
        )
        .is_ok());
    assert_eq!(
        keys.verify(
            &consumption.grant,
            owner,
            "session",
            &content,
            Purpose::Publication,
            101
        ),
        Err(Error::Forbidden)
    );
    assert_eq!(
        keys.verify(
            &consumption.grant,
            owner,
            "retired",
            &content,
            Purpose::Consumption,
            101
        ),
        Err(Error::Forbidden)
    );
    assert!(matches!(
        keys.consume_authorized(&corrupted, owner, "session", 100),
        Err(Error::InvalidPackage)
    ));
    assert!(keys.path.starts_with(directory.path()));
    assert!(
        Keys::beside_executable(&executable)
            .verify(
                &grant.grant,
                owner,
                "session",
                &content,
                Purpose::Publication,
                101
            )
            .is_err(),
        "restart retires grants without changing the manually supplied key"
    );
}

#[test]
fn psf_supplied_skin_packages_authenticate_when_reference_is_provided() {
    let Some(reference) = std::env::var_os("OMNIMORPH_PSF_REFERENCE") else {
        return;
    };
    let reference = std::path::PathBuf::from(reference);
    let mut master = std::fs::read(reference.join("application.bin")).unwrap();
    assert_eq!(master.len(), 32);
    let mut count = 0;
    for entry in std::fs::read_dir(&reference).unwrap() {
        let file = entry.unwrap().path();
        let name = file.file_name().unwrap().to_string_lossy().to_lowercase();
        if !name.ends_with(".psf") || !name.contains("skin") {
            continue;
        }
        let source = std::fs::read(&file).unwrap();
        let mut derived = authenticate(&source, &master).expect("Supplied skin must authenticate");
        derived.fill(0);
        count += 1;
        println!(
            "PASS Rust PSF authentication {}",
            file.file_name().unwrap().to_string_lossy()
        );
    }
    master.fill(0);
    assert_eq!(count, 8);
}
