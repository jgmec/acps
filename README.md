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
[src/main.rs:55:17] wifis.len() = 400
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
[src/main.rs:196:17] cells.len() = 121
cellid 451645		horizontal_accuracy Some(1414)		location https://www.openstreetmap.org/search?query=42.6671791%2F23.277446740000002#map=16/42.6671791/23.277446740000002
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

#### Testing the cell crawler with Docker

You don't need a local Postgres or the SQLx CLI to try the crawler; wtfps creates and updates its tables itself on startup. The `postgis/postgis` image provides Postgres with PostGIS, and `psql` runs inside the container.

```sh
# 1. start a throwaway database (--rm: stopping the container deletes it and all data)
docker run -d --rm --name wtfps-test -e POSTGRES_PASSWORD=test -e POSTGRES_DB=wtfps \
  -p 55432:5432 postgis/postgis:16-3.4
export DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/wtfps

# 2. build; SQLX_OFFLINE uses the saved query metadata in .sqlx/ instead of a live database
SQLX_OFFLINE=true cargo build --release

# 3. create the tables (exits straight away because nothing is queued yet)
./target/release/wtfps explore cell

# 4. seed a cell Apple knows and start crawling
docker exec wtfps-test psql -U postgres -d wtfps \
  -c "insert into cell_todo values (284, 5, 106, 451645)"
./target/release/wtfps explore cell
```

The first run prints `extension "postgis" already exists, skipping`. That is a harmless notice: the image already has PostGIS enabled. To hide it, run with `RUST_LOG=info,sqlx::postgres::notice=warn`.

The seed must be a cell Apple currently has a location for; check it first with `wtfps cell`. You can find the cell your phone is using with an app such as NetMonitor on Android, or Field Test mode (`*3001#12345#*`) on iPhone. The TAC must be correct: with a wrong TAC Apple reports the cell as not found.

Watch progress from a second terminal, or open an interactive prompt with `docker exec -it wtfps-test psql -U postgres -d wtfps`:

```sh
docker exec wtfps-test psql -U postgres -d wtfps \
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
docker exec wtfps-test psql -U postgres -d wtfps -c "\copy (select mcc, mnc, tac_id, cell_id, ST_Y(geom::geometry) lat, ST_X(geom::geometry) lon, acc from cell) to stdout csv header" > cells.csv
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

When you're done, export anything you want to keep, then remove the test database with `docker stop wtfps-test`.

## Development

wtfps is a Nix flake, meaning you can easily pull required dependencies into your environment using `nix dev` and can also compile the project using `nix build`.

If you don't have Nix setup on your system, you will need to install the following:

- Rust
- PostgreSQL v16 <!-- - [PostGIS](https://postgis.net/) -->
- Protobuf: required to serialize requests for Apple's servers
- [SQLx CLI](https://github.com/launchbadge/sqlx/tree/main/sqlx-cli#install): required to configure the database; install with `cargo install sqlx-cli --no-default-features --features rustls,postgres` (not needed for the Docker route in [Testing the cell crawler with Docker](#testing-the-cell-crawler-with-docker))

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
