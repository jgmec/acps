# wtfps

Extracts a [_stupid amount_ of data](https://pub.joel.net.au/datasets/wtfps/viewer.html) from Apple's WiFi Positioning System. Built to gather large amounts of information to use for research and benchmarking.

When devices query Apple's servers for WiFi location information, Apple will respond with up to 400 additional nearby MACs. This was likely implemented to support a client-side cache of nearby WiFi networks, as this would significantly reduce the amount of requests made to Apple's servers, improving privacy and reducing infrastructure costs. However, this is extremely poorly implemented, and can be abused to extract global WiFi coverage information.

wtfps takes advantage of this feature and can be used to scrape estimated locations for over a billion WiFi networks around the world. It keeps a record of where it has explored and will continually query newly discovered MACs in unknown areas to gather more coverage.

## Resources

- [acheong08/apple-corelocation-experiments](https://github.com/acheong08/apple-corelocation-experiments) - reverse engineering, API client implementation in Go
- [Surveilling the Masses with Wi-Fi-Based Positioning Systems](https://www.cs.umd.edu/~dml/papers/wifi-surveillance-sp24.pdf) - research paper
- [ReverseApple/appleloc](https://github.com/ReverseApple/appleloc) - API client implementation in Rust

## Usage

### `query`

You can query the location of a single MAC using `wtfps query`:

```
$ wtfps query 90:CD:B6:11:50:D9
https://www.openstreetmap.org/search?query=53.220985410000004%2F-4.20290374#map=12/53.220985410000004/-4.20290374
400 networks in reply
```

### `explore`

In order to explore Apple's DB, you'll need to setup Postgres and providing wtfps with connection information in the `DATABASE_URL` environment variable. wtfps will automatically create needed tables on first run of `wtfps explore`.

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

You can query the location of a single cell tower using `wtfps cell <mcc> <mnc> <tacid> <cellid>`:

```
$ wtfps cell 284 5 106 451645
cellid: 451645
https://www.openstreetmap.org/search?query=42.6671791%2F23.277446740000002#map=16/42.6671791/23.277446740000002
121 entries in reply
cellid 451645		horizontal_accuracy Some(1414)		location https://www.openstreetmap.org/search?query=42.6671791%2F23.277446740000002#map=16/42.6671791/23.277446740000002
...
```

Each row shows the cell's MNC, TAC, EARFCN with its LTE band, PCI, accuracy and location; the tracking-area estimates are labelled `area est.`.

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

The reply holds the queried cell, up to ~100 nearby cells from all operators, and up to 20 entries with cell ID `4294967295`. Those are rough location estimates for whole tracking areas (fixed accuracy of 149000), not cells. `horizontal_accuracy` is in metres; 1414 is the smallest value Apple reports for cells. Cells Apple doesn't know come back with accuracy `-1` and are printed as `not found`.

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

For each cell wtfps also stores the LTE channel (`earfcn`) and physical cell ID (`pci`) when Apple reports them; Apple reports them for neighbouring cells but not for the cell you query. Cells crawled before these columns existed pick them up when they appear in a later reply.

`altitude` and `vertical_acc` (metres) are stored when a reply includes them; so far Apple has only sent `-1` (unknown) for both, on cells it didn't find. `crawled_at` records when each cell was last crawled, i.e. returned in a reply (refreshed at most hourly); cells crawled before it was added have no value. Two undocumented location fields are stored too, so their meaning can be worked out from the data: `unknown_11` (about 940 for real cells, 1 for tracking-area estimates; possibly a version or age) and `unknown_12` (varies widely and tracks the accuracy; possibly a finer accuracy figure before Apple's 1414 m minimum).

### `heatmap`

Writes a self-contained HTML map estimating where a cell is likely to serve phones:

```
$ wtfps heatmap 452 4 38211 209991683
wrote cell-452-4-38211-209991683.html (285 nearby cells)
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
| `--online` | build the map from Apple's replies only, without a database: nothing is read or saved, and `DATABASE_URL` isn't needed. The map only has the cells from those five replies (one group of ~100 around the cell), so it's thinner near the group's edge than a crawled area. |
| `--no-fetch` | only use the database: fail instead of asking Apple for a cell that isn't crawled |

Open the file in a browser. It shows the cell, its accuracy circle, and the nearest cells of the same operator (up to 800 within `--radius-km`, default 15), with two views:

- **Serving likelihood**: the chance this cell is the strongest of its operator at each spot.
- **Relative signal**: this cell's modelled signal above the edge of coverage, ignoring other cells.

The coverage is a model, not a measurement: signal falls off with distance, and a phone uses the strongest cell on its frequency, allowing for random variation from buildings and terrain. By default the cell only competes with cells on its own EARFCN, because operators run several frequency layers over the same area. Sliders adjust the path-loss exponent, signal variation, accuracy weight and maximum range; the page explains each one. Contour lines (on by default) mark 10, 25, 50, 75 and 90% likelihood, or every 5 dB in the signal view; the thicker 50% line is where the cell becomes the most likely server. They're traced in the browser with d3-contour, so they follow the sliders. The cell must already be in the database (crawl it with `explore cell` first). The map loads MapLibre, d3-contour and the OpenFreeMap basemap from the internet; the cell data is embedded in the file.

See [`examples/`](examples/) for two generated maps with screenshots.

Add `--geojson` to write the same data as GeoJSON instead: the cell (`role: "cell"`), its accuracy circle (`role: "accuracy"`, a polygon with `radius_m`), and the nearby cells (`role: "nearby"`) with `distance_m`, `bearing_deg` from the cell (clockwise from north) and `same_frequency`. Every cell carries `mcc`, `mnc`, `tac_id`, `cell_id`, `site`, `sector`, `acc_m`, `earfcn` and `pci`.

```
$ wtfps heatmap 452 4 38211 209991683 --geojson --radius-km 3
wrote cell-452-4-38211-209991683.geojson (103 nearby cells)
```

The GeoJSON also contains the modelled coverage, calculated by wtfps with the same model as the HTML map: H3 hexagons (`role: "coverage"`) wherever the cell has at least a 1% chance of serving, each with `likelihood` (0–1), `signal_db` (modelled signal above the edge of coverage) and the most likely serving cell there (`most_likely_cell_id`, `most_likely_tac_id`, `most_likely_likelihood`). The cell's own feature gets a `coverage` summary: `area_km2` and `reach_m` where its likelihood is above 50% (measured on hexagons two resolutions finer than the ones written), the number of competing cells, and the model settings used. The settings match the sliders on the HTML map:

| Option | Default | Meaning |
|---|---|---|
| `--path-loss` | 3.5 | how fast signal falls with distance: about 2 in open country, 3–4 in towns |
| `--sigma-db` | 8 | random signal variation from buildings and terrain |
| `--acc-weight` | 0 | how much each cell's position is blurred by its accuracy |
| `--range-km` | 4× typical spacing | distance at which signal fades out |
| `--all-frequencies` | off | compete with cells on every frequency, not just the cell's own EARFCN |
| `--directional` | off | experimental: sectors get a wedge-shaped antenna pattern pointing the way their position is offset from their site's centre (a rough estimate) |
| `--use-unknown-12` | off | experimental: fade out at the cell's own reach from Apple's field 12 instead of the shared range; see [examples](examples/README.md#comparison-fading-out-at-the-cells-own-reach-field-12) |
| `--hex-res` | 10, coarser for large rural areas | H3 resolution of the hexagons (10: ~0.015 km², 11: ~0.002 km²); chosen automatically so a map has at most ~20,000 |
| `--contours` | off | write smooth likelihood contour bands instead of hexagons |
| `--contour-levels` | 0.1,0.25,0.5,0.75,0.9 | where the bands start; the last band runs up to 100% |
| `--contour-step-m` | 20 | spacing of the grid the contours are traced on |

With `--contours`, the coverage is written as contour bands (`role: "contour"`) instead of hexagons: one MultiPolygon per band between two levels, with `likelihood_min`, `likelihood_max`, a `label` such as `"50–75%"`, and the same `fill` / `fill-opacity` styling. Bands don't overlap: each is a ring around the next, so the area where the cell is the most likely server is the sum of the bands from 50% up. The files are smaller than with hexagons and look like the HTML map's heat, except that the bands start at the lowest level (10% by default) where the map fades down to 2%; add a lower level such as `0.02` to show that faint fringe.

### HTTP API

`GET /cells/{mcc}/{mnc}/{tac}/{cell}/heatmap` returns a cell's coverage map: the same lookup as `heatmap` (database first; a cell that isn't crawled yet is fetched from Apple and saved with every other cell in the replies), as HTML, or as GeoJSON with `?format=geojson`. A cell Apple has no location for gives `404`.

Run it with the database in Docker Compose; it listens on all network interfaces, on `WTFPS_API_PORT` (8090 by default, set in `.env`), with no authentication:

```sh
docker compose up -d --build api
curl -o map.html http://localhost:8090/cells/284/5/106/451645/heatmap
```

Or without Docker: `wtfps serve` (default `--listen 0.0.0.0:8090`).

### `migrate`

`explore` applies pending database migrations when it starts. To update the schema without crawling, run `wtfps migrate`.

#### Running the database with Docker Compose

You don't need a local Postgres or the SQLx CLI to run the crawler; wtfps creates and updates its tables itself on startup. `docker-compose.yml` runs the `postgis/postgis` image, which provides Postgres with PostGIS, and `psql` runs inside the container. The data lives in the volume `postgis_data`, which Compose treats as external: it uses it but never creates or deletes it, so the crawl survives `docker compose down` (even with `-v`), restarts and image updates. Create it once with `docker volume create postgis_data`. The port is bound to localhost only. `.env.example` lists the settings: copied to `.env`, both `docker compose` and wtfps read it, and `DATABASE_URL` is built from `POSTGRES_USER`, `POSTGRES_PASSWORD`, `WTFPS_DB_PORT` and `POSTGRES_DB`, so changing one keeps the two in step. The password only takes effect when the volume is first created.

```sh
# 1. start the database (waits until it accepts connections)
cp .env.example .env      # settings for both wtfps and docker compose
docker volume create postgis_data     # once; does nothing if it already exists
docker compose up -d --wait

# 2. build; SQLX_OFFLINE uses the saved query metadata in .sqlx/ instead of a live database
SQLX_OFFLINE=true cargo build --release

# 3. create the tables
./target/release/wtfps migrate

# 4. seed a cell Apple knows and start crawling
docker compose exec db psql -U postgres -d wtfps \
  -c "insert into cell_todo values (284, 5, 106, 451645)"
./target/release/wtfps explore cell
```

The first run prints `extension "postgis" already exists, skipping`. That is a harmless notice: the image already has PostGIS enabled. To hide it, run with `RUST_LOG=info,sqlx::postgres::notice=warn`.

The seed must be a cell Apple currently has a location for; check it first with `wtfps cell`. You can find the cell your phone is using with an app such as NetMonitor on Android, or Field Test mode (`*3001#12345#*`) on iPhone. The TAC must be correct: with a wrong TAC Apple reports the cell as not found.

Watch progress from a second terminal, or open an interactive prompt with `docker compose exec db psql -U postgres -d wtfps`:

```sh
docker compose exec db psql -U postgres -d wtfps \
  -c "select count(*) cells from cell" \
  -c "select count(*) filter (where found) hits, count(*) probes from cell_probe" \
  -c "select count(*) waiting_areas from cell_crawl where last_crawled is null"
```

#### Reading cell coordinates

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

#### Stopping the crawler

Press Ctrl+C, or from another terminal run `pkill -INT -f 'wtfps explore'`. Each batch is saved in its own transaction: finished batches are kept, and an interrupted batch is simply redone on the next run. Run `wtfps explore cell` again to resume.

The crawler also stops by itself once there are no seeds, uncrawled areas or unprobed sites left. With probing that can take a very long time, so to cap a run:

```sh
# stop once the queued areas are done
./target/release/wtfps explore cell --no-probe

# stop after 10 minutes
timeout 10m ./target/release/wtfps explore cell
```

`docker compose stop` (or `down`, even `down -v`) stops the database and keeps the data: the volume is external, so Compose never deletes it. Only `docker volume rm postgis_data` would.

#### Switching from a `docker run` container to Compose

The Compose volume is named exactly `postgis_data`, so a database started with `docker run ... -v postgis_data:/var/lib/postgresql/data` is reused as is. Stop the crawler and the old container first: two servers must never use the same data directory at once.

```sh
docker stop wtfps-test            # with --rm this removes the container, not the named volume
docker compose up -d --wait
docker compose exec db psql -U postgres -d wtfps -c "select count(*) from cell"
./target/release/wtfps migrate
```

To check how a container was started, run `docker inspect wtfps-test --format '{{range .Mounts}}{{.Type}} {{.Name}} -> {{.Destination}}{{"\n"}}{{end}}'`. If it shows a `volume` with a long random name instead of `postgis_data`, the data is in an anonymous volume, which `--rm` deletes when the container stops. Copy it across before stopping the container:

```sh
docker exec wtfps-test pg_dump -U postgres -Fc wtfps > wtfps.dump
docker stop wtfps-test
docker compose up -d --wait
docker compose exec -T db pg_restore -U postgres -d wtfps --clean --if-exists < wtfps.dump
docker compose exec db psql -U postgres -d wtfps -c "select count(*) from cell"
```

Keep `wtfps.dump` until you've checked the restored data.

## Development

wtfps is a Nix flake, meaning you can easily pull required dependencies into your environment using `nix dev` and can also compile the project using `nix build`.

If you don't have Nix setup on your system, you will need to install the following:

- Rust
- PostgreSQL v16 <!-- - [PostGIS](https://postgis.net/) -->
- Protobuf: required to serialize requests for Apple's servers
- [SQLx CLI](https://github.com/launchbadge/sqlx/tree/main/sqlx-cli#install): required to configure the database; install with `cargo install sqlx-cli --no-default-features --features rustls,postgres` (not needed for the Docker route in [Running the database with Docker Compose](#running-the-database-with-docker-compose))

SQLx attempts to validate queries using a live database when compiling the project. You can tell SQLx how to connect to your database by setting the `DATABASE_URL` environment variable, which you can add to `.env`. [DATABASE_URL docs](https://docs.rs/sqlx/latest/sqlx/postgres/struct.PgConnectOptions.html)

```sh
# unix socket
DATABASE_URL=postgresql:///wtfps

# tcp
DATABASE_URL=postgresql://user:password@localhost/wtfps
```

You should now be able to create the database, add your seeds to the `todo` table and start scraping:

```sh
# reset command will drop, create and apply migrations in one go
$ sqlx database reset
Drop database at postgres:///wtfps2 (y/n): y
Applied 20241008024704/migrate init (430ms)

$ psql wtfps
wtfps=# insert into todo values ('aa:bb:cc:dd:ee:ff');
INSERT 0 1
wtfps=# \q

# query macs in the todo table
$ cargo run --release explore todo
# continually crawl new macs
$ cargo run --release explore crawl
```
