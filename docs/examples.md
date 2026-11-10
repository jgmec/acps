# Examples

Every command and URL wtfps supports, with cells that are known to work. The [README](../README.md) explains what each one does, and [crawl.md](crawl.md) how to collect data step by step; this page is for copying.

Commands that use the database read `DATABASE_URL` from the environment or from `.env` in the current directory. Run them from the project folder, with `./target/release/wtfps` (shown as `wtfps` below) or `cargo run --release --`.

## Cells used in these examples

| Radio | mcc / mnc / area / cell | Where |
|---|---|---|
| LTE | 284 / 5 / 106 / 451645 | Sofia, Bulgaria (site 1764, sector 61) |
| LTE | 452 / 4 / 38211 / 209991683 | Kon Tum, Vietnam |
| LTE | 452 / 1 / 36003 / 30883339 | Da Nang, Vietnam (coast) |
| LTE | 282 / 1 / 26201 / 3945042 | rural Georgia (large range) |
| 5G NR | 232 / 1 / 17008 / 17248763906 | Vienna, Austria |
| 5G NR | 232 / 5 / 10143 / 1837252770 | Vienna, band n78 (3599 MHz) |
| 2G GSM | 284 / 1 / 1400 / 9935 | Sofia (the area code is a LAC) |
| 3G UMTS | 609 / 10 / 631 / ./ | Mauritania (RNC 23 × 65536 + CID 56152) |

The area number is a TAC for LTE and 5G, and a LAC for GSM and UMTS. It must be exact: with a wrong one, Apple reports the cell as not found.

## Setup

```sh
cp .env.example .env                    # settings for wtfps and docker compose
docker volume create postgis_data       # once; does nothing if it already exists
docker compose up -d --wait db          # PostGIS on 127.0.0.1:55432
cargo build --release
wtfps migrate                           # create or update the tables
docker compose up -d --build api        # the HTTP API on port 8090
```

Connect with psql (or DBeaver: host 127.0.0.1, port 55432, database wtfps, user postgres, password from `.env`):

```sh
docker compose exec db psql -U postgres -d wtfps
```

## Looking up one cell (`cell`, no database)

```sh
# LTE: the cell, ~100 nearby cells of all operators and 20 tracking-area estimates
wtfps cell 284 5 106 451645

# every field of Apple's raw reply, including undocumented ones
wtfps cell 284 5 106 451645 --raw

# 5G NR (36-bit cell ID; without --nr an ID over 32 bits is refused)
wtfps cell --nr 232 1 17008 17248763906
wtfps cell --nr --raw 232 1 17008 17248763906

# 2G GSM: LAC and cell ID; Apple returns only this cell
wtfps cell --gsm 284 1 1400 9935
wtfps cell --gsm --raw 284 1 1400 9935

# 3G UMTS: LAC and the full 28-bit cell ID = RNC × 65536 + CID
wtfps cell --gsm 609 10 631 $((23 * 65536 + 56152))     # = 1563480

# Apple's server for China (--china goes before the command)
wtfps --china cell 460 0 <tac> <cell_id>
```

## WiFi (`query`, `explore todo`, `explore crawl`)

```sh
# location of one access point, and the ~400 networks Apple returns with it
wtfps query 90:CD:B6:11:50:D9

# every MAC address of a vendor prefix (OUI), as crawl seeds
wtfps oui 90cdb6 > seeds.txt
```

```sql
-- seed the WiFi queue
insert into todo (mac) values ('AA:BB:CC:DD:EE:FF');
\copy todo (mac) from 'seeds.txt'
```

```sh
wtfps explore todo                      # work through the seeded MACs
wtfps explore todo --ignore-extra       # only the seeds, not the networks returned with them
wtfps explore crawl                     # crawl outward by area
```

## Crawling cells (`explore cell`)

Seed with cells Apple knows (check them with `wtfps cell` first). `radio` is `lte` (the default), `nr` or `gsm`:

```sql
insert into cell_todo (mcc, mnc, tac_id, cell_id) values (284, 5, 106, 451645);                      -- LTE
insert into cell_todo (mcc, mnc, tac_id, cell_id, radio) values (232, 1, 17008, 17248763906, 'nr');  -- 5G
insert into cell_todo (mcc, mnc, tac_id, cell_id, radio) values (284, 1, 1400, 9935, 'gsm');         -- GSM / UMTS
```

