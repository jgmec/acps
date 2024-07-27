create table if not exists wifi (
    h3c integer not null,
    mac integer not null,
    lat real not null,
    lon real not null,
    primary key (h3c, mac)
);

create table if not exists wifi_explore (
    mac integer not null primary key,
    cell integer not null unique
);

create table if not exists wifi_brute (
    mac integer not null primary key
);

create table if not exists wifi_fail (
    mac integer not null primary key
);
