//! Route packs: everything a route's 3D view needs, kept on the rider's disk
//! so the route rides without a network.
//!
//! A pack is not a file of its own. It is the route's elevation tiles in the
//! shared terrain cache and its map answer in the map-data cache — the same
//! files the view reads, so a prepared route and a route already opened in 3D
//! are the same thing. See `docs/3d-course-blueprint.md`, section 6.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::data::osm::{self, Cached};
use crate::data::route::Route;
use crate::data::terrain;

/// Whether a route can be ridden in 3D offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackState {
    /// Terrain and map data are both on disk. The map answer may be from an
    /// older version of the query — still ridden, refreshed when online.
    Ready,
    /// Some of it is on disk, not all.
    Partial,
    Missing,
    /// The route is larger than any pack the app will fetch for it.
    TooLarge,
}

/// Where `route`'s pack stands, from the caches under `terrain_root` and
/// `osm_root`. Only file metadata is read.
pub fn state(route: &Route, terrain_root: &Path, osm_root: &Path) -> PackState {
    let Ok(tiles) = terrain::tiles_for_route(route, terrain::CORRIDOR_M) else {
        return PackState::TooLarge;
    };
    if osm::route_key(route).is_err() {
        return PackState::TooLarge;
    }
    let tiles_held = tiles
        .iter()
        .filter(|id| std::fs::metadata(id.cache_path(terrain_root)).is_ok())
        .count();
    let map = osm::cached_state(osm_root, route);
    let terrain_done = tiles_held == tiles.len();
    let map_done = map != Cached::Missing;
    match (terrain_done, map_done) {
        (true, true) => PackState::Ready,
        _ if tiles_held > 0 || map_done => PackState::Partial,
        _ => PackState::Missing,
    }
}

/// [`state`] against the app's own caches.
pub fn state_here(route: &Route) -> PackState {
    state(route, &terrain::cache_dir(), &osm::cache_dir())
}

/// After an automatic download fails, the next automatic try for that route
/// waits this long. The public servers fail by being overloaded; asking again
/// at every launch and every redraw of the library only adds to the load. The
/// rider's own Retry is never held back.
pub const AUTO_RETRY_AFTER: Duration = Duration::from_secs(30 * 60);

/// One preparation per route at a time: a second caller — the library redrawn
/// while a route downloads — waits for the first, then finds its pack ready.
fn route_lock(key: u64) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: Mutex<Option<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>> = Mutex::new(None);
    let mut locks = LOCKS.lock().unwrap_or_else(|p| p.into_inner());
    locks
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// When each route's last automatic download failed.
static FAILED_AT: Mutex<Option<HashMap<u64, Instant>>> = Mutex::new(None);

/// Whether an automatic download of `route` may be tried now — not within
/// [`AUTO_RETRY_AFTER`] of one that failed.
pub fn auto_fetch_allowed(route: &Route) -> bool {
    let Ok(key) = osm::route_key(route) else {
        return false;
    };
    let failed = FAILED_AT.lock().unwrap_or_else(|p| p.into_inner());
    failed
        .as_ref()
        .and_then(|m| m.get(&key))
        .is_none_or(|at| at.elapsed() >= AUTO_RETRY_AFTER)
}

/// Download whatever of `route`'s pack is not on disk: terrain first (fast,
/// and enough to ride on), then map data, queued app-wide one route at a time
/// (see `osm::load_queued`). A failure holds back automatic tries for a while.
pub async fn prepare(route: Route) -> Result<()> {
    let key = osm::route_key(&route)?;
    let lock = route_lock(key);
    let _this_route = lock.lock().await;
    let result = async {
        let tiles = terrain::tiles_for_route(&route, terrain::CORRIDOR_M)?;
        terrain::load(terrain::cache_dir(), tiles).await?;
        osm::load_queued(osm::cache_dir(), route).await?;
        anyhow::Ok(())
    }
    .await;
    record(key, result.is_ok());
    result
}

