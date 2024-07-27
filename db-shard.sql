create table if not exists wifi (
    mac integer not null primary key,
    lat real not null,
    lon real not null
);
