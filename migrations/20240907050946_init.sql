create table wifi_seen (
    mac macaddr primary key not null,
    cell bigint not null,
    lat double precision not null,
    lon double precision not null,
    gone boolean not null default false
);
create index wifi_seen_cell on wifi_seen (cell);
create index wifi_seen_gone on wifi_seen (mac) where gone;

create table wifi_todo (
    mac macaddr primary key not null
);

create table wifi_cell (
    cell bigint primary key not null,
    last_crawled timestamp with time zone
);

create index wifi_cell_last_crawled on wifi_cell (last_crawled);
create index wifi_cell_todo on wifi_cell (cell) where last_crawled is null;