/// Note how a download of the route `key` went: a failure holds back
/// automatic tries, a success clears that.
fn record(key: u64, ok: bool) {
    let mut failed = FAILED_AT.lock().unwrap_or_else(|p| p.into_inner());
    let failed = failed.get_or_insert_with(HashMap::new);
    if ok {
        failed.remove(&key);
    } else {
        failed.insert(key, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::course3d::LocalFrame;
    use crate::data::route::RoutePoint;

    fn route(points: usize) -> Route {
        let frame = LocalFrame::at(41.0, 29.0);
        let pts: Vec<RoutePoint> = (0..points)
            .map(|i| {
                let (lat, lng) = frame.to_geo(0.0, i as f64 * 8.0);
                RoutePoint {
                    lat,
                    lng,
                    elevation_m: 0.0,
                    distance_m: i as f32 * 8.0,
                    gradient: 0.0,
                }
            })
            .collect();
        Route {
            name: "Pack".into(),
            total_distance_m: pts.last().map(|p| p.distance_m).unwrap_or(0.0),
            total_gain_m: 0.0,
            points: pts,
        }
    }

    fn roots(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("cycle-pack-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        (base.join("terrain"), base.join("osm"))
    }

    /// A PNG-looking file at every tile the route needs.
    fn hold_all_tiles(route: &Route, root: &Path) {
        for id in terrain::tiles_for_route(route, terrain::CORRIDOR_M).expect("tiles") {
            let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
            png.resize(64, 0);
            terrain::store(root, id, &png).expect("stored");
        }
    }

    fn hold_map(route: &Route, root: &Path, query: &str) {
        // Through the public path: an answer for the route, as a download leaves it.
        let key = osm::route_key(route).expect("a key");
        std::fs::create_dir_all(root).expect("dir");
        std::fs::write(root.join(format!("route-{key:016x}.json")), b"{}").expect("data");
        std::fs::write(
            root.join(format!("route-{key:016x}.version")),
            format!("{:016x}", osm::stable_hash(query)),
        )
        .expect("version");
    }

    #[test]
    fn should_have_no_pack_for_a_new_route() {
        let (t, o) = roots("new");
        assert_eq!(state(&route(500), &t, &o), PackState::Missing);
    }

    #[test]
    fn should_be_partial_with_terrain_but_no_map() {
        let (t, o) = roots("partial");
        let r = route(500);
        hold_all_tiles(&r, &t);
        assert_eq!(state(&r, &t, &o), PackState::Partial);
    }

    #[test]
    fn should_be_ready_with_both() {
        let (t, o) = roots("ready");
        let r = route(500);
        hold_all_tiles(&r, &t);
        hold_map(&r, &o, &osm::query(&r).expect("q"));
        assert_eq!(state(&r, &t, &o), PackState::Ready);
    }

    #[test]
    fn should_stay_ready_when_an_update_changes_the_query() {
        // Offline after an app update, the older answer is what the rider has.
        let (t, o) = roots("outdated");
        let r = route(500);
        hold_all_tiles(&r, &t);
        hold_map(&r, &o, "the query an older version asked");
        assert_eq!(state(&r, &t, &o), PackState::Ready);
    }

    #[test]
    fn should_not_be_ready_with_a_tile_missing() {
        let (t, o) = roots("tile");
        let r = route(500);
        hold_all_tiles(&r, &t);
        hold_map(&r, &o, &osm::query(&r).expect("q"));
        let first = terrain::tiles_for_route(&r, terrain::CORRIDOR_M).expect("tiles")[0];
        std::fs::remove_file(first.cache_path(&t)).expect("removed");
        assert_eq!(state(&r, &t, &o), PackState::Partial);
    }

    #[test]
    fn should_call_an_impossible_route_too_large() {
        // Istanbul to Paris in one line: no pack for that.
        let pts: Vec<RoutePoint> = (0..=400)
            .map(|i| {
                let t = i as f64 / 400.0;
                RoutePoint {
                    lat: 41.0 + t * 7.85,
                    lng: 29.0 - t * 26.65,
                    elevation_m: 0.0,
                    distance_m: i as f32 * 5_000.0,
                    gradient: 0.0,
                }
            })
            .collect();
        let r = Route {
            name: "Far".into(),
            total_distance_m: 2_000_000.0,
            total_gain_m: 0.0,
            points: pts,
        };
        let (t, o) = roots("far");
        assert_eq!(state(&r, &t, &o), PackState::TooLarge);
    }

    #[test]
    fn should_hold_back_automatic_tries_after_a_failure_until_a_success() {
        // A route of its own, so no other test's record interferes.
        let r = route(321);
        let key = osm::route_key(&r).expect("a key");
        assert!(auto_fetch_allowed(&r));
        record(key, false);
        assert!(
            !auto_fetch_allowed(&r),
            "tried again straight after failing"
        );
        record(key, true);
        assert!(auto_fetch_allowed(&r));
    }
}
