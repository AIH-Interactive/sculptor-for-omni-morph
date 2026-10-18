use super::*;

#[test]
fn preview_cache_duration_defaults_and_explicit_values_are_preserved() {
    let defaults: LibraryConfig = toml::from_str("").unwrap();
    assert_eq!(defaults.preview_cache_seconds, 3600);
    let custom: LibraryConfig = toml::from_str("preview_cache_seconds = 7200").unwrap();
    assert_eq!(custom.preview_cache_seconds, 7200);
    let disabled: LibraryConfig = toml::from_str("preview_cache_seconds = 0").unwrap();
    assert_eq!(disabled.preview_cache_seconds, 0);
    assert!(toml::from_str::<LibraryConfig>("preview_cache_seconds = -1").is_err());
}

// Synthetic test-only password/salt; independently derived with Python hashlib.
const KEY: &str = "055bcd61a2939b3846e10d4f9ebdf02be6610f5f894fd7882e985b9f87b81cff";

fn admin() -> AdminConfig {
    AdminConfig {
        player_name: "ExampleAdmin".into(),
        uuid: Uuid::from_u128(4),
        password_hash: format!("pbkdf2-sha256$v1$600000$000102030405060708090a0b0c0d0e0f${KEY}"),
    }
}

#[test]
fn unicode_administrator_config_accepts_simple_uuid() {
    for name in [
        "ExampleAdmin",
        "测",
        "测试管理员AE3803",
        "中文、名字",
        "e\u{0301}",
        "name\u{200b}",
        "\u{0080}",
        "\u{d7ff}",
    ] {
        for uuid in [admin().uuid.to_string(), admin().uuid.simple().to_string()] {
            let mut candidate = admin();
            candidate.player_name = name.into();
            let parsed_uuid: Uuid = uuid.parse().unwrap();
            assert_eq!(parsed_uuid, candidate.uuid);
            let result = LibraryConfig::try_from(RawLibraryConfig {
                admins: vec![candidate],
                ..Default::default()
            });
            assert!(result.is_ok(), "rejected name {name} with UUID {uuid}");
        }
    }
    let configured = format!(
        "{}\n[[model_library.admins]]\nplayer_name = \"测试管理员AE3803\"\nuuid = \"{}\"\npassword_hash = \"{}\"\n",
        include_str!("../../../Config.example.toml"),
        admin().uuid.simple(),
        admin().password_hash
    );
    let config: crate::state::Config = toml::from_str(&configured).unwrap();
    assert_eq!(
        config.model_library.admins[0].player_name,
        "测试管理员AE3803"
    );
    assert_eq!(config.model_library.admins[0].uuid, admin().uuid);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("Config.toml");
    std::fs::write(&path, configured).unwrap();
    assert_eq!(crate::state::Config::parse(path), config);
}

#[tokio::test]
async fn password_vector_and_wrong_identity_password_and_budgets() {
    let admin = admin();
    admin
        .verify_password(admin.uuid, "exampleadmin", "synthetic-test-password".into())
        .await
        .unwrap();
    assert_eq!(
        admin
            .verify_password(admin.uuid, "ExampleAdmin", "wrong".into())
            .await,
        Err(ErrorCode::Forbidden)
    );
    assert_eq!(
        admin
            .verify_password(
                Uuid::from_u128(5),
                "ExampleAdmin",
                "synthetic-test-password".into()
            )
            .await,
        Err(ErrorCode::Forbidden)
    );
    assert_eq!(
        admin
            .verify_password(admin.uuid, "Other", "synthetic-test-password".into())
            .await,
        Err(ErrorCode::Forbidden)
    );
    assert_eq!(
        admin
            .verify_password(admin.uuid, "ExampleAdmin", "x".repeat(1025))
            .await,
        Err(ErrorCode::Forbidden)
    );
    assert_eq!(
        admin
            .verify_password(admin.uuid, "ExampleAdmin", String::new())
            .await,
        Err(ErrorCode::Forbidden)
    );
    let display = format!("{admin:?}");
    assert!(!display.contains(KEY));
    assert!(!display.contains("pbkdf2"));

    let mut unicode = admin.clone();
    unicode.player_name = "测试管理员AE3803".into();
    unicode
        .verify_password(
            unicode.uuid,
            "测试管理员ae3803",
            "synthetic-test-password".into(),
        )
        .await
        .unwrap();
    for (uuid, name) in [
        (Uuid::from_u128(5), "测试管理员AE3803"),
        (unicode.uuid, "其他管理员AE3803"),
    ] {
        assert_eq!(
            unicode
                .verify_password(uuid, name, "synthetic-test-password".into())
                .await,
            Err(ErrorCode::Forbidden)
        );
    }
}

