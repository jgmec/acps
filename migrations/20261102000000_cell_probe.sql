-- known LTE sites (cell_id >> 8), probed when nearby site numbers have been guessed
create table cell_site (
    mcc integer not null,
    mnc integer not null,
    site bigint not null,
    probed_at timestamp with time zone,
    primary key (mcc, mnc, site)
);

create index cell_site_todo on cell_site (mcc, mnc, site) where probed_at is null;

insert into cell_site (mcc, mnc, site)
select distinct mcc, mnc, cell_id >> 8 from cell;

-- finding a site's cells by id range
create index cell_operator_id on cell (mcc, mnc, cell_id);

-- guessed cells already tried, so they are never queried twice
create table cell_probe (
    mcc integer not null,
    mnc integer not null,
    tac_id integer not null,
    cell_id bigint not null,
    found boolean not null,
    probed_at timestamp with time zone not null default current_timestamp,
    primary key (mcc, mnc, tac_id, cell_id)
);
