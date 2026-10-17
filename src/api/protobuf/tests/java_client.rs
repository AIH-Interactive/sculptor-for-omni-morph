//! Optional cross-language gate: executes the production Java adapters against this listener.
use crate::{auth::Userinfo, AppState};
use std::{path::Path, process::Stdio, time::Duration};
use uuid::Uuid;

pub(crate) async fn integration(base: &str, state: &AppState, figura: &[u8]) {
    if std::env::var_os("OMNIMORPH_CLIENT_CLASSPATH_FILE").is_none() {
        return;
    }
    let fixtures = tempfile::tempdir().unwrap();
    std::fs::write(fixtures.path().join("figura.bin"), figura).unwrap();
    for (name, encrypted) in [("plain", false), ("encrypted", true)] {
        let (bytes, hash) = crate::service::package::tests::fixture(encrypted, false);
        std::fs::write(fixtures.path().join(format!("{name}.bin")), bytes).unwrap();
        std::fs::write(
            fixtures.path().join(format!("{name}.sha256")),
            faster_hex::hex_string(&hash),
        )
        .unwrap();
    }
    let owner = Uuid::parse_str("10000000-0000-4000-8000-000000001301").unwrap();
    let observer = Uuid::parse_str("10000000-0000-4000-8000-000000001302").unwrap();
    for (uuid, token) in [(owner, "java-source"), (observer, "java-observer")] {
        state
            .user_manager
            .insert(
                uuid,
                token.into(),
                Userinfo {
                    uuid,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    *state.figura_versions.write().await = Some(crate::FiguraVersions {
        release: "0.1.5".into(),
        prerelease: "0.1.5".into(),
    });
    let arguments = [
        "top.aihmc.omnimorph.backend2.protocol.CloudAdapterIntegrationTest".to_string(),
        base.to_string(),
        fixtures.path().to_string_lossy().into_owned(),
        owner.to_string(),
        observer.to_string(),
    ];
    execute(&arguments, fixtures.path()).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.session.count(owner) != 0 || state.session.count(observer) != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Java adapter sockets must clean up their sessions");
}

pub(crate) async fn execute(arguments: &[String], directory: &Path) {
    let classpath_file =
        std::env::var("OMNIMORPH_CLIENT_CLASSPATH_FILE").expect("Java classpath gate enabled");
    let classpath =
        std::fs::read_to_string(classpath_file).expect("Java integration classpath file");
    let mut complete = vec!["-cp".to_string(), classpath.trim().to_string()];
    complete.extend_from_slice(arguments);
    let argument_file = directory.join("java.args");
    let quoted = complete
        .iter()
        .map(|argument| {
            format!(
                "\"{}\"\n",
                argument.replace('\\', "\\\\").replace('"', "\\\"")
            )
        })
        .collect::<String>();
    std::fs::write(&argument_file, quoted).unwrap();
    let java_home = std::env::var("JAVA_HOME").expect("JDK 25 JAVA_HOME for client gate");
    let java =
        Path::new(&java_home)
            .join("bin")
            .join(if cfg!(windows) { "java.exe" } else { "java" });
    let status = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(java)
            .arg(format!("@{}", argument_file.display()))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .expect("production Java adapter gate timed out")
    .expect("production Java adapter process");
    assert!(
        status.success(),
        "production Java adapter integration failed"
    );
}