#[test]
fn administrator_names_enforce_character_budget_and_configured_range() {
    for name in [
        "",
        " name",
        "name ",
        "two names",
        "name\n",
        "name\0",
        "name\u{007f}",
        "name\u{e000}",
        "name\u{1f600}",
        "name-name",
        "name.name",
        "名字名字名字名字名字名字名字名字名",
    ] {
        let mut candidate = admin();
        candidate.player_name = name.into();
        assert!(
            LibraryConfig::try_from(RawLibraryConfig {
                admins: vec![candidate],
                ..Default::default()
            })
            .is_err(),
            "accepted invalid name {name:?}"
        );
    }
    let mut candidate = admin();
    candidate.player_name = "名".repeat(16);
    assert!(LibraryConfig::try_from(RawLibraryConfig {
        admins: vec![candidate.clone()],
        ..Default::default()
    })
    .is_ok());
    let mut duplicate = candidate.clone();
    duplicate.uuid = Uuid::from_u128(5);
    assert!(LibraryConfig::try_from(RawLibraryConfig {
        admins: vec![candidate, duplicate],
        ..Default::default()
    })
    .is_err());
}

#[test]
fn invalid_parameters_and_duplicate_config_fail_closed() {
    for value in [
        "plain-sha256",
        "pbkdf2-sha256$v2$600000$000102030405060708090a0b0c0d0e0f$",
        "pbkdf2-sha256$v1$1$000102030405060708090a0b0c0d0e0f$",
        "pbkdf2-sha256$v1$2000001$000102030405060708090a0b0c0d0e0f$",
        "pbkdf2-sha256$v1$0600000$000102030405060708090a0b0c0d0e0f$",
    ] {
        assert!(PasswordHash::parse(value).is_err());
    }
    let mut invalid = admin();
    invalid.password_hash = invalid.password_hash.to_ascii_uppercase();
    assert!(PasswordHash::parse(&invalid.password_hash).is_err());
    assert!(
        LibraryConfig::try_from(RawLibraryConfig {
            admins: vec![admin(), admin()],
            ..Default::default()
        })
        .is_err()
    );
    let mut duplicate_name = admin();
    duplicate_name.uuid = Uuid::from_u128(5);
    duplicate_name.player_name = "exampleadmin".into();
    assert!(
        LibraryConfig::try_from(RawLibraryConfig {
            admins: vec![admin(), duplicate_name],
            ..Default::default()
        })
        .is_err()
    );
    let mut nil = admin();
    nil.uuid = Uuid::nil();
    assert!(
        LibraryConfig::try_from(RawLibraryConfig {
            admins: vec![nil],
            ..Default::default()
        })
        .is_err()
    );
    let config: crate::state::Config =
        toml::from_str(include_str!("../../../Config.example.toml")).unwrap();
    assert!(config.model_library.admins.is_empty());
    let configured = format!(
        "{}\n[[model_library.admins]]\nplayer_name = \"ExampleAdmin\"\nuuid = \"{}\"\npassword_hash = \"{}\"\n",
        include_str!("../../../Config.example.toml"),
        admin().uuid,
        admin().password_hash
    );
    let config: crate::state::Config = toml::from_str(&configured).unwrap();
    assert_eq!(config.model_library.admins, vec![admin()]);
}

#[test]
fn administrator_tokens_are_bound_rotated_and_replay_revokes_family() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path()).unwrap();
    let config = admin();
    let binding = AdminBinding {
        player: config.uuid,
        origin: "https://example.invalid",
        instance: "instance-1",
        config: &config,
    };
    let credentials = storage.library_admin_issue(&binding, true, 100).unwrap();
    storage
        .library_admin_authorize(&binding, &credentials.access_token, 101)
        .unwrap();
    assert_eq!(
        storage.library_admin_authorize(
            &binding,
            &credentials.access_token,
            credentials.expires_at
        ),
        Err(ErrorCode::Unauthenticated)
    );
    let foreign = AdminBinding {
        player: config.uuid,
        origin: "https://other.invalid",
        instance: "instance-1",
        config: &config,
    };
    assert_eq!(
        storage.library_admin_authorize(&foreign, &credentials.access_token, 101),
        Err(ErrorCode::Unauthenticated)
    );
    let restarted = AdminBinding {
        player: config.uuid,
        origin: binding.origin,
        instance: "instance-2",
        config: &config,
    };
    assert_eq!(
        storage.library_admin_authorize(&restarted, &credentials.access_token, 101),
        Err(ErrorCode::Unauthenticated)
    );
    let impostor = AdminBinding {
        player: Uuid::from_u128(5),
        origin: binding.origin,
        instance: binding.instance,
        config: &config,
    };
    assert_eq!(
        storage.library_admin_authorize(&impostor, &credentials.access_token, 101),
        Err(ErrorCode::Forbidden)
    );

    let refresh = credentials.refresh_token.as_deref().unwrap();
    let next = storage
        .library_admin_refresh(&binding, refresh, 102)
        .unwrap();
    storage
        .library_admin_authorize(&binding, &next.access_token, 103)
        .unwrap();
    assert_eq!(
        storage.library_admin_authorize(&binding, &credentials.access_token, 103),
        Err(ErrorCode::Unauthenticated)
    );
    assert!(matches!(
        storage.library_admin_refresh(&binding, refresh, 104),
        Err(ErrorCode::Unauthenticated)
    ));
    assert_eq!(
        storage.library_admin_authorize(&binding, &next.access_token, 105),
        Err(ErrorCode::Unauthenticated)
    );
    assert!(matches!(
        storage.library_admin_refresh(&binding, next.refresh_token.as_deref().unwrap(), 105),
        Err(ErrorCode::Unauthenticated)
    ));

    let credentials = storage.library_admin_issue(&binding, false, 200).unwrap();
    assert!(credentials.refresh_token.is_none());
    storage
        .library_admin_logout(&binding, &credentials.access_token)
        .unwrap();
    storage
        .library_admin_logout(&binding, &credentials.access_token)
        .unwrap();
    assert_eq!(
        storage.library_admin_authorize(&binding, &credentials.access_token, 201),
        Err(ErrorCode::Unauthenticated)
    );
}

