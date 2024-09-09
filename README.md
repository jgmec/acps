# wtfps

Extracts a _stupid amount_ of data from Apple's WiFi Positioning System. Built to gather large amounts of information to use for research and benchmarking.

## Resources

- [acheong08/apple-corelocation-experiments](https://github.com/acheong08/apple-corelocation-experiments) - reverse engineering, API client implementation in Go
- [Surveilling the Masses with Wi-Fi-Based Positioning Systems](https://www.cs.umd.edu/~dml/papers/wifi-surveillance-sp24.pdf) - research paper
- [ReverseApple/appleloc](https://github.com/ReverseApple/appleloc) - API client implementation in Rust

## Design

- Apple's DB will return up to 400 nearby MACs when queried. wtfps takes advantage of this to explore further by continually querying new MACs
- uses h3 cells with resolution 10 to keep track of areas that have been crawled
  - significantly increases crawling speed by deduplicating requests that would return the same information
  - wtfps tries to crawl MACs that are nearby to reduce unnecessary database writes as requests will return overlapping data
- database is split into 16 partitions by location
  - identified using [H3 cells](https://www.uber.com/en-AU/blog/h3/) with resolution 3
    - 12 393 km^2 on average per part, 41.2k unique parts globally - [source](https://h3geo.org/docs/core-library/restable/#average-area-in-m2)
  - hash based partitioning
  - MAC uniqueness is only enforced per cell
  - could allow for concurrency when crawling?

## Usage

You can query the location of a single MAC using `wtfps query`:

```
$ wtfps query 90:CD:B6:11:50:D9
https://www.openstreetmap.org/search?query=53.220985410000004%2F-4.20290374#map=12/53.220985410000004/-4.20290374
[src/main.rs:55:17] wifis.len() = 400
```

In order to explore Apple's DB, you'll need to setup Postgres. wtfps requires some initial MACs to seed crawling:

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
