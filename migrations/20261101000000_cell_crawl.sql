-- horizontal accuracy in metres as reported by Apple, and the H3 area used for crawling
alter table cell add column acc bigint;
alter table cell add column crawl_area bigint;

create index cell_crawl_area on cell (mcc, mnc, crawl_area);
create index cell_missing_area on cell (mcc) where crawl_area is null;

-- areas queued for cell exploration, one per operator
create table cell_crawl (
    mcc integer not null,
    mnc integer not null,
    area bigint not null,
    last_crawled timestamp with time zone,
    primary key (mcc, mnc, area)
);

create index cell_crawl_todo on cell_crawl (mcc, mnc, area) where last_crawled is null;

-- cells Apple returned without a location
create table cell_not_found (
    mcc integer not null,
    mnc integer not null,
    tac_id integer not null,
    cell_id bigint not null,
    checked_at timestamp with time zone not null default current_timestamp,
    primary key (mcc, mnc, tac_id, cell_id)
);
