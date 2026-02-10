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

-- cell: areas crawled, exploration queue
select count(*) from crawl where last_crawled is null;

-- wifi: scraped wifi networks
select count(*) from wifi;
```

### `cell`

You can query the location of a single Cell using `wtfps cell`:

```
$ wtfps cell 284 1 1401 8168961
cellid: 8168961
Deserialized: CellTower { mcc: 284, mnc: 1, cell_id: 8168961, tac_id: 1401, location: None, uarfcn: None, pid: None }
https://www.openstreetmap.org/search?query=42.66833496%2F23.28340721#map=16/42.66833496/23.28340721
[src/main.rs:190:17] cells.len() = 121
```

## Development

wtfps is a Nix flake, meaning you can easily pull required dependencies into your environment using `nix dev` and can also compile the project using `nix build`.

If you don't have Nix setup on your system, you will need to install the following:

- Rust
- PostgreSQL v16 <!-- - [PostGIS](https://postgis.net/) -->
- Protobuf: required to serialize requests for Apple's servers
- [SQLx CLI](https://github.com/launchbadge/sqlx/tree/main/sqlx-cli#install): required to configure the database

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
