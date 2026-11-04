//! Loads the engine makes in the background, such as a queue's queries. Their yt-dlp runs take
//! background turns (`ENTROXIMER_YTDLP_BACKGROUND_RUNS`), so that they never hold up playback or
//! clients' requests, until the load is promoted, e.g. once its item is what plays next: from then
//! on its runs take normal turns, like the playback's own.

use std::future::Future;

use tokio::sync::{Semaphore, SemaphorePermit};
use tokio_util::sync::CancellationToken;

tokio::task_local! {
    static BACKGROUND: Promotion;
}

/// Lets a background load take normal turns from now on. Clones share it.
#[derive(Clone, Debug, Default)]
pub struct Promotion(CancellationToken);

impl Promotion {
    pub fn promote(&self) {
        self.0.cancel();
    }

    pub fn is_promoted(&self) -> bool {
        self.0.is_cancelled()
    }
}

/// Runs `load` as a background load, until `promotion` promotes it. The load must not spawn
/// tasks for its yt-dlp runs, which would run as normal ones.
pub async fn run<F: Future>(promotion: Promotion, load: F) -> F::Output {
    BACKGROUND.scope(promotion, load).await
}

/// A turn for one yt-dlp run: from `normal`, or, for a background load not promoted yet, from
/// `background` (or `normal` once it is promoted while waiting).
pub(crate) async fn turn<'a>(
    normal: &'a Semaphore,
    background: &'a Semaphore,
) -> SemaphorePermit<'a> {
    let waiting = BACKGROUND
        .try_with(Promotion::clone)
        .ok()
        .filter(|promotion| !promotion.is_promoted());
    let permit = match waiting {
        Some(promotion) => tokio::select! {
            permit = background.acquire() => permit,
            () = promotion.0.cancelled() => normal.acquire().await,
        },
        None => normal.acquire().await,
    };
    permit.expect("the semaphores are never closed")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn promoted_loads_switch_to_normal_turns() {
        let normal = Arc::new(Semaphore::new(1));
        let background = Arc::new(Semaphore::new(0));
        let promotion = Promotion::default();
        let waiting = tokio::spawn({
            let (normal, background) = (Arc::clone(&normal), Arc::clone(&background));
            run(promotion.clone(), async move {
                let _turn = turn(&normal, &background).await;
            })
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "no background turn is free");
        promotion.promote();
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("a promoted load takes a normal turn")
            .unwrap();

        // outside a background load, turns are normal ones
        let _turn = turn(&normal, &background).await;
    }

    #[tokio::test]
    async fn background_loads_take_background_turns() {
        let normal = Semaphore::new(0);
        let background = Semaphore::new(1);
        run(Promotion::default(), async {
            let _turn = turn(&normal, &background).await;
            assert_eq!(background.available_permits(), 0);
        })
        .await;
    }
}
