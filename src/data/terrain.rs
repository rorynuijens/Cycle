//! Elevation for the 3D course: the land either side of the road.
//!
//! Heights come from the Terrain Tiles open dataset on AWS (Mapzen's
//! "Terrarium" encoding): 256 px PNG tiles in the usual slippy-map grid, each
//! pixel's height packed into its red, green and blue. No key, no account.
//! Zoom 13 is about 19 m a pixel at the equator, 14 at Istanbul — finer than
//! the ground grid it feeds.
//!
//! Tiles are fetched once and kept under `<data>/terrain/`, shared between
//! routes: two rides out of the same town need most of the same land.
//!
//! The PNG container is opened by the caller (GTK's image loader, in the UI
//! layer); this module starts from raw RGB and owns everything after —
//! decoding, validation and sampling — so all of it is testable here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::data::route::Route;

/// Zoom level every tile is fetched at.
pub const ZOOM: u8 = 13;

/// Width and height of a tile, in pixels.
pub const TILE_PX: u32 = 256;

/// How far either side of the route the land is fetched, in metres. Must
/// reach past the edge of the ground the view draws — see
/// `course3d::GROUND_RADIUS_M`.
pub const CORRIDOR_M: f64 = 1_300.0;

/// Most tiles one route may ask for. A ride across a country at ±1.3 km is
/// well under this; anything over it is a GPX that is not a ride.
pub const MAX_TILES: usize = 400;

/// Largest tile accepted, from the network or the cache. Real tiles are about
/// 100 KB; nothing legitimate comes near this.
pub const MAX_TILE_BYTES: u64 = 1_048_576;

/// Heights outside this range are not on Earth: a tile that decodes to one is
/// corrupt, not unusual. The Mariana Trench and Everest, with room to spare.
const LOWEST_M: f32 = -11_500.0;
const HIGHEST_M: f32 = 9_000.0;

const TILE_URL: &str = "https://s3.amazonaws.com/elevation-tiles-prod/terrarium";

/// How long one tile may take to arrive.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Tiles fetched at once. Enough to fill a route in seconds, few enough to be
/// a polite client of an open dataset.
const FETCH_CONCURRENCY: usize = 4;

/// The eight bytes every PNG starts with.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// One tile of the slippy-map grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileId {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileId {
    pub fn url(&self) -> String {
        format!("{TILE_URL}/{}/{}/{}.png", self.z, self.x, self.y)
    }

    /// Where this tile is kept under `root`. Built from numbers alone, so no
    /// tile can name a path outside the cache.
    pub fn cache_path(&self, root: &Path) -> PathBuf {
        root.join(self.z.to_string())
            .join(self.x.to_string())
            .join(format!("{}.png", self.y))
    }
}

/// The terrain cache directory.
pub fn cache_dir() -> PathBuf {
    crate::data::paths::data_dir().join("terrain")
}

/// Position in whole-world pixels at `z`: x east from 180° W, y south from
/// the top of the Web Mercator square.
fn world_px(lat: f64, lng: f64, z: u8) -> (f64, f64) {
    let size = (TILE_PX as f64) * f64::from(1u32 << z);
    // Web Mercator stops at ±85.05°; nobody rides past it.
    let lat = lat.clamp(-85.05, 85.05).to_radians();
    let x = (lng + 180.0) / 360.0 * size;
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * size;
    (x, y)
}

/// The tile holding `lat`, `lng` at zoom `z`.
pub fn tile_at(lat: f64, lng: f64, z: u8) -> TileId {
    let (x, y) = world_px(lat, lng, z);
    let max = (1u32 << z) - 1;
    let tile = |p: f64| ((p / TILE_PX as f64).floor().max(0.0) as u32).min(max);
    TileId {
        z,
        x: tile(x),
        y: tile(y),
    }
}

/// The route asked for more land than any ride needs.
#[derive(Debug, thiserror::Error, PartialEq)]
#[error("route needs {0} terrain tiles, more than the {MAX_TILES} allowed")]
pub struct TooManyTiles(pub usize);

