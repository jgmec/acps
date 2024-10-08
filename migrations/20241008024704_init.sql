create extension postgis;

create table wifi (
    mac macaddr primary key not null,
    geom geography(Point) not null,
    check_area bigint not null,
    crawl_area bigint not null
);

create index wifi_geom on wifi using gist (geom);
create index wifi_check_area on wifi (check_area);
create index wifi_crawl_area on wifi (crawl_area);

create table crawl (
    area bigint not null primary key,
    last_crawled timestamp with time zone
);

create index crawl_todo on crawl (area) where last_crawled is null;

create table todo (
    mac macaddr primary key not null
);
