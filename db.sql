create table if not exists wifi (
    mac integer not null primary key,
    cell integer not null,
    lat real not null,
    lon real not null
);

create index if not exists wifi_cell on wifi (cell);

create table if not exists wifi_explore (
    -- used as primary key to provide a somewhat random order
    mac integer not null primary key,
    cell integer not null unique
);
