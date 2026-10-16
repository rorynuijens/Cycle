# Blueprint: the 3D route from behind the rider

How Tour du Salon builds the road ahead. This is the plan of record.

> **Since 15 October 2026 the world is made up, not mapped** (the rider's decision, with a
> reference screenshot: a low-poly cartoon countryside seen from behind a low-poly cyclist).
> No OpenStreetMap, no elevation tiles, no route packs, no network at all. The GPX is the
> only input. The earlier plan (map data, 3D BAG, AHN, Overpass etiquette) is retired; the
> research behind it stays in `reports/OSM data for realistic 3D cycling.md` (git-ignored).
>
> **Since 16 October 2026 the road is made up too** (the rider's call: tight bends cornered
> "very weird"). The GPX gives the road its distance, its heights and the bends the rider
> brakes for — nothing else of its shape.

---

## 1. Rules (the `stylized-route` skill, §1)

1. **One route spine.** The GPX gives the distance, the heights and the bends. The ride and
   the view read the same numbers; the view never writes them.
2. **One elevation function**, linear between GPX points. No visual smoothing of height.
3. **The road's height is never changed.** The land bends to the road.
4. **1 km ridden is 1 km of road.** The road is laid out metre by metre from the distance.
5. **Nothing visual feeds the ride.** Bends are measured on the GPX alone (`data/bends.rs`);
   the physics brakes for them and the view lays its road through them, never the reverse.
6. **Deterministic.** Every choice is seeded from the route itself or the place.
7. **The road is never under water.** Streams keep more than 30 m from the road's edge.

Each rule has a test in `src/data/` (see section 6).

## 2. Pipeline

```
GPX route
  └─ bends::curvatures      signed curvature per GPX point (40 m chord), shared with SIM braking
  └─ track::lay             the made-up road: distance + GPX heights + the braking bends
       └─ world::World       seed from the route; land height = the road's height carried
       │                     out by inverse-distance weighting + noise hills that grow away
       │                     from the road; fields, woods, streams, poplar rows
       └─ course3d::world_mesh  fine ground (20 m) to 1.2 km, coarse ground (80 m) to 3.2 km,
       │                     fitted to the road (flat shoulder, blend, bank band)
       ├─ roadway            the track, its paint, grass banks under its edges
       ├─ trees, animals     props: instanced shapes with seeded placement
       ├─ sky                mountains on the horizon and clouds, around the eye
       └─ biome, palette     every colour by name; biome weights blur the look
  └─ ui/widgets/course_view.rs  GL: sky → horizon → world, rider, props → outline pass
```

Built off the main thread when a route is set: about 1 s in release for a 40 km route.

## 3. The look

* **Camera:** a chase camera 4 m behind and 2.1 m above the rider, aimed 14 m ahead, looking
  along the road's direction averaged from 12 m behind the rider to 4 m ahead — so it swings
  round a bend smoothly and a little after the rider, as a following camera would.
* **Rider:** low-poly boxes — a yellow helmet (shell, narrower crown, dark vents and
  straps) over brown hair, orange jersey, red frame; all colours from one `rider::Avatar`. The legs turn with
  the distance ridden (one crank turn per 6.5 m), so they stop when the rider stops. The rider
  faces along the road under the wheels (±1.5 m) and leans into a bend by `tan θ = v²/(g r)`,
  at most 35°, upright when stopped.
* **Road:** a fixed game track, 5.2 m wide, yellow edges, dashed centre line. The rider
  rides the middle of the right-hand lane.
* **Land:** lime meadows; fields (wheat, ploughed, green) in patches behind a 22 m grass
  verge; woods on patches that grow away from the road; rock only on banks steeper than 50°.
* **Water:** meandering streams with drifting stripes.
* **Trees:** round, cone and poplar shapes; woods thinned past 350 m; poplar rows along
  the road; lone trees in the meadows.
* **Animals:** sheep flocks grazing (turning, heads dipping) 18 m or more from the road;
  birds circling high, wings beating. Moved on the GPU from the time alone.
* **Sky:** a gradient sky, blocky clouds drifting round, pale low-poly mountains with snow.
* **Light:** flat faces, three toon bands, a touch of sky ambient, rim light,
  squared-exponential haze that is complete before the world's edge.