/// Every tile within `corridor_m` of any point of `route`, sorted.
///
/// Each point marks out the square `corridor_m` around it; the tiles under
/// the four corners and the point itself cover that square whenever it is
/// smaller than a tile, which at ±1.3 km and zoom 13 it always is.
pub fn tiles_for_route(route: &Route, corridor_m: f64) -> Result<Vec<TileId>, TooManyTiles> {
    let mut tiles = std::collections::BTreeSet::new();
    for p in &route.points {
        let frame = crate::data::course3d::LocalFrame::at(p.lat, p.lng);
        for (de, dn) in [
            (0.0, 0.0),
            (-corridor_m, -corridor_m),
            (corridor_m, -corridor_m),
            (-corridor_m, corridor_m),
            (corridor_m, corridor_m),
            (0.0, -corridor_m),
            (0.0, corridor_m),
            (-corridor_m, 0.0),
            (corridor_m, 0.0),
        ] {
            let (lat, lng) = frame.to_geo(de, dn);
            tiles.insert(tile_at(lat, lng, ZOOM));
        }
        if tiles.len() > MAX_TILES {
            return Err(TooManyTiles(tiles.len()));
        }
    }
    Ok(tiles.into_iter().collect())
}

/// Height in metres of one Terrarium pixel.
pub fn terrarium_height(r: u8, g: u8, b: u8) -> f32 {
    (f32::from(r) * 256.0 + f32::from(g) + f32::from(b) / 256.0) - 32_768.0
}

/// A tile that cannot be used.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum TileError {
    #[error("tile is {0}×{1} px, not {TILE_PX}×{TILE_PX}")]
    WrongSize(u32, u32),
    #[error("tile pixel data is shorter than its size claims")]
    Truncated,
    #[error("tile decodes to {0} m, which is not a height on Earth")]
    Implausible(f32),
}

/// Heights of a tile from its RGB pixels, row by row.
///
/// `stride` is the byte length of one row, which may be padded past
/// `width × 3`. A tile of the wrong size, too few bytes, or any height that is
/// not on Earth is refused whole: a half-corrupt tile would draw a cliff.
pub fn decode_terrarium(
    rgb: &[u8],
    width: u32,
    height: u32,
    stride: usize,
) -> Result<Vec<f32>, TileError> {
    if width != TILE_PX || height != TILE_PX {
        return Err(TileError::WrongSize(width, height));
    }
    let row = TILE_PX as usize * 3;
    if stride < row {
        return Err(TileError::Truncated);
    }
    let needed = stride * (TILE_PX as usize - 1) + row;
    if rgb.len() < needed {
        return Err(TileError::Truncated);
    }
    let mut out = Vec::with_capacity((TILE_PX * TILE_PX) as usize);
    for y in 0..TILE_PX as usize {
        let line = &rgb[y * stride..y * stride + row];
        for px in line.chunks_exact(3) {
            let h = terrarium_height(px[0], px[1], px[2]);
            if !(LOWEST_M..=HIGHEST_M).contains(&h) {
                return Err(TileError::Implausible(h));
            }
            out.push(h);
        }
    }
    Ok(out)
}

/// Decoded tiles, sampled as one surface.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dem {
    tiles: HashMap<TileId, Vec<f32>>,
}

