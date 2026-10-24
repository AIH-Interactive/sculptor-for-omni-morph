//! Bounded resource bodies with current-session and source checks per chunk.
use super::*;
use axum::body::{Body, Bytes};
use std::{sync::LazyLock, time::Duration};
use tokio::io::AsyncReadExt;

static STREAMS: LazyLock<Mutex<HashMap<Uuid, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) struct DownloadPermit(Uuid);

impl DownloadPermit {
    pub(crate) fn acquire(player: Uuid) -> Result<Self, ErrorCode> {
        let mut streams = STREAMS.lock().map_err(|_| ErrorCode::Internal)?;
        if streams.values().sum::<usize>() >= 32 || streams.get(&player).copied().unwrap_or(0) >= 2
        {
            return Err(ErrorCode::LimitExceeded);
        }
        *streams.entry(player).or_default() += 1;
        Ok(Self(player))
    }
}

impl Drop for DownloadPermit {
    fn drop(&mut self) {
        if let Ok(mut streams) = STREAMS.lock() {
            if let Some(count) = streams.get_mut(&self.0) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    streams.remove(&self.0);
                }
            }
        }
    }
}

pub(crate) fn guarded_body<F>(
    state: AppState,
    file: fs::File,
    length: u64,
    resource: Option<AvatarDescriptor>,
    viewer: Uuid,
    authorize: F,
) -> Result<Body, ErrorCode>
where
    F: Fn(&AppState) -> Result<Uuid, ErrorCode> + Send + 'static,
{
    let permit = DownloadPermit::acquire(viewer)?;
    let reference = resource
        .map(|descriptor| {
            let owner = Uuid::parse_str(&descriptor.owner).map_err(|_| ErrorCode::Internal)?;
            Ok::<_, ErrorCode>((owner, descriptor.asset_id, descriptor.revision))
        })
        .transpose()?;
    let stream = futures_util::stream::try_unfold(
        (state, file, length, reference, authorize, permit),
        move |(state, mut file, remaining, reference, authorize, permit)| async move {
            if remaining == 0 {
                return Ok(None);
            }
            let denied = || {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Resource transfer is no longer authorized",
                )
            };
            if authorize(&state).map_err(|_| denied())? != viewer {
                return Err(denied());
            }
            if let Some((owner, asset, revision)) = reference.as_ref() {
                let storage = state.storage.clone();
                let owner = *owner;
                let asset = asset.clone();
                let revision = *revision;
                let limits = state.config.read().await.outfit.limits.clone();
                crate::service::library::work(move || {
                    storage.library_resource_access_with_outfit_limits(
                        owner,
                        &asset,
                        revision,
                        crate::service::library_policy::Actor {
                            uuid: viewer,
                            admin_context: false,
                            granted: false,
                        },
                        &limits,
                    )
                })
                .await
                .map_err(|_| denied())?;
            }
            let mut bytes = vec![0; remaining.min(32 * 1024) as usize];
            let read = tokio::time::timeout(Duration::from_secs(30), file.read(&mut bytes))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "Resource read timed out")
                })??;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Incomplete resource",
                ));
            }
            bytes.truncate(read);
            Ok(Some((
                Bytes::from(bytes),
                (
                    state,
                    file,
                    remaining - read as u64,
                    reference,
                    authorize,
                    permit,
                ),
            )))
        },
    );
    Ok(Body::from_stream(stream))
}
