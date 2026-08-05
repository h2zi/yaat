//! Signed Tauri updater orchestration and cancellable download state.

use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use tauri::{AppHandle, ipc::Channel};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::Notify;
use yaat_contracts::{ReleaseUpdate, UpdatePhase, UpdateProgress};

use crate::error::{AppError, AppResult};

#[derive(Default)]
pub struct UpdateState {
    pending: Mutex<Option<Update>>,
    cancellation: Mutex<Option<Arc<UpdateCancellation>>>,
}

#[derive(Default)]
struct UpdateCancellation {
    cancelled: AtomicBool,
    notify: Notify,
}

impl UpdateCancellation {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    async fn cancelled(&self) {
        let notified = self.notify.notified();
        if self.cancelled.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

pub async fn check(app: &AppHandle, state: &UpdateState) -> AppResult<Option<ReleaseUpdate>> {
    let update = app
        .updater_builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(update_error)?
        .check()
        .await
        .map_err(update_error)?;
    let metadata = update.as_ref().map(|release| ReleaseUpdate {
        current_version: release.current_version.clone(),
        latest_version: release.version.clone(),
    });
    *lock(&state.pending)? = update;
    Ok(metadata)
}

pub async fn install(
    app: &AppHandle,
    state: &UpdateState,
    on_progress: Channel<UpdateProgress>,
) -> AppResult<()> {
    let update = lock(&state.pending)?
        .take()
        .ok_or_else(|| AppError::NotFound("pending application update".into()))?;
    let cancellation = Arc::new(UpdateCancellation::default());
    *lock(&state.cancellation)? = Some(Arc::clone(&cancellation));

    let progress = on_progress.clone();
    let mut downloaded = 0_u64;
    let download = update.download(
        move |chunk_length, content_length| {
            downloaded = downloaded.saturating_add(chunk_length as u64);
            let _ = progress.send(UpdateProgress {
                phase: UpdatePhase::Downloading,
                downloaded,
                total: content_length,
            });
        },
        || {},
    );
    let result = tokio::select! {
        result = download => result.map_err(update_error),
        () = cancellation.cancelled() => Err(AppError::Cancelled),
    };
    *lock(&state.cancellation)? = None;
    let bytes = result?;

    let _ = on_progress.send(UpdateProgress {
        phase: UpdatePhase::Installing,
        downloaded: 0,
        total: None,
    });
    update.install(bytes).map_err(update_error)?;
    app.restart();
}

pub fn cancel(state: &UpdateState) -> AppResult<()> {
    if let Some(cancellation) = lock(&state.cancellation)?.as_ref() {
        cancellation.cancel();
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> AppResult<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| AppError::Internal("application updater state is poisoned".into()))
}

fn update_error(error: impl std::fmt::Display) -> AppError {
    AppError::UpdateUnavailable(error.to_string())
}
