create table cell_todo (
    mcc integer not null,
    mnc integer not null,
    tac_id integer not null,
    cell_id bigint not null,
    primary key (mcc, mnc, tac_id, cell_id)
);

create table cell (
    mcc integer not null,
    mnc integer not null,
    tac_id integer not null,
    cell_id bigint not null,
    geom geography(Point) not null,
    primary key (mcc, mnc, tac_id, cell_id)
);

create index cell_geom on cell using gist (geom);