The same from the shell, with the Compose database:

```sh
docker compose exec db psql -U postgres -d wtfps -c "insert into cell_todo values (284, 5, 106, 451645)"
```

```sh
wtfps explore cell                      # seeds, then the area crawl, then probing and GSM block scans
wtfps explore cell --no-probe           # no guessing: seeds and the area crawl only
```

Stop it with Ctrl+C; it continues where it left off. Progress:

```sql
select radio, count(*) from cell group by radio;                        -- cells per radio
select count(*) from cell_todo;                                         -- seeds waiting
select radio, count(*) from cell_crawl where last_crawled is null group by radio;   -- areas waiting
select radio, count(*) from cell_not_found group by radio;              -- cells Apple doesn't know
select count(*) filter (where probed_at is null), count(*) filter (where siblings_probed_at is null)
from cell_site;                                                         -- LTE sites waiting to be probed
select count(*), count(*) filter (where found) from cell_probe;         -- LTE guesses tried / found
select count(*), sum(found) from cell_gsm_block;                        -- GSM blocks scanned / cells in them
```

Where a cell is, and why it has no antenna direction:

```sql
select mcc, mnc, tac_id, cell_id, radio, ST_Y(geom::geometry) lat, ST_X(geom::geometry) lon, acc
from cell where mcc = 284 and mnc = 5 and cell_id = 451645;

-- the other known sectors of its site and how far each is from the site's centre
with s as (select mcc, mnc, cell_id >> 8 as site from cell
           where mcc = 284 and mnc = 5 and cell_id = 451645 and radio = 'lte' limit 1),
c as (select c.cell_id & 255 as sector, c.pci, c.earfcn, c.acc, c.geom
      from cell c, s where c.mcc = s.mcc and c.mnc = s.mnc and c.cell_id >> 8 = s.site and c.radio = 'lte')
select sector, pci, earfcn, acc,
       round(ST_Distance(geom, (select ST_Centroid(ST_Collect(geom::geometry))::geography from c))) as m_from_centre
from c order by sector;
```

## Coverage maps (`heatmap`)

Writes `cell-<mcc>-<mnc>-<tac>-<cell>.html` (or `.geojson`). A cell that isn't in the database is fetched from Apple and saved first.

```sh
# HTML map, default settings (Okumura–Hata, own frequency only)
wtfps heatmap 284 5 106 451645
wtfps heatmap 452 4 38211 209991683
wtfps heatmap 282 1 26201 3945042

# GeoJSON instead (opens in geojson.io or QGIS)
wtfps heatmap --geojson 284 5 106 451645
wtfps heatmap --geojson --out sofia.geojson 284 5 106 451645

# smooth contour bands instead of hexagons
wtfps heatmap --geojson --contours 284 5 106 451645
wtfps heatmap --geojson --contours --contour-levels 0.02,0.1,0.5,0.9 --contour-step-m 10 284 5 106 451645

# model choices
wtfps heatmap --propagation simple 284 5 106 451645     # distance only, range 4× cell spacing
wtfps heatmap --all-frequencies 284 5 106 451645        # compete with every band
wtfps heatmap --directional 284 5 106 451645            # wedge-shaped sectors (LTE only)
wtfps heatmap --terrain 452 4 38211 209991683           # hills and ridges (downloads elevation tiles)
wtfps heatmap --use-unknown-12 284 5 106 451645         # fade out at Apple's field 12
wtfps heatmap --range-km 5 --path-loss 3.2 --sigma-db 6 --acc-weight 0.5 284 5 106 451645
wtfps heatmap --hex-res 11 --geojson 284 5 106 451645   # finer hexagons
wtfps heatmap --radius-km 20 284 5 106 451645           # fewer nearby cells

# where the cells come from
wtfps heatmap --online 284 5 106 451645                 # Apple only, no database, nothing saved
wtfps heatmap --no-fetch 284 5 106 451645               # database only, never ask Apple

# 5G NR (file cell-nr-…)
wtfps heatmap --nr 232 5 10143 1837252770
wtfps heatmap --nr --geojson 232 1 17008 17248763906

# 2G GSM / 3G UMTS (file cell-gsm-…)
wtfps heatmap --gsm 284 1 1400 9935
wtfps heatmap --gsm --online --geojson 284 1 1400 9935
```