impl Dem {
    /// Add a decoded tile. One of the wrong length is ignored.
    pub fn insert(&mut self, id: TileId, heights: Vec<f32>) {
        if heights.len() == (TILE_PX * TILE_PX) as usize {
            self.tiles.insert(id, heights);
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// One pixel's height by whole-world pixel position, if its tile is here.
    fn pixel(&self, gx: i64, gy: i64) -> Option<f32> {
        if gx < 0 || gy < 0 {
            return None;
        }
        let t = i64::from(TILE_PX);
        let id = TileId {
            z: ZOOM,
            x: u32::try_from(gx / t).ok()?,
            y: u32::try_from(gy / t).ok()?,
        };
        let tile = self.tiles.get(&id)?;
        tile.get(((gy % t) * t + gx % t) as usize).copied()
    }

    /// Height at `lat`, `lng`, interpolated between the four nearest pixel
    /// centres — across tile edges as readily as within a tile. `None` where
    /// any of the four is missing.
    pub fn height_at(&self, lat: f64, lng: f64) -> Option<f32> {
        let (x, y) = world_px(lat, lng, ZOOM);
        // Pixel centres sit half a pixel in from their corners.
        let (x, y) = (x - 0.5, y - 0.5);
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = ((x - x0) as f32, (y - y0) as f32);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let h00 = self.pixel(x0, y0)?;
        let h10 = self.pixel(x0 + 1, y0)?;
        let h01 = self.pixel(x0, y0 + 1)?;
        let h11 = self.pixel(x0 + 1, y0 + 1)?;
        let top = h00 + (h10 - h00) * fx;
        let bottom = h01 + (h11 - h01) * fx;
        Some(top + (bottom - top) * fy)
    }
}

/// Read a cached tile, if there is a usable one.
pub fn read_cached(root: &Path, id: TileId) -> Option<Vec<u8>> {
    let path = id.cache_path(root);
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > MAX_TILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    bytes.starts_with(&PNG_SIGNATURE).then_some(bytes)
}

/// Keep a fetched tile. Written beside its final name and renamed into place,
/// so a crash mid-write leaves no half tile for the next ride to read.
pub fn store(root: &Path, id: TileId, bytes: &[u8]) -> Result<()> {
    anyhow::ensure!(bytes.starts_with(&PNG_SIGNATURE), "not a PNG");
    anyhow::ensure!(bytes.len() as u64 <= MAX_TILE_BYTES, "tile too large");
    let path = id.cache_path(root);
    let dir = path.parent().context("tile path has no directory")?;
    std::fs::create_dir_all(dir).context("creating the terrain cache")?;
    let tmp = path.with_extension("png.part");
    std::fs::write(&tmp, bytes).context("writing a terrain tile")?;
    std::fs::rename(&tmp, &path).context("keeping a terrain tile")?;
    Ok(())
}

/// Fetch one tile over the network, refusing anything that is not a PNG of a
/// sane size.
async fn fetch(client: &reqwest::Client, id: TileId) -> Result<Vec<u8>> {
    let response = client
        .get(id.url())
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .context("requesting a terrain tile")?
        .error_for_status()
        .context("terrain tile refused")?;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_TILE_BYTES)
    {
        anyhow::bail!("terrain tile too large");
    }
    let bytes = response.bytes().await.context("reading a terrain tile")?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_TILE_BYTES,
        "terrain tile too large"
    );
    anyhow::ensure!(
        bytes.starts_with(&PNG_SIGNATURE),
        "terrain tile is not a PNG"
    );
    Ok(bytes.to_vec())
}

