create table wifi (
    mac macaddr primary key not null,

    cell bigint,

    lat double precision,
    lon double precision,
    acc bigint,
    alt bigint,
    alt_acc bigint,

    first_seen timestamp with time zone,
    last_seen timestamp with time zone,
    last_crawled timestamp with time zone
);

create index wifi_cell on wifi (cell);
create index wifi_first_seen on wifi using brin (first_seen);
create index wifi_last_crawled on wifi using brin (last_crawled);
