# wtfps

Extracts a [_stupid amount_ of data](https://pub.joel.net.au/datasets/wtfps/viewer.html) from Apple's WiFi Positioning System. Built to gather large amounts of information to use for research and benchmarking.

When devices query Apple's servers for WiFi location information, Apple will respond with up to 400 additional nearby MACs. This was likely implemented to support a client-side cache of nearby WiFi networks, as this would significantly reduce the amount of requests made to Apple's servers, improving privacy and reducing infrastructure costs. However, this is extremely poorly implemented, and can be abused to extract global WiFi coverage information.

wtfps takes advantage of this feature and can be used to scrape estimated locations for over a billion WiFi networks around the world. It keeps a record of where it has explored and will continually query newly discovered MACs in unknown areas to gather more coverage. It does the same for LTE cell towers, and can draw a map of where a cell is likely to serve phones.

## Resources

- [acheong08/apple-corelocation-experiments](https://github.com/acheong08/apple-corelocation-experiments) - reverse engineering, API client implementation in Go
- [Surveilling the Masses with Wi-Fi-Based Positioning Systems](https://www.cs.umd.edu/~dml/papers/wifi-surveillance-sp24.pdf) - research paper
- [ReverseApple/appleloc](https://github.com/ReverseApple/appleloc) - API client implementation in Rust

## Usage

Commands that use the database read `DATABASE_URL` from the environment or from a `.env` file in the current directory; see [Running with Docker Compose](#running-with-docker-compose) for the quickest setup.

### `query`

You can query the location of a single MAC using `wtfps query`:

```
$ wtfps query 90:CD:B6:11:50:D9
https://www.openstreetmap.org/search?query=53.220985410000004%2F-4.20290374#map=12/53.220985410000004/-4.20290374
400 networks in reply
```

### `explore`

`wtfps explore todo` and `wtfps explore crawl` explore Apple's WiFi database. wtfps creates the tables it needs on the first run.

wtfps requires some initial MACs to seed crawling:

```
wtfps=# insert into todo (mac) values ('AA:BB:CC:DD:EE:FF');
wtfps=# \copy todo (mac) from 'seeds.txt';
```

```
# seeds.txt
aabbccddeeff
000000000001
000000000002
```

You can monitor progress by querying the DB:

```sql
-- todo: user-controlled queue
select count(*) from todo;

-- crawl: areas queued for WiFi exploration
select count(*) from crawl where last_crawled is null;

-- wifi: scraped wifi networks
select count(*) from wifi;
```

### `cell`

You can query the location of a single cell tower using `wtfps cell <mcc> <mnc> <tac> <cellid>`. It asks Apple directly and doesn't use the database:

```
$ wtfps cell 284 5 106 451645
cellid: 451645
https://www.openstreetmap.org/search?query=42.66722488%2F23.2772789#map=16/42.66722488/23.2772789
121 entries in reply
cell_id       mnc     tac  earfcn  band            pci   acc_m  location
451645          5     106       -  -                 -    1414  https://www.openstreetmap.org/search?query=42.66722488%2F23.2772789#map=…
1496580         3    1700    1800  3 (1800 MHz)    183    1414  https://www.openstreetmap.org/search?query=42.64780044%2F23.25974273#map=…
362558          5     106    3350  7 (2600 MHz)    387    1414  https://www.openstreetmap.org/search?query=42.65793991%2F23.2717514#map=…
...
area est.       1    1401       -  -                 -  149000  https://www.openstreetmap.org/search?query=42.66709899%2F23.26831436#map=…
```

The reply holds the queried cell, up to ~100 nearby cells from all operators, and up to 20 tracking-area estimates (`area est.`, cell ID `4294967295` in the raw data): rough locations for whole tracking areas with a fixed accuracy of 149000, not cells. Each row shows the cell's MNC, TAC, LTE channel (EARFCN) with its band, physical cell ID (PCI), accuracy in metres and location. 1414 m is the smallest accuracy Apple reports for cells. Cells Apple doesn't know come back with accuracy `-1` and are printed as `not found`.

Add `--raw` to see every field of Apple's reply, including ones wtfps doesn't decode: each field path (`22` is the list of cells, `22.5` a cell's location, `22.5.3` its accuracy) with how often it occurs, how many distinct values it has, and the most common values.

```
$ wtfps cell 452 1 36003 30883339 --raw
reply: 5694 bytes; header 00 01 00 00 00 01 00 00 16 34 (declares 5684 bytes, 5684 follow)

field             count  distinct  most common values (value: times)
22                  120            (message)
22.1                120         1  452: 120
22.3                120       101  -1: 20, 104962572: 1, 104965643: 1, 104985867: 1
22.5.3              120        14  1414: 88, 149000: 20, 10220: 1, 10721: 1
22.5.11             120        10  939: 27, 1: 20, 941: 18, 940: 16
22.5.12             120        99  0: 21, 999: 2, 1004: 1, 1017: 1
...
```

### `explore cell`

Works like `explore todo/crawl` but for cell towers. Seed `cell_todo` with one or more known cell towers and wtfps will crawl outward, discovering and storing nearby cells returned by Apple's API.

Seeds in `cell_todo` are queried first. After that, wtfps crawls by area: every discovered cell is assigned an H3 resolution 7 area (~5 km²) per operator (`mcc`, `mnc`), and one random known cell is queried per uncrawled area in `cell_crawl`. Cells Apple returns without a location are recorded in `cell_not_found`. Cells stored before area crawling existed are assigned areas automatically on the next run.

Apple returns the same fixed group of ~100 cells for every cell in that group, so crawling neighbours alone can't leave the first group. When no areas are left, wtfps probes: an LTE cell ID is `site * 256 + sector`, so it guesses cells at site numbers next to known sites (±1 and ±2, or ±10 and ±20 for operators numbering sites in tens), reusing the known site's TACs and sectors. About 1 in 8 guesses is found, and each hit brings in a new group to crawl. Known sites are tracked in `cell_site`, and every guess is recorded in `cell_probe` so it is never repeated. Pass `--no-probe` to skip probing.

```sh
# seed with a known cell tower (mcc, mnc, tac_id, cell_id)
wtfps=# insert into cell_todo (mcc, mnc, tac_id, cell_id) values (284, 5, 106, 451645);

# start crawling
$ cargo run --release -- explore cell
```

The seed must be a cell Apple currently has a location for; check it first with `wtfps cell`. You can find the cell your phone is using with an app such as NetMonitor on Android, or Field Test mode (`*3001#12345#*`) on iPhone. The TAC must be correct: with a wrong TAC Apple reports the cell as not found.

Monitor progress:

```sql
-- seeded cells pending exploration
select count(*) from cell_todo;

-- areas queued for cell exploration
select count(*) from cell_crawl where last_crawled is null;

-- collected cell towers with location (acc: horizontal accuracy in metres)
select count(*) from cell;

-- cells Apple has no location for
select count(*) from cell_not_found;

-- known sites waiting to be probed
select count(*) from cell_site where probed_at is null;

-- guessed cells tried and found
select count(*), count(*) filter (where found) from cell_probe;
```

Besides position and accuracy (`acc`), each cell stores:

| Column | Meaning |
|---|---|
| `earfcn`, `pci` | LTE channel and physical cell ID. Apple reports them for most cells, but not always for the cell you queried; cells pick them up when they appear in a later reply. |
| `crawled_at` | when the cell was last returned in a reply (refreshed at most hourly); empty for cells crawled before it was added |
| `altitude`, `vertical_acc` | metres, when a reply includes them; so far Apple has only sent `-1` (unknown) for both, on cells it didn't find |
| `unknown_11` | undocumented location field 11: about 940 for real cells, 1 for tracking-area estimates (possibly a version or age) |
| `unknown_12` | undocumented location field 12: grows with the spacing between cells (median 1.9 km where cells are under 385 m apart, 6.4 km where they're over 1.4 km apart), so it behaves like the cell's reach in metres; its meaning is a guess |

### `heatmap`

Writes a self-contained HTML map estimating where a cell is likely to serve phones:

```
$ wtfps heatmap 452 4 38211 209991683
wrote cell-452-4-38211-209991683.html (799 nearby cells)
```

The cell doesn't have to be crawled yet. If it isn't in the database, wtfps asks Apple for it and for its site's neighbouring sector numbers (the cell's sector ±1 and ±2: Apple's reply for a cell often lacks its own site's other sectors, which antenna directions need), saves every cell in the replies as the crawler would, then draws the map. That's five requests, about a second:

```
$ wtfps heatmap 284 5 106 451645
cell wasn't crawled yet: fetched it from Apple and saved 103 cells
wrote cell-284-5-106-451645.html (36 nearby cells)
```

The saved cells' areas are queued for the crawler, so a later `explore cell` continues from there. A cell Apple doesn't know is reported and recorded in `cell_not_found`.

| Option | Meaning |
|---|---|
| `--online` | build the map from Apple's replies only, without a database: nothing is read or saved. The map only has the cells from those five replies (one group of ~100 around the cell), so it's thinner near the group's edge than a crawled area. |
| `--no-fetch` | only use the database: fail instead of asking Apple for a cell that isn't crawled |
| `--radius-km` | include the operator's cells within this distance (default 50; at most the 800 nearest, so towns use a few km and rural cells, which reach tens of km, still see their competitors) |
| `--out` | output file (default `cell-<mcc>-<mnc>-<tac>-<cell>.html`, or `.geojson`) |

Open the file in a browser. It shows the cell, its accuracy circle and the operator's nearby cells, with two views:

- **Serving likelihood**: the chance this cell is the strongest of its operator at each spot.
- **Relative signal**: this cell's modelled signal above the edge of coverage, ignoring other cells.

The coverage is a model, not a measurement: signal falls off with distance, and a phone uses the strongest cell on its frequency, allowing for random variation from buildings and terrain. The map controls:

| Control | Default | Meaning |
|---|---|---|
| Path-loss exponent | 3.5 | how fast signal falls with distance: about 3.4–3.7 for masts of 50–15 m (Okumura–Hata's distance slope; see [below](#propagation-model-and-okumurahata)) |
| Signal variation | 8 dB | random variation from buildings and terrain |
| Accuracy weight | 0 | how much each cell's position is blurred by its accuracy |
| Max range | Okumura–Hata's radius for the cell's band and environment, 0.5–60 km | where signal fades out; the map draws out to 3× this, so the gradual fade-out isn't cut off. Shown with its basis, e.g. "1.3 (Hata, urban)"; without a known band, 4× the typical cell spacing |
| Propagation model | Okumura–Hata | `Okumura–Hata (COST-231)`: lower frequencies carry further and the max range comes from Hata's radius (see [below](#propagation-model-and-okumurahata)); `Simple (distance only)`: every frequency is equal and the max range is 4× the cell spacing, the model before Hata was added |
| Only cells on this cell's frequency | on | the cell only competes with cells on its own EARFCN, because operators run several frequency layers over the same area; off when its EARFCN is unknown. When off, lower-frequency cells carry further by Okumura–Hata's frequency term |
| Contour lines | on | lines at 10, 25, 50, 75 and 90% likelihood, or every 5 dB in the signal view; the thicker 50% line is where the cell becomes the most likely server |
| Antenna directions | off | experimental: sectors get a wedge-shaped antenna pattern pointing from their site's centre towards their position (a rough estimate); the orange line shows this cell's direction |
| Field 12 | off | experimental: fade out at the cell's own reach from `unknown_12` instead of the shared max range |

Everything is recalculated in the browser as you move the controls; hover anywhere for the values at that spot, or over a cell for its details. The map loads MapLibre, d3-contour and the OpenFreeMap basemap from the internet; the cell data is embedded in the file. See [`examples/`](examples/) for generated maps with screenshots and comparisons of the options.

Add `--geojson` to write the same data as GeoJSON instead: the cell (`role: "cell"`), its accuracy circle (`role: "accuracy"`, a polygon with `radius_m`), and the nearby cells (`role: "nearby"`) with `distance_m`, `bearing_deg` from the cell (clockwise from north) and `same_frequency`. Every cell carries `mcc`, `mnc`, `tac_id`, `cell_id`, `site`, `sector`, `acc_m`, `earfcn`, `pci`, `unknown_12`, `crawled_at` and `azimuth_deg` (estimated antenna direction).

```
$ wtfps heatmap 452 4 38211 209991683 --geojson --radius-km 3
wrote cell-452-4-38211-209991683.geojson (103 nearby cells)
```

The GeoJSON also contains the modelled coverage, calculated by wtfps with the same model as the HTML map: H3 hexagons (`role: "coverage"`) wherever the cell has at least a 1% chance of serving, each with `likelihood` (0–1), `signal_db` (modelled signal above the edge of coverage) and the most likely serving cell there (`most_likely_cell_id`, `most_likely_tac_id`, `most_likely_likelihood`). The cell's own feature gets a `coverage` summary: `area_km2` and `reach_m` where its likelihood is above 50% (measured on hexagons two resolutions finer than the ones written), the number of competing cells, and the model settings used. Features carry `fill` / `fill-opacity` / `marker-color` styling, which geojson.io and GitHub's map preview apply automatically. The model settings match the map controls:

| Option | Default | Meaning |
|---|---|---|
| `--path-loss` | 3.5 | how fast signal falls with distance (about 3.4–3.7 for masts of 50–15 m) |
| `--sigma-db` | 8 | random signal variation from buildings and terrain |
| `--acc-weight` | 0 | how much each cell's position is blurred by its accuracy |
| `--range-km` | Okumura–Hata radius for the band and environment (or 4× spacing), 0.5–60 km | distance at which signal fades out; the summary's `range_basis` says how it was chosen |
| `--propagation` | `hata` | `hata` (Okumura–Hata / COST-231) or `simple` (distance only, range 4× the cell spacing); recorded in the summary's `propagation` |
| `--all-frequencies` | off | compete with cells on every frequency, not just the cell's own EARFCN, each weighted by Okumura–Hata's frequency term |
| `--directional` | off | experimental: wedge-shaped antenna patterns; adds the cell's direction as a line (`role: "antenna"`) |
| `--use-unknown-12` | off | experimental: fade out at the cell's own reach from field 12 instead of the shared range |
| `--hex-res` | 10, coarser for large rural areas | H3 resolution of the hexagons (10: ~0.015 km², 11: ~0.002 km²); chosen automatically so a map has at most ~20,000 |
| `--contours` | off | write smooth likelihood contour bands instead of hexagons |
| `--contour-levels` | 0.1,0.25,0.5,0.75,0.9 | where the bands start; the last band runs up to 100% |
| `--contour-step-m` | 20 | spacing of the grid the contours are traced on |

With `--contours`, the coverage is written as contour bands (`role: "contour"`) instead of hexagons: one MultiPolygon per band between two levels, with `likelihood_min`, `likelihood_max`, a `label` such as `"50–75%"`, and the same styling. Bands don't overlap: each is a ring around the next, so the area where the cell is the most likely server is the sum of the bands from 50% up. The files are smaller than with hexagons and look like the HTML map's heat, except that the bands start at the lowest level (10% by default) where the map fades down to 2%; add a lower level such as `0.02` to show that faint fringe.

### Propagation model and Okumura–Hata

A summary; [docs/propagation-models.md](docs/propagation-models.md) explains both models in detail: the formulas, every parameter, how the likelihood and the range are computed, a comparison on three cells, and the limits.

The model follows the [Okumura–Hata](https://en.wikipedia.org/wiki/Hata_model) path-loss formula (COST-231 Hata above 1500 MHz), with a 30 m mast and the phone at 1.5 m:

- **Distance**: Hata's path loss grows by 44.9 − 6.55·log₁₀(mast height) dB per tenfold distance: 35.2 dB for a 30 m mast, which is a path-loss exponent of 3.52. The default of 3.5 is that slope. Hata's suburban and rural corrections only subtract a constant, so the slope stays the same outside towns; an exponent of 2 (free space) only applies to short line-of-sight paths.
- **Frequency**: each cell's downlink frequency comes from its EARFCN, and costs it Hata's frequency loss relative to the mapped cell: +1.3 dB at 900 MHz, +11 at 1800, +13 at 2100 and +16.5 at 2600 MHz compared with 800 MHz. With the default same-frequency competition this term is equal for all cells and cancels out; with every frequency competing it lets low-band cells carry further.
- **Range**: by default, the distance at which Hata's path loss reaches 140 dB (a typical LTE link budget) at the cell's frequency, in an environment judged by how far apart cells on its own channel are: urban under 800 m, suburban under 2 km, rural beyond. For example 1.3 km for band 3 in a town, 16.6 km for band 20 in the countryside. Cells without a known EARFCN fall back to 4× the typical cell spacing.

The simple model (`--propagation simple`, `?propagation=simple`, or the selector on the map) leaves out the frequency term and takes the range from 4× the cell spacing, as wtfps did before. In the default same-frequency competition both models give nearly the same core area; they differ in how far the faint fringe reaches and, with every frequency competing, in how much the low bands win.

Hata was fitted for 150–1500 MHz (COST-231 to 2000 MHz), distances of 1–20 km and masts of 30–200 m; higher bands are extrapolated, and dense town cells a few hundred metres apart are outside its range, as they are outside the simple model's. Apple's field 12 (`unknown_12`) supports the frequency effect: relative to the spacing of their cells, 800 and 900 MHz cells reach 1.7–2× further than 1800 and 2100 MHz cells, where Hata predicts 1.6× (rural) to 2.1× (urban) for 800 against 1800 MHz.

### HTTP API

The OpenAPI 3.1 specification is in [docs/openapi.yaml](docs/openapi.yaml); open it in any OpenAPI viewer (for example Swagger Editor or Redocly) for an interactive reference.

```
GET /cells/{mcc}/{mnc}/{tac}/{cell}/heatmap                  HTML map
GET /cells/{mcc}/{mnc}/{tac}/{cell}/heatmap?format=geojson   GeoJSON
```

The same lookup as `heatmap`: the cell is looked up in the database; if it isn't crawled yet, it's fetched from Apple with its site's sibling sectors (five requests, about a second) and saved together with every other cell in the replies, then the map is drawn. Later requests for the same cell are served from the database (tens of milliseconds). The HTML map has all the controls above; the GeoJSON uses the default model settings and a 50 km radius. Add `propagation=simple` to use the simple propagation model instead of Okumura–Hata (`propagation=hata`, the default), for both formats; the HTML map then opens with that model selected, and keeps it when you click another cell. On an HTML map served by the API, clicking another cell opens that cell's map (the browser's Back button returns); on a map opened as a file, clicking does nothing.

| Status | When |
|---|---|
| `200` | the map: `text/html`, or `application/geo+json`. The `X-Wtfps-Source` header says where it came from: `database`, `fetched` (from Apple and saved) or `online` (from Apple, not saved: see below) |
| `400` | an unknown `format`, or a cell number that isn't a number |
| `404` | the cell isn't in the database and Apple has no location for it; it's recorded in `cell_not_found` (any neighbours Apple returned are still saved). The message says so, e.g. `Apple has no location for cell 262/1/100/1000001 (recorded in cell_not_found)`. |
| `500` | Apple couldn't be reached or kept failing after retries, or a database query failed; the message has the error |

A cell that got a `404` is asked about again on every request, so a cell Apple learns about later is found.

#### Without a database

If the database can't be reached (down, refusing connections, or no connection within 2 seconds), the API doesn't fail: it builds the map from Apple's replies (the cell and its sibling sectors) and saves nothing. The response then has `X-Wtfps-Source: online`, the HTML map's subtitle ends with "from Apple, not saved: database unavailable", and the GeoJSON cell has `"source": "online"` and `"saved": false`. Each fallback is logged as a warning. The map is thinner than one from a crawled area: it only has the ~100 cells in those replies.

The server also starts while the database is down, and applies migrations in the background, retrying every 10 seconds; until they have run (on a new database the tables don't exist yet) requests are answered from Apple the same way. Once the database is back, requests use it and save again, without a restart. Only being unable to reach the database triggers the fallback: a failing query still gives `500`, so real problems aren't hidden.

`wtfps heatmap` does the same, printing `not saved (database unavailable)`; with `--no-fetch` it fails instead. The crawlers (`explore`) always need the database.

```sh
curl -o map.html http://localhost:8090/cells/284/5/106/451645/heatmap
curl -o map.geojson 'http://localhost:8090/cells/284/5/106/451645/heatmap?format=geojson'
```

Run it with Docker Compose (see [below](#the-http-api-in-docker)), or directly with `wtfps serve` (default `--listen 0.0.0.0:8090`). It listens on all network interfaces, with no authentication.

### `migrate`

`explore` applies pending database migrations when it starts, and `serve` in the background (retrying until the database can be reached). To update the schema without crawling, run `wtfps migrate`.

## Running with Docker Compose

You don't need a local Postgres or the SQLx CLI. `docker-compose.yml` runs two services:

- `db`: the `postgis/postgis` image, Postgres with PostGIS, on `127.0.0.1:WTFPS_DB_PORT` (55432) for wtfps on this machine;
- `api`: wtfps serving the HTTP API on `WTFPS_API_PORT` (8090), reachable from the LAN.

`.env.example` lists the settings. Copied to `.env`, both `docker compose` and wtfps read it, and `DATABASE_URL` is built from `POSTGRES_USER`, `POSTGRES_PASSWORD`, `WTFPS_DB_PORT` and `POSTGRES_DB`, so changing one keeps them in step. The password only takes effect when the volume is first created.

### The database

The data lives in the volume `postgis_data`, which Compose treats as external: it uses it but never creates or deletes it, so the crawl survives `docker compose down` (even with `-v`), restarts and image updates. Only `docker volume rm postgis_data` would delete it.

```sh
# 1. start the database (waits until it accepts connections)
cp .env.example .env                  # settings for both wtfps and docker compose
docker volume create postgis_data     # once; does nothing if it already exists
docker compose up -d --wait db

# 2. build; SQLX_OFFLINE uses the saved query metadata in .sqlx/ instead of a live database
SQLX_OFFLINE=true cargo build --release

# 3. create the tables
./target/release/wtfps migrate

# 4. seed a cell Apple knows and start crawling
docker compose exec db psql -U postgres -d wtfps \
  -c "insert into cell_todo values (284, 5, 106, 451645)"
./target/release/wtfps explore cell
```

The first run prints `extension "postgis" already exists, skipping`. That is a harmless notice: the image already has PostGIS enabled. To hide it, set `RUST_LOG=info,sqlx::postgres::notice=warn`.

Watch progress from a second terminal, or open an interactive prompt with `docker compose exec db psql -U postgres -d wtfps`:

```sh
docker compose exec db psql -U postgres -d wtfps \
  -c "select count(*) cells from cell" \
  -c "select count(*) filter (where found) hits, count(*) probes from cell_probe" \
  -c "select count(*) waiting_areas from cell_crawl where last_crawled is null"
```

### The HTTP API in Docker

```sh
docker compose up -d --build api
curl -o map.html http://localhost:8090/cells/284/5/106/451645/heatmap
```

The `api` service starts once `db` is healthy and connects to it over Docker's internal network. Set `WTFPS_API_PORT` in `.env` to use another port. The image is built from the `Dockerfile` with the official `rust` and `debian` images only; Cargo's downloads and build output are kept in BuildKit cache mounts, so the first build takes about 2.5 minutes and later builds after a source change about 25 seconds. `docker builder prune` clears that cache (it has nothing to do with the database volume).

### Reading cell coordinates

Coordinates are stored in the `geom` column (PostGIS `geography(Point)`, WGS84) of the `cell` and `wifi` tables. It is a binary type, so convert it to read it. Note that `ST_AsText` and `ST_MakePoint` put longitude first.

```sql
-- latitude / longitude as numbers, with accuracy in metres
select mcc, mnc, tac_id, cell_id,
       ST_Y(geom::geometry) as lat, ST_X(geom::geometry) as lon, acc
from cell limit 10;

-- as text: POINT(lon lat)
select cell_id, ST_AsText(geom) from cell limit 10;

-- cells within 2 km of a point (lon, lat)
select cell_id, ST_Y(geom::geometry) lat, ST_X(geom::geometry) lon
from cell
where ST_DWithin(geom, ST_MakePoint(23.2774, 42.6672)::geography, 2000);
```

Export all cells to CSV:

```sh
docker compose exec db psql -U postgres -d wtfps -c "\copy (select mcc, mnc, tac_id, cell_id, ST_Y(geom::geometry) lat, ST_X(geom::geometry) lon, acc from cell) to stdout csv header" > cells.csv
```

### Stopping the crawler

Press Ctrl+C, or from another terminal run `pkill -INT -f 'wtfps explore'`. Each batch is saved in its own transaction: finished batches are kept, and an interrupted batch is simply redone on the next run. Run `wtfps explore cell` again to resume.

The crawler also stops by itself once there are no seeds, uncrawled areas or unprobed sites left. With probing that can take a very long time, so to cap a run:

```sh
# stop once the queued areas are done
./target/release/wtfps explore cell --no-probe

# stop after 10 minutes
timeout 10m ./target/release/wtfps explore cell
```

`docker compose stop` (or `down`, even `down -v`) stops the containers and keeps the data.

### Switching from a `docker run` container to Compose

The Compose volume is named exactly `postgis_data`, so a database started with `docker run ... -v postgis_data:/var/lib/postgresql/data` is reused as is. Stop the crawler and the old container first: two servers must never use the same data directory at once.

```sh
docker stop wtfps-test            # with --rm this removes the container, not the named volume
docker compose up -d --wait db
docker compose exec db psql -U postgres -d wtfps -c "select count(*) from cell"
./target/release/wtfps migrate
```

To check how a container was started, run `docker inspect wtfps-test --format '{{range .Mounts}}{{.Type}} {{.Name}} -> {{.Destination}}{{"\n"}}{{end}}'`. If it shows a `volume` with a long random name instead of `postgis_data`, the data is in an anonymous volume, which `--rm` deletes when the container stops. Copy it across before stopping the container:

```sh
docker exec wtfps-test pg_dump -U postgres -Fc wtfps > wtfps.dump
docker stop wtfps-test
docker volume create postgis_data
docker compose up -d --wait db
docker compose exec -T db pg_restore -U postgres -d wtfps --clean --if-exists < wtfps.dump
docker compose exec db psql -U postgres -d wtfps -c "select count(*) from cell"
```

Keep `wtfps.dump` until you've checked the restored data.

## Limits: LTE only

The cell features assume LTE. All cells Apple has returned so far look like LTE, but its replies don't say which radio technology a cell uses, and the database has no column for it. Cells without an EARFCN or PCI, such as 3G cells imported from elsewhere, would work only partly:

- **Lookup** works: cells in the database are served from it. A missing cell is asked of Apple, which doesn't seem to know 3G cells, so the answer is `404`.
- **Frequency filter**: with no EARFCN it's off, so the cell competes with every cell of its operator, all channels mixed; with several carriers that makes its area too small. Band names are empty.
- **Antenna directions and crawler probing** are wrong for 3G: both take the site as `cell_id / 256`, the LTE numbering. A 3G cell ID is the radio network controller × 65,536 plus a local cell ID whose sector numbering is up to the operator.
- **Channel numbers overlap between technologies.** Bands are named from the LTE table, but some 3G channel numbers fall in the same ranges: 2950, reported for MNC 1 cells in Bulgaria, is LTE band 7 (2600 MHz) or 3G band 8 (900 MHz). A cell labelled "band 7" with a number between 2937 and 3088 may be 3G.

Positions, accuracy, the coverage model and contours work the same for any cell.

## Development

wtfps is a Nix flake, meaning you can easily pull required dependencies into your environment using `nix dev` and can also compile the project using `nix build`.

If you don't have Nix setup on your system, you will need:

- Rust (edition 2024)
- Protobuf (`protoc`): required to serialize requests for Apple's servers
- Postgres 16 with PostGIS: the [Docker Compose setup](#running-with-docker-compose) provides it

Build with `SQLX_OFFLINE=true cargo build --release`: the WiFi queries are checked at compile time against the saved query metadata in `.sqlx/`. After changing one of those `query!` macros, regenerate it against a running database with the [SQLx CLI](https://github.com/launchbadge/sqlx/tree/main/sqlx-cli#install) (`cargo install sqlx-cli --no-default-features --features rustls,postgres`, then `cargo sqlx prepare`). The cell queries are checked at run time and don't need this.

| File | Contents |
|---|---|
| `src/main.rs` | commands, crawlers, storing replies, heatmap data and GeoJSON |
| `src/fetch.rs` | Apple client (request framing, retries, decoding) |
| `src/lookup.rs` | heatmap lookup: database first, else fetch from Apple and save |
| `src/coverage.rs` | coverage model, antenna directions, contour bands |
| `src/heatmap.html` | HTML map template; its JavaScript has its own copy of the model, kept in sync with `coverage.rs` |
| `src/api.rs` | HTTP API |
| `src/raw.rs` | schema-less dump of Apple's replies (`cell --raw`) |
| `src/apple.proto` | Apple's protobuf messages, including the undocumented fields |
| `migrations/` | database schema, applied automatically |