* **Outlines:** a post pass on depth folds and sharp normal turns, fading with the haze.

## 4. The made-up road (`data/track.rs`)

* **Kept from the GPX:** every GPX point is a road point at its own distance and height, so
  the road climbs in the physics' own straight lines; the road is exactly as long as the ride.
* **Bends the rider brakes for** (radius under 250 m, whole under 150 m — SIM braking starts
  at about 117 m) are laid at the same distance with the same radius.
* **Never more than 75° off straight ahead.** A bend that would turn further is mirrored if
  that keeps the road nearer straight, else only its middle is kept at its own radius — the
  tightest part, where the rider is slowest. So the road never comes back on itself: every
  metre gains at least 0.26 m forward, and out-and-backs, laps and U-turns ride on into new
  country. No bridges over itself, no road drawn twice.
* **Everything else is made up:** gentle seeded bends (never tighter than 400 m, about ±28°),
  steering back towards a middle line once the road drifts 1.5 km aside.
* **Ground at a bend:** where the road within reach turns by more than 20°, the ground is held
  under every point of it within a grid cell's reach (a flat triangle across a hairpin on a
  climb otherwise rose over the lower leg); where the grade changes it is held lower by a
  quarter of the change over that reach (a triangle bridging the foot of a steep drop stood
  over it). Grass banks 20 m deep hang from the road's edges so it never floats.
* **Verge:** over that held-down ground, a grass strip either side of the road at its own
  height (2 cm under the edge), level for 1.5 m, then sloping out to meet the drawn ground by
  36 m — stopping halfway to any other stretch of road. Without it every road stood on a bank
  (2 m on a noisy climb). Trees and animals stand on the verge or the drawn ground.

The earlier real-shape road needed a length-fitted smoothing, hidden passes for roads ridden
twice (a shader discard) and bridges over itself; all of that is gone. The 2D route map still
shows the real route.

## 5. Next

| Step | Adds |
|---|---|
| S2 | An "Animals" setting; more animals per biome; the street-dog encounter |
| S3 | `theme:<biome>` and `mix` modes; finer ground near the road; biome terrain shaping |
| S4 | Signature props (windmills, barns, a lighthouse), km markers and gradient signs |
| S5 | Make your own avatar: build (male/female), outfit, hair, helmet, bike — `rider::Avatar` is the hook |

## 6. How it is verified

* **Rule tests:** road at the ride's height every 10 m; a gradient snapshot worked by hand;
  road as long as the ride and every GPX point on it; never past 75° and never back past
  itself (out-and-back, 6 m U-turn, laps, switchbacks); the braking bend's radius seen where
  the rider brakes; no water above the road within 30 m; identical worlds across builds; no
  animal or tree near the road; no ground triangle over the road on a steep hairpin or at
  the foot of a drop. All ten saved routes probed: no ground over the road anywhere.
* **Cornering tests:** the rider faces the road under the wheels round a 12 m bend; the lean
  matches `v²/(g r)` both ways, upright when stopped or for a NaN speed, capped at 35°; the
  camera never turns faster than the road and eases into a bend.
* **Offscreen renders:** `render_course_offscreen` draws the real view through EGL into a
  PNG (`CYCLE_3D_GPX`, `CYCLE_3D_OUT`, `CYCLE_3D_AT`, `CYCLE_3D_TIME`, `CYCLE_3D_SPEED`, `CYCLE_3D_ES=1`,
  `CYCLE_3D_GTK_STATE=1`). Run with `--release`.
* **Desk build with a fake rider:** `CYCLE_FAKE_RIDER=200 cargo run` (debug only).
* **The rider looks before it is committed.**

## 7. Traps already paid for

* GTK leaves its GL state behind (depth test on, depth cleared to 0): set everything, every frame.
* Flat colours come from the provoking vertex; per-face jitter on a wall draws a seam.
* The world's own distance to the road (anchors 250 m apart) cuts corners; anything that
  must be exact near the road (the verge) uses the ground builder's nearest-road index.
* A stream's clearance must include its own width and half a step of its line.
* A rider heading taken 20 m up the road faced across every tight bend; take it under the
  wheels and let the camera do the smoothing.
* A test road built on grid lines hides ground faults: put the knee of a grade change off them.
* Nearest-road search must be per-bucket candidate lists, or the build takes a minute.