#[test]
fn config_changes_and_refresh_expiry_reject_existing_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path()).unwrap();
    let config = admin();
    let binding = AdminBinding {
        player: config.uuid,
        origin: "https://example.invalid",
        instance: "instance-1",
        config: &config,
    };
    let credentials = storage.library_admin_issue(&binding, true, 100).unwrap();
    for change_name in [false, true] {
        let mut changed = config.clone();
        if change_name {
            changed.player_name = "Renamed".into();
        } else {
            changed.password_hash = changed.password_hash.replace("600000", "600001");
        }
        let changed_binding = AdminBinding {
            player: changed.uuid,
            origin: binding.origin,
            instance: binding.instance,
            config: &changed,
        };
        assert_eq!(
            storage.library_admin_authorize(&changed_binding, &credentials.access_token, 101),
            Err(ErrorCode::Unauthenticated)
        );
        assert!(matches!(
            storage.library_admin_refresh(
                &changed_binding,
                credentials.refresh_token.as_deref().unwrap(),
                101
            ),
            Err(ErrorCode::Unauthenticated)
        ));
    }
    assert!(matches!(
        storage.library_admin_refresh(
            &binding,
            credentials.refresh_token.as_deref().unwrap(),
            100 + REFRESH_SECONDS
        ),
        Err(ErrorCode::Unauthenticated)
    ));
    let raw_found: bool = storage.library_read(|database| {
        database.query_row("SELECT EXISTS(SELECT 1 FROM library_admin_sessions WHERE token_hash = ?1 OR refresh_hash = ?2)",
            params![credentials.access_token.as_bytes(), credentials.refresh_token.as_ref().unwrap().as_bytes()], |row| row.get(0))
            .map_err(|_| ErrorCode::Internal)
    }).unwrap();
    assert!(!raw_found);
}

#[test]
fn removing_or_changing_config_permanently_revokes_old_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path()).unwrap();
    let config = admin();
    let binding = AdminBinding {
        player: config.uuid,
        origin: "https://example.invalid",
        instance: "instance-1",
        config: &config,
    };
    for removed in [false, true] {
        let credentials = storage.library_admin_issue(&binding, true, 100).unwrap();
        let mut changed = config.clone();
        changed.player_name = "Renamed".into();
        let changed_config = LibraryConfig {
            admins: if removed { vec![] } else { vec![changed] },
            ..Default::default()
        };
        storage.library_admin_reconcile(&changed_config).unwrap();
        storage
            .library_admin_reconcile(&LibraryConfig {
                admins: vec![config.clone()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            storage.library_admin_authorize(&binding, &credentials.access_token, 101),
            Err(ErrorCode::Unauthenticated)
        );
        assert!(matches!(
            storage.library_admin_refresh(
                &binding,
                credentials.refresh_token.as_deref().unwrap(),
                101
            ),
            Err(ErrorCode::Unauthenticated)
        ));
    }
}
#[test]
fn builtin_categories_are_always_present_and_config_only_adds_extras() {
    let defaults: LibraryConfig = toml::from_str("").unwrap();
    assert!(defaults.categories.is_empty());
    assert_eq!(defaults.effective_categories(), ["full_model", "decorative_parts"]);
    let extra: LibraryConfig = toml::from_str("categories = ['custom']").unwrap();
    assert_eq!(extra.effective_categories(), ["full_model", "decorative_parts", "custom"]);
    assert!(toml::from_str::<LibraryConfig>("categories = ['full_model']").is_err());
    assert!(toml::from_str::<LibraryConfig>("categories = ['custom', 'custom']").is_err());
}