## HTTP API (`serve`)

```sh
wtfps serve                             # 0.0.0.0:8090
wtfps serve --listen 127.0.0.1:8099
docker compose up -d --build api        # the same in Docker, port WTFPS_API_PORT (8090)
```

Open in a browser (on the map, clicking another cell opens its map):

```
http://localhost:8090/cells/284/5/106/451645/heatmap
http://localhost:8090/cells/284/5/106/451645/heatmap?propagation=simple
http://localhost:8090/cells/452/4/38211/209991683/heatmap?terrain=true
http://localhost:8090/nr/232/5/10143/1837252770/heatmap
http://localhost:8090/gsm/284/1/1400/9935/heatmap
http://localhost:8090/gsm/609/10/631/1563480/heatmap
```

| Parameter | Values | Default |
|---|---|---|
| `format` | `html`, `geojson` | `html` |
| `propagation` | `hata`, `simple` (any case) | `hata` |
| `terrain` | `true`, `false`, `1`, `0` (any case) | `false` |

With curl:

```sh
# HTML map
curl -o map.html http://localhost:8090/cells/284/5/106/451645/heatmap

# GeoJSON, with options
curl -o map.geojson 'http://localhost:8090/cells/284/5/106/451645/heatmap?format=geojson'
curl -o map.geojson 'http://localhost:8090/cells/284/5/106/451645/heatmap?format=geojson&propagation=simple&terrain=true'

# 5G and GSM
curl -o nr.html  http://localhost:8090/nr/232/5/10143/1837252770/heatmap
curl -o gsm.geojson 'http://localhost:8090/gsm/284/1/1400/9935/heatmap?format=geojson'

# where the map came from: database, fetched (from Apple and saved) or online (not saved)
curl -s -o /dev/null -D - http://localhost:8090/cells/284/5/106/451645/heatmap | grep -i x-wtfps-source

# just the status code
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:8090/nr/232/1/17008/17248763905/heatmap
```

Responses:

| Request | Status |
|---|---|
| `/cells/284/5/106/451645/heatmap` | `200` |
| `/nr/232/1/17008/17248763905/heatmap` (a cell Apple doesn't know) | `404`, `Apple has no location for cell …` |
| `/cells/284/5/106/451645/heatmap?format=pdf` | `400`, `unknown format "pdf": use html or geojson` |
| `/cells/284/5/106/451645/heatmap?propagation=cost231` | `400`, `unknown propagation …` |
| `/cells/284/5/106/451645/heatmap?terrain=yes` | `400`, `unknown terrain …` |
| `/cells/284/5/x/451645/heatmap` | `400`, the path segment isn't a number |
| Apple unreachable, or a failing database query | `500` |

The full specification is [openapi.yaml](openapi.yaml).

## Settings (`.env` or environment)

```sh
DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/wtfps   # database
WTFPS_DB_PORT=55432                                         # host port of the Compose database
WTFPS_API_PORT=8090                                         # host port of the Compose API
WTFPS_DEM_DIR=dem                                           # elevation tiles for terrain
WTFPS_APPLE_URL=https://gs-loc.apple.com/clls/wloc          # Apple's service, e.g. through a proxy
WTFPS_APPLE_CHINA_URL=https://gs-loc-cn.apple.com/clls/wloc # with --china
WTFPS_DEM_URL=https://copernicus-dem-30m.s3.amazonaws.com   # elevation tile mirror
RUST_LOG=info,sqlx::postgres::notice=warn                   # quieter logs
```

A value exported in the shell overrides `.env`. To use one only for a single command:

```sh
WTFPS_APPLE_URL=http://proxy:3128/clls/wloc wtfps explore cell
DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/wtfps wtfps heatmap 284 5 106 451645
```

## Development

```sh
cargo test                              # unit tests, no database or network needed
cargo test -- --ignored                 # also reads real elevation tiles from dem/
cargo clippy --all-targets
SQLX_OFFLINE=true cargo build --release
cargo sqlx prepare                      # after changing a query! macro (needs the database)
```
