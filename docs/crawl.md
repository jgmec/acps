# Crawling data

How to collect cells and WiFi networks from Apple's location service into the database: where to get seeds, how each crawl spreads, how to watch and steer it, and how to get the data out. For single commands to copy, see [examples.md](examples.md); for what each table holds, the [README](../README.md).

## The crawls at a glance

| Crawl | Command | Seeds | How it spreads | Stops when |
|---|---|---|---|---|
| LTE cells | `wtfps explore cell` | `cell_todo`, radio `lte` | each reply brings ~100 nearby cells; one cell per uncrawled 5 km² area is asked next; then guesses of nearby site numbers and missing sectors (probing) | no seeds, areas or unprobed sites left |
| 5G NR cells | the same run | `cell_todo`, radio `nr` | like LTE, by area; no probing | no seeds or areas left |
| 2G GSM / 3G UMTS cells | the same run | `cell_todo`, radio `gsm` | replies contain only the asked cells, so blocks of 500 cell IDs around known cells are asked | no seeds or unscanned blocks left |
| WiFi networks | `wtfps explore todo`, then `wtfps explore crawl` | `todo` (MAC addresses) | each reply brings up to ~400 nearby networks; one network per uncrawled ~0.002 km² area is asked next | no areas left |

One `explore cell` run handles all three cell types, interleaved. Run only one crawler of each kind at a time against a database.

## 1. Before you start

```sh
docker compose up -d --wait db          # the database (see the README for the first setup)
./target/release/wtfps migrate          # bring the tables up to date
```

Every finished batch is saved in its own transaction, so a crawler can be stopped at any moment with Ctrl+C (or `pkill -INT -f 'wtfps explore'`) and resumed by running it again. To let it run after you close the terminal:

```sh
nohup ./target/release/wtfps explore cell > crawl.log 2>&1 &
tail -f crawl.log

# or inside Docker, next to the database
docker compose run --rm -d --name wtfps-crawler api explore cell
docker logs -f wtfps-crawler
```

To cap a run: `timeout 2h ./target/release/wtfps explore cell`, or `--no-probe` to stop once the queued areas are done.

## 2. Finding seeds

A seed must be a cell Apple has a location for, with the exact area code. The quickest source is the phone in your pocket:

- **Android**: an app such as *Network Cell Info Lite* or *NetMonitor* shows the serving cell. Set the network mode to "2G only" or "LTE only" for a moment to see other technologies, then switch back.
- **iPhone**: Field Test mode, dial `*3001#12345#*`.

What to read off, per technology:

| Radio | Numbers you need | Notes |
|---|---|---|
| LTE | MCC, MNC, **TAC**, cell ID (ECI) | the cell ID is eNB × 256 + sector; an app showing eNB 1764 and sector 61 means cell 451645 |
| 5G NR | MCC, MNC, **TAC**, NCI | NCI up to 36 bits, e.g. 17248763906; only with standalone 5G (with non-standalone 5G the phone reports the LTE cell, which works too) |
| GSM | MCC, MNC, **LAC**, CID | CID up to 65535 |
| UMTS (3G) | MCC, MNC, **LAC**, RNC, CID | the cell ID is RNC × 65536 + CID; the CID alone isn't found. Apple seems to have dropped 3G where it's switched off |

Check each seed before inserting it; "not found" usually means a wrong area code:

```sh
./target/release/wtfps cell 284 5 106 451645              # LTE
./target/release/wtfps cell --nr 232 1 17008 17248763906  # 5G
./target/release/wtfps cell --gsm 284 1 1400 9935         # GSM
./target/release/wtfps cell --gsm 609 10 631 1563480      # UMTS: 23 × 65536 + 56152
```

An LTE or 5G reply lists ~100 nearby cells of all operators, so one good cell is enough to seed every operator in a town: the crawler saves them all.

## 3. Crawling LTE cells

```sql
insert into cell_todo (mcc, mnc, tac_id, cell_id) values (284, 5, 106, 451645);
```

```sh
./target/release/wtfps explore cell
```

What happens, with log lines from a real run:

