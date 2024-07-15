create table wifi (
    mac binary(6) not null primary key,
    lat real not null,
    lon real not null,
    acc integer not null,
    alt integer not null,
    alt_acc integer not null
);

create table wifi_explore (
    cell integer not null primary key,
    mac binary(6) not null
);
