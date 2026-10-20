//! "For You": recommendations built from the library's taste profile, and
//! the auto-generated Discover Mix playlist.

use std::time::Duration;

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::error::{ApiError, ApiResult};
use crate::models::{now_ms, Recommendation, WorkerRecommendation};

pub const DISCOVER_MIX_NAME: &str = "Discover Mix";
const POOL_SIZE: usize = 50;
const MIX_SIZE: usize = 30;
const SEEDS_PER_KIND: usize = 5;
pub const LAST_REFRESH_KEY: &str = "recommendations_refreshed_at";

#[derive(Deserialize)]
pub struct ListQuery {
    limit: Option<usize>,
}

pub async fn list(State(s): State<AppState>, Query(p): Query<ListQuery>) -> ApiResult<Json<Vec<Recommendation>>> {
    let limit = p.limit.unwrap_or(30).clamp(1, 100);
    let recs = s.db.recommendations(limit)?;
    if recs.is_empty() && s.db.get_meta(LAST_REFRESH_KEY)?.is_none() && !s.db.library_ids()?.is_empty() {
        // First visit with a library but no recommendations yet: build them now.
        rebuild(&s).await?;
        return Ok(Json(s.db.recommendations(limit)?));
    }
    Ok(Json(recs))
}

pub async fn refresh(State(s): State<AppState>) -> ApiResult<Json<Vec<Recommendation>>> {
    rebuild(&s).await?;
    Ok(Json(s.db.recommendations(POOL_SIZE)?))
}

/// Recompute recommendations and regenerate the Discover Mix.
pub async fn rebuild(s: &AppState) -> ApiResult<usize> {
    let _guard = s.recs_lock.lock().await;
    let seeds = s.db.taste_seeds(SEEDS_PER_KIND)?;
    if seeds.is_empty() {
        return Err(ApiError::bad_request("your library is empty — download a few songs first so the app can learn your taste"));
    }
    let exclude = s.db.library_ids()?;
    let data = s
        .music
        .request(
            json!({"cmd": "recommend", "seed_video_ids": seeds, "exclude_video_ids": exclude, "limit": POOL_SIZE}),
            Duration::from_secs(180),
        )
        .await?;
    let raw: Vec<WorkerRecommendation> = serde_json::from_value(data.get("tracks").cloned().unwrap_or(data))
        .map_err(|e| ApiError::internal(format!("bad recommendations from provider: {e}")))?;

    let recs: Vec<Recommendation> = raw
        .into_iter()
        .map(|r| Recommendation { track: r.track, score: r.score, because_video_id: r.because_video_id, because_artist: r.because_artist })
        .collect();
    for r in &recs {
        s.db.upsert_track_meta(&r.track)?;
    }
    s.db.replace_recommendations(&recs)?;

    if !crate::settings::load(&s.db).discover_mix {
        s.db.set_meta(LAST_REFRESH_KEY, &now_ms().to_string())?;
        tracing::info!("recommendations rebuilt from {} seeds: {} songs (Discover Mix off)", seeds.len(), recs.len());
        return Ok(recs.len());
    }
    let mix_id = match s.db.find_playlist_by_kind("discover")? {
        Some(id) => id,
        None => s.db.create_playlist(DISCOVER_MIX_NAME, "discover", None)?,
    };
    let mix: Vec<String> = recs.iter().take(MIX_SIZE).map(|r| r.track.video_id.clone()).collect();
    s.db.set_playlist_tracks(mix_id, &mix)?;
    s.db.set_meta(LAST_REFRESH_KEY, &now_ms().to_string())?;
    tracing::info!("recommendations rebuilt from {} seeds: {} songs", seeds.len(), recs.len());
    Ok(recs.len())
}

/// Background task: rebuild once a day (checked hourly).
pub async fn daily_refresh(s: AppState) {
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    // Let startup settle before the first check.
    tokio::time::sleep(Duration::from_secs(20)).await;
    let mut tick = tokio::time::interval(Duration::from_secs(60 * 60));
    loop {
        tick.tick().await;
        let last = s.db.get_meta(LAST_REFRESH_KEY).ok().flatten().and_then(|v| v.parse::<i64>().ok());
        let due = last.map(|t| now_ms() - t >= DAY_MS).unwrap_or(true);
        let has_library = s.db.library_ids().map(|ids| !ids.is_empty()).unwrap_or(false);
        let enabled = crate::settings::load(&s.db).daily_mix;
        if due && has_library && enabled {
            if let Err(e) = rebuild(&s).await {
                tracing::warn!("daily recommendation refresh failed: {e}");
            }
        }
    }
}