1. **Seeds** are asked first. Every cell in the replies is saved, and its 5 km² area (H3 resolution 7, per operator) is queued in `cell_crawl`.
2. **Area crawl**: one random known cell from each uncrawled area is asked, 256 per batch, 8 at a time:
   ```
   making 256 lte cell requests...
   3900 lte cells with location, 0 not found, 268 areas crawled
   ```
   A batch takes about 13 seconds and usually brings 2,500–4,000 cells, most of them already known.
3. **Probing**, once no areas are left: Apple returns the same fixed group of ~100 cells for every cell in it, so the crawl can't leave a group by itself. wtfps guesses cells at neighbouring site numbers (about 1 in 13 exists) and each site's missing sector numbers (about 1 in 20). Every hit brings a new group, whose areas are crawled again:
   ```
   probing 53 guessed cells near 3 sites (33 of them missing sectors of known sites)...
   16 of 53 guesses found
   ```

With probing the crawl can run for days. `--no-probe` skips it.

## 4. Crawling 5G cells

```sql
insert into cell_todo (mcc, mnc, tac_id, cell_id, radio) values (232, 1, 17008, 17248763906, 'nr');
```

5G replies look like LTE ones (~100 nearby 5G cells and tracking-area estimates), so 5G is crawled by area in the same run, with its own areas (an area crawled for LTE is still crawled for 5G):

```
making 1 nr cell requests...
101 nr cells with location, 0 not found, 1 areas crawled
making 15 nr cell requests...
```

There's no 5G probing (how a 5G cell ID splits into site and sector differs per operator), so 5G stays within the groups reached from seeds. To reach more, seed 5G cells from other places.

## 5. Crawling GSM and UMTS cells

```sql
insert into cell_todo (mcc, mnc, tac_id, cell_id, radio) values (284, 1, 1400, 9935, 'gsm');
```

Apple's GSM replies contain only the asked cells, but one request can ask for 500. So for each known GSM cell, the crawler asks for its block of 500 cell IDs and the two next to it, in its LAC:

```
asking for 1 seeded GSM cells...
scanning 3 blocks of 500 GSM cell IDs...
25 GSM cells with location, 0 not found, 3 blocks scanned
```

Scanned blocks are recorded in `cell_gsm_block` and never asked again. The scan spreads while it finds cells and stops at empty blocks; cells far away in the numbering, or in other LACs, need their own seeds. `--no-probe` turns the block scan off, leaving only seeds.

## 6. Crawling WiFi networks

```sql
insert into todo (mac) values ('90:CD:B6:11:50:D9');
```

```sh
./target/release/wtfps query 90:CD:B6:11:50:D9    # check it: location and ~400 networks
./target/release/wtfps explore todo               # ask the seeds; saves them and every network returned
./target/release/wtfps explore crawl              # then one network per uncrawled ~0.002 km² area
```

`explore todo --ignore-extra` saves only the seeded networks, not the ones returned with them. Seeds can also be every MAC of a vendor prefix, though most of them won't exist:

```sh
./target/release/wtfps oui 90cdb6 > seeds.txt
docker compose exec -T db psql -U postgres -d wtfps -c "\copy todo (mac) from stdin" < seeds.txt
```

WiFi requests run 256 at a time, so a WiFi crawl is much heavier on Apple (and a proxy) than the cell crawl.

## 7. Many seeds at once

From a CSV file `seeds.csv` with lines like `284,5,106,451645,lte`:

```sh
docker compose exec -T db psql -U postgres -d wtfps \
  -c "\copy cell_todo (mcc, mnc, tac_id, cell_id, radio) from stdin with csv" < seeds.csv
```

A line that repeats a queued seed (or another line) makes the whole load fail on the table's key, and nothing is loaded; remove repeats first, e.g. with `sort -u seeds.csv`.

## 8. Watching progress

