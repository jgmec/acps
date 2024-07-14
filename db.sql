create table if not exists wifi (
    mac blob not null primary key,
    lat real not null,
    lon real not null,
    acc integer not null,
    alt integer not null,
    alt_acc integer not null
);

create table if not exists wifi_explore (
    cell integer not null primary key,
    mac blob not null
);