/// Every tile in `ids` as PNG bytes: from the cache where it is there, from
/// the network where not (keeping what arrives). All or nothing — one missing
/// tile is an error, because a gap in the land draws as a cliff.
pub async fn load(root: PathBuf, ids: Vec<TileId>) -> Result<Vec<(TileId, Vec<u8>)>> {
    use futures::stream::{self, StreamExt, TryStreamExt};

    let client = reqwest::Client::builder()
        .user_agent(concat!(
            "TourDuSalon/",
            env!("CARGO_PKG_VERSION"),
            " (io.github.rorynuijens.Cycle)"
        ))
        .build()
        .context("starting the terrain download")?;
    let fetched = ids.len();
    let tiles: Vec<(TileId, Vec<u8>)> = stream::iter(ids)
        .map(|id| {
            let client = client.clone();
            let root = root.clone();
            async move {
                if let Some(bytes) = read_cached(&root, id) {
                    return Ok((id, bytes));
                }
                let bytes = fetch(&client, id).await?;
                if let Err(e) = store(&root, id, &bytes) {
                    // Still usable for this ride; only the next one refetches.
                    tracing::warn!("Could not cache a terrain tile: {e}");
                }
                anyhow::Ok((id, bytes))
            }
        })
        .buffer_unordered(FETCH_CONCURRENCY)
        .try_collect()
        .await?;
    tracing::info!("Terrain ready ({fetched} tiles)");
    Ok(tiles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::route::RoutePoint;

    // ── Tiles ─────────────────────────────────────────────────────────────────

    #[test]
    fn should_find_the_tile_the_dataset_serves_for_maltepe() {
        // The rider's Maltepe start. Worked out independently of this code
        // and fetched while building it: a 256×256 tile whose heights run
        // from 1.6 m to 387 m, which is Maltepe and the hills behind it.
        assert_eq!(
            tile_at(40.94487, 29.14197, 13),
            TileId {
                z: 13,
                x: 4759,
                y: 3073
            }
        );
    }

    #[test]
    fn should_put_the_corners_of_the_world_in_the_corner_tiles() {
        // The square's edge is ±85.0511°; 85° itself is thirteen tiles in.
        assert_eq!(tile_at(85.05, -180.0, 13), TileId { z: 13, x: 0, y: 0 });
        assert_eq!(tile_at(85.0, -180.0, 13), TileId { z: 13, x: 0, y: 13 });
        assert_eq!(
            tile_at(-85.05, 179.9999, 13),
            TileId {
                z: 13,
                x: 8191,
                y: 8191
            }
        );
        // Past the Mercator limit and the dateline still lands on a real tile.
        assert_eq!(
            tile_at(90.0, 180.0, 13),
            TileId {
                z: 13,
                x: 8191,
                y: 0
            }
        );
    }

    #[test]
    fn should_keep_a_tile_inside_the_cache() {
        let id = TileId {
            z: 13,
            x: 4759,
            y: 3069,
        };
        assert_eq!(
            id.cache_path(Path::new("/cache")),
            PathBuf::from("/cache/13/4759/3069.png")
        );
        assert_eq!(
            id.url(),
            "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/13/4759/3069.png"
        );
    }

    fn route_through(points: &[(f64, f64)]) -> Route {
        Route {
            name: "Test".into(),
            points: points
                .iter()
                .enumerate()
                .map(|(i, &(lat, lng))| RoutePoint {
                    lat,
                    lng,
                    elevation_m: 0.0,
                    distance_m: i as f32,
                    gradient: 0.0,
                })
                .collect(),
            total_distance_m: points.len() as f32,
            total_gain_m: 0.0,
        }
    }

    #[test]
    fn should_cover_the_whole_corridor_around_every_point() {
        let route = route_through(&[(40.94, 29.14), (40.95, 29.16), (40.97, 29.19)]);
        let tiles = tiles_for_route(&route, CORRIDOR_M).expect("a short route");
        for p in &route.points {
            let frame = crate::data::course3d::LocalFrame::at(p.lat, p.lng);
            // Probe a grid across the square, not just its corners.
            for i in -4..=4 {
                for j in -4..=4 {
                    let (de, dn) = (CORRIDOR_M * i as f64 / 4.0, CORRIDOR_M * j as f64 / 4.0);
                    let (lat, lng) = frame.to_geo(de, dn);
                    let t = tile_at(lat, lng, ZOOM);
                    assert!(tiles.contains(&t), "{t:?} missing for {de},{dn}");
                }
            }
        }
    }

    #[test]
    fn should_ask_for_no_tiles_for_an_empty_route() {
        assert_eq!(
            tiles_for_route(&route_through(&[]), CORRIDOR_M),
            Ok(Vec::new())
        );
    }

    #[test]
    fn should_refuse_a_route_that_crosses_a_continent() {
        // Istanbul to Paris in one straight line: a GPX, but not a ride.
        let pts: Vec<(f64, f64)> = (0..=400)
            .map(|i| {
                let t = i as f64 / 400.0;
                (41.0 + t * (48.85 - 41.0), 29.0 + t * (2.35 - 29.0))
            })
            .collect();
        assert!(matches!(
            tiles_for_route(&route_through(&pts), CORRIDOR_M),
            Err(TooManyTiles(n)) if n > MAX_TILES
        ));
    }

    // ── Terrarium decoding ────────────────────────────────────────────────────

    #[test]
    fn should_decode_terrarium_at_the_ends_of_its_range() {
        assert_eq!(terrarium_height(128, 0, 0), 0.0);
        assert_eq!(terrarium_height(0, 0, 0), -32_768.0);
        assert_eq!(terrarium_height(255, 255, 255), 32_767.0 + 255.0 / 256.0);
        assert_eq!(terrarium_height(128, 100, 128), 100.5);
        // One step of blue is 1/256 m.
        assert_eq!(terrarium_height(128, 0, 1), 1.0 / 256.0);
    }

    fn tile_of(h: f32) -> Vec<u8> {
        let v = h + 32_768.0;
        let r = (v / 256.0).floor() as u8;
        let g = (v - f32::from(r) * 256.0).floor() as u8;
        let b = ((v - v.floor()) * 256.0) as u8;
        [r, g, b].repeat((TILE_PX * TILE_PX) as usize)
    }

    #[test]
    fn should_decode_a_whole_tile() {
        let heights = decode_terrarium(&tile_of(42.5), TILE_PX, TILE_PX, 768).expect("a tile");
        assert_eq!(heights.len(), 65_536);
        assert!(heights.iter().all(|&h| h == 42.5));
    }

    #[test]
    fn should_skip_row_padding() {
        // 4 bytes of padding after every row, filled with nonsense.
        let mut padded = Vec::new();
        for row in tile_of(10.0).chunks(768) {
            padded.extend_from_slice(row);
            padded.extend_from_slice(&[0, 0, 0, 0]);
        }
        let heights = decode_terrarium(&padded, TILE_PX, TILE_PX, 772).expect("a tile");
        assert!(heights.iter().all(|&h| h == 10.0));
    }

    #[test]
    fn should_refuse_a_tile_of_the_wrong_size() {
        assert_eq!(
            decode_terrarium(&tile_of(0.0), 255, 256, 768),
            Err(TileError::WrongSize(255, 256))
        );
    }

    #[test]
    fn should_refuse_a_truncated_tile() {
        let mut t = tile_of(0.0);
        t.pop();
        assert_eq!(
            decode_terrarium(&t, TILE_PX, TILE_PX, 768),
            Err(TileError::Truncated)
        );
        assert_eq!(
            decode_terrarium(&tile_of(0.0), TILE_PX, TILE_PX, 700),
            Err(TileError::Truncated)
        );
    }

    #[test]
    fn should_refuse_a_tile_that_is_not_on_earth() {
        // All black is −32 768 m: a blank or broken tile, not a trench.
        let black = vec![0u8; 768 * 256];
        assert!(matches!(
            decode_terrarium(&black, TILE_PX, TILE_PX, 768),
            Err(TileError::Implausible(_))
        ));
        // One bad pixel spoils the tile.
        let mut t = tile_of(5.0);
        t[3 * 1000] = 255;
        assert!(decode_terrarium(&t, TILE_PX, TILE_PX, 768).is_err());
    }

    proptest::proptest! {
        #[test]
        fn should_never_panic_on_any_bytes(
            bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..2000),
            w in 0u32..300, h in 0u32..300, stride in 0usize..2000,
        ) {
            let _ = decode_terrarium(&bytes, w, h, stride);
        }
    }

    // ── Sampling ──────────────────────────────────────────────────────────────

    /// Two tiles side by side whose height is the whole-world pixel x, so any
    /// interpolation can be checked exactly.
    fn ramp_dem() -> (Dem, TileId) {
        let a = tile_at(40.94, 29.14, ZOOM);
        let b = TileId { x: a.x + 1, ..a };
        let mut dem = Dem::default();
        for id in [a, b] {
            let h: Vec<f32> = (0..TILE_PX * TILE_PX)
                .map(|i| (id.x * TILE_PX + i % TILE_PX) as f32 - (a.x * TILE_PX) as f32)
                .collect();
            dem.insert(id, h);
        }
        (dem, a)
    }

    /// `lat`, `lng` of a whole-world pixel position at zoom 13.
    fn geo_of(gx: f64, gy: f64) -> (f64, f64) {
        let size = (TILE_PX as f64) * 8192.0;
        let lng = gx / size * 360.0 - 180.0;
        let n = std::f64::consts::PI * (1.0 - 2.0 * gy / size);
        (n.sinh().atan().to_degrees(), lng)
    }

    #[test]
    fn should_interpolate_between_pixel_centres() {
        let (dem, a) = ramp_dem();
        let gy = (a.y * TILE_PX) as f64 + 100.5;
        // Pixel 10's centre is at 10.5; a quarter of the way to pixel 11's.
        let (lat, lng) = geo_of((a.x * TILE_PX) as f64 + 10.75, gy);
        let h = dem.height_at(lat, lng).expect("inside the tile");
        assert!((h - 10.25).abs() < 1e-3, "{h}");
    }

    #[test]
    fn should_be_continuous_across_a_tile_edge() {
        let (dem, a) = ramp_dem();
        let gy = (a.y * TILE_PX) as f64 + 100.5;
        // Between the last pixel of one tile and the first of the next.
        let (lat, lng) = geo_of((a.x * TILE_PX) as f64 + 256.0, gy);
        let h = dem.height_at(lat, lng).expect("spans both tiles");
        assert!((h - 255.5).abs() < 1e-3, "{h}");
    }

    #[test]
    fn should_have_no_height_where_a_tile_is_missing() {
        let (dem, a) = ramp_dem();
        let gy = (a.y * TILE_PX) as f64 + 100.5;
        // The far edge of the second tile needs a third that is not there.
        let (lat, lng) = geo_of((a.x * TILE_PX) as f64 + 511.9, gy);
        assert_eq!(dem.height_at(lat, lng), None);
        assert_eq!(Dem::default().height_at(40.9, 29.1), None);
    }

    #[test]
    fn should_ignore_a_tile_of_the_wrong_length() {
        let mut dem = Dem::default();
        dem.insert(TileId { z: 13, x: 1, y: 1 }, vec![0.0; 10]);
        assert!(dem.is_empty());
    }

    // ── Cache ─────────────────────────────────────────────────────────────────

    fn png_bytes(n: usize) -> Vec<u8> {
        let mut v = PNG_SIGNATURE.to_vec();
        v.resize(n, 7);
        v
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cycle-terrain-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn should_read_back_what_it_stored_and_leave_nothing_half_written() {
        let root = temp_root("roundtrip");
        let id = TileId {
            z: 13,
            x: 4759,
            y: 3069,
        };
        store(&root, id, &png_bytes(1000)).expect("stored");
        assert_eq!(read_cached(&root, id), Some(png_bytes(1000)));
        let leftovers: Vec<_> = std::fs::read_dir(id.cache_path(&root).parent().expect("a dir"))
            .expect("readable")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "part"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn should_refuse_to_store_or_read_what_is_not_a_tile() {
        let root = temp_root("refuse");
        let id = TileId { z: 13, x: 1, y: 2 };
        assert!(store(&root, id, b"<html>Access Denied</html>").is_err());
        assert!(store(&root, id, &png_bytes(MAX_TILE_BYTES as usize + 1)).is_err());
        // A file planted in the cache by something else is not trusted either.
        let path = id.cache_path(&root);
        std::fs::create_dir_all(path.parent().expect("a dir")).expect("dir");
        std::fs::write(&path, b"not a png").expect("write");
        assert_eq!(read_cached(&root, id), None);
        assert_eq!(read_cached(&root, TileId { z: 13, x: 9, y: 9 }), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