```sql
-- cells per radio, and how many were added in the last hour
select radio, count(*), count(*) filter (where crawled_at > now() - interval '1 hour') last_hour
from cell group by radio;

-- what's waiting
select radio, count(*) from cell_todo group by radio;                                  -- seeds
select radio, count(*) from cell_crawl where last_crawled is null group by radio;       -- areas
select count(*) filter (where probed_at is null) neighbours,
       count(*) filter (where siblings_probed_at is null) siblings from cell_site;       -- LTE probing
select count(*) blocks, sum(found) cells from cell_gsm_block;                           -- GSM blocks

-- results
select radio, count(*) from cell_not_found group by radio;
select count(*), count(*) filter (where found) from cell_probe;                         -- LTE guesses
select mcc, mnc, radio, count(*) from cell group by 1, 2, 3 order by 4 desc;           -- per operator

-- WiFi
select count(*) from todo;
select count(*) filter (where last_crawled is null) waiting, count(*) from crawl;
select count(*) from wifi;
```

The log says the same per batch. A batch of LTE requests that all fail stops the crawler with `all 256 cell requests in batch failed`; failed cells stay queued for the next run.

## 9. Steering the crawl

The area crawl follows wherever replies lead, including neighbouring countries. To keep it to one country, mark the other countries' queued areas as crawled; nothing is removed, and setting `last_crawled` back to `null` queues them again:

```sql
-- crawl only Bulgaria (mcc 284) for now
update cell_crawl set last_crawled = now() where mcc <> 284 and last_crawled is null;

-- later: queue every area outside Bulgaria again (including ones crawled for real)
update cell_crawl set last_crawled = null where mcc <> 284;
```

To crawl a new region, seed a cell there; to refresh an area's cells, queue it again:

```sql
update cell_crawl set last_crawled = null where mcc = 284 and mnc = 5;
```

A known cell keeps the position it was first stored with; later sightings update its channel, PCI, undocumented fields and `crawled_at`, not its position. Apple's own estimates do drift (cell 451645 moved by about 25 m, and its accuracy changed from 1414 to 5848 m, between two days).

## 10. Getting the data out

```sh
# CSV of all cells
docker compose exec -T db psql -U postgres -d wtfps -c "\copy (
  select radio, mcc, mnc, tac_id, cell_id, earfcn, pci, acc,
         ST_Y(geom::geometry) lat, ST_X(geom::geometry) lon, crawled_at
  from cell order by radio, mcc, mnc, tac_id, cell_id) to stdout with csv header" > cells.csv

# GeoJSON of one operator's cells, for geojson.io or QGIS
docker compose exec -T db psql -U postgres -d wtfps -At -c "
  select json_build_object('type', 'FeatureCollection', 'features', json_agg(json_build_object(
    'type', 'Feature', 'geometry', ST_AsGeoJSON(geom)::json,
    'properties', json_build_object('radio', radio, 'tac', tac_id, 'cell', cell_id, 'earfcn', earfcn,
                                    'pci', pci, 'acc', acc))))
  from cell where mcc = 284 and mnc = 5" > cells.geojson
```

Coverage maps of single cells come from `wtfps heatmap` or the HTTP API; see [examples.md](examples.md).

## 11. Troubleshooting

| Symptom | Cause and fix |
|---|---|
| a seed is recorded in `cell_not_found` | Apple doesn't know it, or the TAC/LAC is wrong; check with `wtfps cell` (with `--nr` or `--gsm` for those) |
| `WARN wtfps::fetch] TimedOut` on every request, while `wtfps cell` works | something between wtfps and Apple limits the crawler's load, e.g. a proxy in `WTFPS_APPLE_URL` (a Squid setting was the cause once); a variable exported in the shell overrides `.env` |
| `pool timed out while waiting for an open connection` | the database isn't reachable: check `DATABASE_URL` and `docker compose ps` |
| `migration … was previously applied but is missing` | an older wtfps build against a newer database: rebuild (`cargo build --release`, `docker compose up -d --build api`) |
| the LTE crawl finds nothing new for a long time | it's probing; most guesses miss. `--no-probe` ends the run instead |
| a GSM scan stops after a few blocks | the LAC's known cells all sit in one range of IDs; seed a cell from another range or LAC |
